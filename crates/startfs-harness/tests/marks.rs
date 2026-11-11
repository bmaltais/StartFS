//! Marks: a named instant that holds every version needed to restore to it past the
//! filesystem's window, and that `restore` accepts by name.

use startfs_core::net::NodeId;
use startfs_core::ops::*;
use startfs_harness::{Cluster, noise};
use std::time::Duration;

const MINUTE: Duration = Duration::from_secs(60);
const HOUR: Duration = Duration::from_secs(3600);
const DAY: Duration = Duration::from_secs(86400);

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

/// Mark the present on node `i`, then let a minute pass so that what follows is plainly later:
/// a state that ends at the very instant of a mark is not the one the mark names.
fn mark(c: &Cluster, i: usize, name: &str) {
    c.control(i).mark_create(name).unwrap();
    pass(c, MINUTE, Duration::from_secs(10));
}

fn read(c: &Cluster, i: usize, name: &str) -> Vec<u8> {
    let fs = c.fs(i);
    let f = fs.lookup(ROOT_INO, name).unwrap();
    fs.read(f.ino, 0, u32::MAX).unwrap()
}

#[test]
fn a_mark_keeps_an_overwritten_file_restorable_past_the_window() {
    let c = Cluster::with_filesystem(2, 3, CONFIG);
    let a = put(&c.fs(0), "a", &noise(8 * 1024, 1));
    pass(&c, HOUR, 10 * MINUTE);
    mark(&c, 0, "before");
    c.fs(0).write(a, 0, &noise(8 * 1024, 2)).unwrap();
    pass(&c, 5 * DAY, HOUR);

    c.control(1).restore_mark("/a", "before").unwrap();
    pass(&c, 10 * MINUTE, MINUTE);
    for i in 0..3 {
        assert_eq!(read(&c, i, "a"), noise(8 * 1024, 1), "node {i}");
    }
}

#[test]
fn without_a_mark_the_version_is_gone_after_the_window() {
    let c = Cluster::with_filesystem(2, 3, CONFIG);
    let a = put(&c.fs(0), "a", &noise(8 * 1024, 1));
    pass(&c, HOUR, 10 * MINUTE);
    c.fs(0).write(a, 0, &noise(8 * 1024, 2)).unwrap();
    pass(&c, 5 * DAY, HOUR);

    assert_eq!(c.control(0).history("/a").unwrap().len(), 1);
    assert_eq!(total_replicas(&c), 2, "only the current data is held");
}

#[test]
fn deleting_the_mark_releases_what_only_it_held() {
    let c = Cluster::with_filesystem(2, 3, CONFIG);
    let a = put(&c.fs(0), "a", &noise(8 * 1024, 1));
    pass(&c, HOUR, 10 * MINUTE);
    mark(&c, 0, "before");
    c.fs(0).write(a, 0, &noise(8 * 1024, 2)).unwrap();
    pass(&c, 5 * DAY, HOUR);
    assert_eq!(c.control(2).history("/a").unwrap().len(), 2);
    assert_eq!(
        total_replicas(&c),
        4,
        "the marked version and the current one"
    );

    c.control(1).mark_delete("before").unwrap();
    pass(&c, 2 * HOUR, 10 * MINUTE);
    assert_eq!(c.control(0).history("/a").unwrap().len(), 1);
    assert_eq!(total_replicas(&c), 2, "the marked version is trimmed");
}

#[test]
fn a_mark_names_the_state_of_a_deleted_directory_tree_past_the_window() {
    let c = Cluster::with_filesystem(2, 3, CONFIG);
    let fs = c.fs(0);
    let top = fs.mkdir(ROOT_INO, "proj", 0o755).unwrap();
    let f = fs.create(top.ino, "x", 0o644).unwrap();
    fs.write(f.ino, 0, &noise(8 * 1024, 7)).unwrap();
    pass(&c, HOUR, 10 * MINUTE);
    mark(&c, 0, "m");
    fs.unlink(top.ino, "x").unwrap();
    fs.rmdir(ROOT_INO, "proj").unwrap();
    pass(&c, 5 * DAY, HOUR);

    c.control(2).restore_mark("/proj", "m").unwrap();
    pass(&c, 10 * MINUTE, MINUTE);
    for i in 0..3 {
        let fs = c.fs(i);
        let top = fs.lookup(ROOT_INO, "proj").unwrap();
        let f = fs.lookup(top.ino, "x").unwrap();
        assert_eq!(
            fs.read(f.ino, 0, u32::MAX).unwrap(),
            noise(8 * 1024, 7),
            "node {i}"
        );
    }
}

