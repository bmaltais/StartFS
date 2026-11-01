//! Versioned history: an overwritten or deleted file keeps its earlier
//! versions for the filesystem's window, so they can be listed and restored.

use startfs_core::fs::{parse_restore_point, restore_point};
use startfs_core::net::NodeId;
use startfs_core::ops::*;
use startfs_harness::{Cluster, noise};
use std::time::Duration;

const MINUTE: Duration = Duration::from_secs(60);
const HOUR: Duration = Duration::from_secs(3600);

/// A one-hour GC retention and a one-day window, so the window is what holds data.
const CONFIG: &str = "gc_retention_secs = 3600\nkeep_versions_secs = 86400";

fn put(fs: &impl FsOps, name: &str, data: &[u8]) -> Ino {
    let f = fs.create(ROOT_INO, name, 0o644).unwrap();
    assert_eq!(fs.write(f.ino, 0, data).unwrap(), data.len());
    f.ino
}

/// Chunks held as replicas across the whole cluster, as `status` reports them.
fn total_replicas(c: &Cluster) -> u64 {
    (0..c.node_count())
        .map(|i| c.control(i).status().map_or(0, |s| s.usage.chunks))
        .sum()
}

/// Let `by` pass in steps, letting the cluster work after each.
fn pass(c: &Cluster, by: Duration, step: Duration) {
    let mut left = by;
    while !left.is_zero() {
        let s = step.min(left);
        c.advance(s);
        c.sync();
        left -= s;
    }
}

#[test]
fn an_overwritten_files_earlier_data_is_kept_for_the_window_then_reclaimed() {
    let c = Cluster::with_filesystem(2, 3, CONFIG);
    let f = put(&c.fs(0), "a", &noise(8 * 1024, 1));
    pass(&c, HOUR, 10 * MINUTE);
    c.fs(0).write(f, 0, &noise(8 * 1024, 2)).unwrap();
    pass(&c, HOUR, 10 * MINUTE);
    assert_eq!(total_replicas(&c), 4, "both versions, two copies each");

    pass(&c, 20 * HOUR, HOUR);
    assert_eq!(total_replicas(&c), 4, "reclaimed within the window");

    // Replaced at 1 h, so the window ends at 25 h and the retention with it: the longer of
    // the two holds the data, not their sum (26 h).
    pass(&c, 3 * HOUR + 30 * MINUTE, 10 * MINUTE);
    assert_eq!(total_replicas(&c), 2, "kept past the window");
}

#[test]
fn a_deleted_files_data_is_kept_for_the_window_then_reclaimed() {
    let c = Cluster::with_filesystem(2, 3, CONFIG);
    put(&c.fs(0), "a", &noise(8 * 1024, 1));
    pass(&c, HOUR, 10 * MINUTE);
    c.fs(1).unlink(ROOT_INO, "a").unwrap();

    pass(&c, 20 * HOUR, HOUR);
    assert_eq!(total_replicas(&c), 2, "reclaimed within the window");

    // Deleted at 1 h: released at 25 h, not at the sum of window and retention (26 h).
    pass(&c, 4 * HOUR + 30 * MINUTE, 10 * MINUTE);
    assert_eq!(total_replicas(&c), 0, "kept past the window");
}

#[test]
fn a_state_that_lasted_under_half_a_minute_is_not_kept() {
    let c = Cluster::with_filesystem(2, 3, CONFIG);
    let f = put(&c.fs(0), "a", &noise(8 * 1024, 1));
    c.advance(Duration::from_secs(10));
    c.fs(0).write(f, 0, &noise(8 * 1024, 2)).unwrap();
    put(&c.fs(0), "tmp", &noise(8 * 1024, 3));
    c.advance(Duration::from_secs(10));
    c.fs(0).unlink(ROOT_INO, "tmp").unwrap();

    pass(&c, 2 * HOUR, 10 * MINUTE);
    assert_eq!(total_replicas(&c), 2, "only the current version is held");
}