#[test]
fn a_marks_chunks_are_repaired_to_n_copies_after_a_member_is_lost() {
    let c = Cluster::with_filesystem(2, 3, CONFIG);
    let a = put(&c.fs(0), "a", &noise(8 * 1024, 1));
    pass(&c, HOUR, 10 * MINUTE);
    mark(&c, 0, "m");
    c.fs(0).write(a, 0, &noise(8 * 1024, 2)).unwrap();
    pass(&c, 5 * DAY, HOUR);

    c.crash(2);
    pass(&c, 2 * HOUR, 10 * MINUTE);
    let held: u64 = (0..2)
        .map(|i| c.control(i).status().unwrap().usage.chunks)
        .sum();
    assert_eq!(
        held, 4,
        "both versions are back to two copies on the survivors"
    );
}

#[test]
fn marks_are_listed_oldest_first_and_every_member_sees_them() {
    let c = Cluster::with_filesystem(2, 3, CONFIG);
    mark(&c, 0, "first");
    pass(&c, HOUR, 10 * MINUTE);
    let second = c.control(1).mark_create("second").unwrap();
    pass(&c, MINUTE, Duration::from_secs(10));
    for i in 0..3 {
        let marks = c.control(i).marks().unwrap();
        let names: Vec<_> = marks.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["first", "second"], "node {i}");
        assert_eq!(marks[1].at, second.at, "node {i}");
        assert_eq!(marks[1].by, second.by, "node {i}");
    }
}

#[test]
fn a_member_that_was_away_learns_the_marks_when_it_returns() {
    let c = Cluster::with_filesystem(2, 3, CONFIG);
    c.network()
        .partition(&[&[NodeId(0), NodeId(1)], &[NodeId(2)]]);
    mark(&c, 0, "m");
    pass(&c, 2 * DAY, HOUR);
    assert!(c.control(2).marks().unwrap().is_empty());

    c.network().heal();
    pass(&c, 10 * MINUTE, MINUTE);
    assert_eq!(c.control(2).marks().unwrap().len(), 1);
}

#[test]
fn a_delete_beats_a_create_a_member_missed_and_the_name_can_be_made_again() {
    let c = Cluster::with_filesystem(2, 3, CONFIG);
    c.network()
        .partition(&[&[NodeId(0), NodeId(1)], &[NodeId(2)]]);
    mark(&c, 0, "m");
    pass(&c, 10 * MINUTE, MINUTE);
    c.control(0).mark_delete("m").unwrap();
    pass(&c, 10 * MINUTE, MINUTE);

    c.network().heal();
    pass(&c, 10 * MINUTE, MINUTE);
    for i in 0..3 {
        assert!(c.control(i).marks().unwrap().is_empty(), "node {i}");
    }
    mark(&c, 2, "m");
    pass(&c, 10 * MINUTE, MINUTE);
    for i in 0..3 {
        assert_eq!(c.control(i).marks().unwrap().len(), 1, "node {i}");
    }
}

#[test]
fn two_members_making_the_same_name_at_once_settle_on_the_earlier_instant() {
    let c = Cluster::with_filesystem(2, 3, CONFIG);
    c.network()
        .partition(&[&[NodeId(0)], &[NodeId(1), NodeId(2)]]);
    let early = c.control(0).mark_create("m").unwrap();
    pass(&c, HOUR, 10 * MINUTE);
    let late = c.control(1).mark_create("m").unwrap();
    assert!(early.at < late.at);

    c.network().heal();
    pass(&c, 10 * MINUTE, MINUTE);
    for i in 0..3 {
        let marks = c.control(i).marks().unwrap();
        assert_eq!(marks.len(), 1, "node {i}");
        assert_eq!(marks[0].at, early.at, "node {i}");
    }
}