#[test]
fn history_lists_an_overwritten_files_versions_oldest_first() {
    let c = Cluster::with_filesystem(2, 3, CONFIG);
    let f = put(&c.fs(0), "a", &noise(8 * 1024, 1));
    let first = c.now();
    pass(&c, HOUR, 10 * MINUTE);
    c.fs(0).write(f, 0, &noise(9 * 1024, 2)).unwrap();
    let second = c.now();
    pass(&c, HOUR, 10 * MINUTE);

    // Any member answers, since every member derives the same versions.
    let versions = c.control(2).history("/a").unwrap();
    assert_eq!(versions.len(), 2, "{versions:?}");
    let (old, new) = (&versions[0], &versions[1]);
    assert_eq!((old.size, new.size), (8 * 1024, 9 * 1024));
    assert!(old.written >= first && old.written < second, "{versions:?}");
    assert!(new.written >= second, "{versions:?}");
    assert_eq!(old.replaced, Some(new.written));
    assert_eq!(new.replaced, None, "the current version");
    assert!(!old.deleted && !new.deleted);
}

#[test]
fn history_lists_a_deleted_file_inside_a_deleted_directory() {
    let c = Cluster::with_filesystem(2, 3, CONFIG);
    let fs = c.fs(0);
    let dir = fs.mkdir(ROOT_INO, "d", 0o755).unwrap();
    let f = fs.create(dir.ino, "a", 0o644).unwrap();
    fs.write(f.ino, 0, &noise(8 * 1024, 1)).unwrap();
    pass(&c, HOUR, 10 * MINUTE);
    fs.unlink(dir.ino, "a").unwrap();
    fs.rmdir(ROOT_INO, "d").unwrap();
    let gone = c.now();
    pass(&c, HOUR, 10 * MINUTE);

    let versions = c.control(1).history("/d/a").unwrap();
    assert_eq!(versions.len(), 1, "{versions:?}");
    assert_eq!(versions[0].size, 8 * 1024);
    assert!(versions[0].deleted);
    assert!(versions[0].replaced.is_some_and(|t| t >= gone));
}

#[test]
fn history_of_a_path_that_never_existed_is_not_found() {
    let c = Cluster::with_filesystem(2, 3, CONFIG);
    assert!(c.control(0).history("/nothing").is_err());
}

#[test]
fn a_member_caught_up_by_state_transfer_lists_the_same_versions() {
    let c = Cluster::with_filesystem(2, 3, CONFIG);
    let a = put(&c.fs(0), "a", &noise(8 * 1024, 1));
    pass(&c, HOUR, 10 * MINUTE);
    // Node 2 holds `a` as it is now, and never hears of `b` or what follows.
    c.network()
        .partition(&[&[NodeId(0), NodeId(1)], &[NodeId(2)]]);
    let b = put(&c.fs(0), "b", &noise(8 * 1024, 2));
    pass(&c, HOUR, 10 * MINUTE);
    c.fs(0).write(a, 0, &noise(9 * 1024, 3)).unwrap();
    c.fs(0).write(b, 0, &noise(9 * 1024, 4)).unwrap();
    pass(&c, 3 * HOUR, 30 * MINUTE);
    assert!(
        c.control(0).status().unwrap().log.entries < 3,
        "the log was not compacted, so no state transfer is needed"
    );

    c.network().heal();
    pass(&c, HOUR, 10 * MINUTE);
    for path in ["/a", "/b"] {
        let here = c.control(0).history(path).unwrap();
        assert_eq!(here.len(), 2, "{path}: {here:?}");
        assert_eq!(c.control(2).history(path).unwrap(), here, "{path}");
    }
}

/// The restore point `history` shows for the `n`th version of `path`, as a user would type it.
fn point(c: &Cluster, path: &str, n: usize) -> Nanos {
    let written = c.control(0).history(path).unwrap()[n].written;
    parse_restore_point(&restore_point(written)).unwrap()
}

fn read(c: &Cluster, i: usize, dir: Ino, name: &str) -> Vec<u8> {
    let fs = c.fs(i);
    let f = fs.lookup(dir, name).unwrap();
    fs.read(f.ino, 0, u32::MAX).unwrap()
}

#[test]
fn an_overwritten_file_and_a_deleted_file_are_restored_on_every_member() {
    let c = Cluster::with_filesystem(2, 3, CONFIG);
    let a = put(&c.fs(0), "a", &noise(8 * 1024, 1));
    put(&c.fs(0), "b", &noise(8 * 1024, 2));
    pass(&c, HOUR, 10 * MINUTE);
    c.fs(0).write(a, 0, &noise(8 * 1024, 3)).unwrap();
    c.fs(1).unlink(ROOT_INO, "b").unwrap();
    pass(&c, HOUR, 10 * MINUTE);

    let (at_a, at_b) = (point(&c, "/a", 0), point(&c, "/b", 0));
    c.control(1).restore("/a", at_a).unwrap();
    c.control(1).restore("/b", at_b).unwrap();
    pass(&c, 10 * MINUTE, MINUTE);
    for i in 0..3 {
        assert_eq!(read(&c, i, ROOT_INO, "a"), noise(8 * 1024, 1), "node {i}");
        assert_eq!(read(&c, i, ROOT_INO, "b"), noise(8 * 1024, 2), "node {i}");
    }
    // The restore is a write like any other: what it replaced is a version in turn.
    let versions = c.control(2).history("/a").unwrap();
    assert_eq!(versions.len(), 3, "{versions:?}");
    assert!(versions[2].replaced.is_none());
}