#[test]
fn a_mark_can_be_neither_made_twice_nor_given_a_bad_name_nor_found_when_absent() {
    let c = Cluster::with_filesystem(2, 3, CONFIG);
    let control = c.control(0);
    control.mark_create("m").unwrap();
    assert!(control.mark_create("m").is_err(), "a live name is taken");
    let long = "x".repeat(65);
    for bad in ["", "a b", "a/b", long.as_str(), "20261101-091502.114"] {
        assert!(control.mark_create(bad).is_err(), "{bad:?}");
    }
    assert!(control.mark_delete("nothing").is_err());
    assert!(control.restore_mark("/a", "nothing").is_err());
    control.mark_delete("m").unwrap();
    assert!(control.mark_delete("m").is_err(), "already deleted");
}

#[test]
fn status_and_df_count_the_bytes_only_marks_hold() {
    let c = Cluster::with_filesystem(2, 3, CONFIG);
    let a = put(&c.fs(0), "a", &noise(8 * 1024, 1));
    pass(&c, HOUR, 10 * MINUTE);
    mark(&c, 0, "m");
    c.fs(0).write(a, 0, &noise(9 * 1024, 2)).unwrap();
    pass(&c, HOUR, 10 * MINUTE);
    for i in 0..3 {
        let e = c.control(i).status().unwrap().efficiency;
        assert_eq!(
            e.version_bytes,
            8 * 1024,
            "node {i}: held by the window too"
        );
        assert_eq!(e.mark_bytes, 0, "node {i}");
    }

    pass(&c, 5 * DAY, HOUR);
    for i in 0..3 {
        let e = c.control(i).status().unwrap().efficiency;
        assert_eq!(e.version_bytes, 8 * 1024, "node {i}");
        assert_eq!(
            e.mark_bytes,
            8 * 1024,
            "node {i}: only the mark holds it now"
        );
        assert_eq!(c.fs(i).space().unwrap().used_bytes, 17 * 1024, "node {i}");
    }
}

#[test]
fn status_text_lists_the_marks_that_exist() {
    use startfs_core::status_view::render;
    let c = Cluster::with_filesystem(2, 3, CONFIG);
    let quiet = render(&c.control(0).status().unwrap(), false);
    assert!(!quiet.contains("marks:"), "{quiet}");

    mark(&c, 0, "before-upgrade");
    mark(&c, 1, "scratch-mark");
    c.control(1).mark_delete("scratch-mark").unwrap();
    pass(&c, MINUTE, Duration::from_secs(10));
    for i in 0..3 {
        let text = render(&c.control(i).status().unwrap(), false);
        let line = text.lines().find(|l| l.starts_with("marks:")).unwrap();
        assert!(line.contains("before-upgrade"), "node {i}: {line}");
        assert!(!line.contains("scratch-mark"), "node {i}: {line}");
    }
}

#[test]
fn a_member_away_longer_than_the_window_still_holds_the_marked_versions_on_return() {
    let c = Cluster::with_filesystem(2, 3, CONFIG);
    let a = put(&c.fs(0), "a", &noise(8 * 1024, 1));
    pass(&c, HOUR, 10 * MINUTE);
    c.network()
        .partition(&[&[NodeId(0), NodeId(1)], &[NodeId(2)]]);
    mark(&c, 0, "m");
    c.fs(0).write(a, 0, &noise(8 * 1024, 2)).unwrap();
    pass(&c, 5 * DAY, HOUR);

    c.network().heal();
    pass(&c, HOUR, MINUTE);
    for i in 0..3 {
        assert_eq!(c.control(i).history("/a").unwrap().len(), 2, "node {i}");
    }
    c.control(2).restore_mark("/a", "m").unwrap();
    pass(&c, 10 * MINUTE, MINUTE);
    for i in 0..3 {
        assert_eq!(read(&c, i, "a"), noise(8 * 1024, 1), "node {i}");
    }
}