#[test]
fn a_file_restored_into_a_deleted_directory_brings_the_directory_back() {
    let c = Cluster::with_filesystem(2, 3, CONFIG);
    let fs = c.fs(0);
    let dir = fs.mkdir(ROOT_INO, "d", 0o755).unwrap();
    let f = fs.create(dir.ino, "a", 0o644).unwrap();
    fs.write(f.ino, 0, &noise(8 * 1024, 1)).unwrap();
    pass(&c, HOUR, 10 * MINUTE);
    fs.unlink(dir.ino, "a").unwrap();
    fs.rmdir(ROOT_INO, "d").unwrap();
    pass(&c, HOUR, 10 * MINUTE);

    c.control(2).restore("/d/a", point(&c, "/d/a", 0)).unwrap();
    pass(&c, 10 * MINUTE, MINUTE);
    for i in 0..3 {
        let d = c.fs(i).lookup(ROOT_INO, "d").unwrap();
        assert_eq!(read(&c, i, d.ino, "a"), noise(8 * 1024, 1), "node {i}");
    }
}

#[test]
fn a_restore_point_where_the_path_held_nothing_is_refused() {
    let c = Cluster::with_filesystem(2, 3, CONFIG);
    let before = c.now() - 1_000_000_000;
    put(&c.fs(0), "a", &noise(8 * 1024, 1));
    pass(&c, HOUR, 10 * MINUTE);
    assert!(c.control(0).restore("/a", before).is_err());
    assert!(c.control(0).restore("/nothing", c.now()).is_err());
}

#[test]
fn the_window_set_on_one_member_applies_on_every_member() {
    let c = Cluster::with_filesystem(2, 3, CONFIG);
    c.control(1).set_keep_versions(3 * 86400).unwrap();
    pass(&c, MINUTE, Duration::from_secs(10));
    for i in 0..3 {
        let f = c.control(i).status().unwrap().filesystem.unwrap();
        assert_eq!(f.keep_versions_secs, 3 * 86400, "node {i}");
    }
}

#[test]
fn shortening_the_window_releases_the_versions_past_it() {
    let c = Cluster::with_filesystem(2, 3, CONFIG);
    let f = put(&c.fs(0), "a", &noise(8 * 1024, 1));
    pass(&c, HOUR, 10 * MINUTE);
    c.fs(0).write(f, 0, &noise(8 * 1024, 2)).unwrap();
    pass(&c, HOUR, 10 * MINUTE);
    assert_eq!(c.control(2).history("/a").unwrap().len(), 2);

    c.control(2).set_keep_versions(0).unwrap();
    pass(&c, 2 * HOUR, 10 * MINUTE);
    assert_eq!(c.control(0).history("/a").unwrap().len(), 1);
    assert_eq!(
        total_replicas(&c),
        2,
        "the released version's chunks are trimmed"
    );
}

#[test]
fn status_and_df_count_the_bytes_only_versions_hold() {
    let c = Cluster::with_filesystem(2, 3, CONFIG);
    let f = put(&c.fs(0), "a", &noise(8 * 1024, 1));
    put(&c.fs(0), "b", &noise(4 * 1024, 2));
    pass(&c, HOUR, 10 * MINUTE);
    c.fs(0).write(f, 0, &noise(9 * 1024, 3)).unwrap();
    c.fs(0).unlink(ROOT_INO, "b").unwrap();
    pass(&c, HOUR, 10 * MINUTE);

    for i in 0..3 {
        let s = c.control(i).status().unwrap();
        let e = s.efficiency;
        assert_eq!(e.logical_bytes, 9 * 1024, "node {i}");
        assert_eq!(e.version_bytes, 12 * 1024, "node {i}");
        assert_eq!(
            s.gc.pending_bytes, 0,
            "node {i}: versions are not waiting for GC"
        );
        assert_eq!(c.fs(i).space().unwrap().used_bytes, 21 * 1024, "node {i}");
    }
}
