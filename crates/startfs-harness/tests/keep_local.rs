//! Keeping chosen paths in this member's cache, and freeing the rest of it.

use startfs_core::net::NodeId;
use startfs_core::ops::*;
use startfs_harness::{Cluster, noise};

/// Node 0 stores everything; node 1 only reads, through a cache that holds two of the four
/// 100 KB files.
fn reader() -> Cluster {
    let c = Cluster::start_with_config(2, "cache_bytes = 250000");
    let token = c.control(0).create("home", 2).unwrap();
    c.control(1).join(&token, Some(0)).unwrap();
    for i in 0..4 {
        let f = c.fs(0).create(ROOT_INO, &format!("f{i}"), 0o644).unwrap();
        c.fs(0).write(f.ino, 0, &data(i)).unwrap();
    }
    c.sync();
    c
}

fn data(i: u64) -> Vec<u8> {
    noise(100_000, 2 * i + 1)
}

fn read(c: &Cluster, i: u64) -> FsResult<Vec<u8>> {
    let ino = c.fs(1).lookup(ROOT_INO, &format!("f{i}"))?.ino;
    c.fs(1).read(ino, 0, u32::MAX)
}

fn offline(c: &Cluster) {
    c.network().partition(&[&[NodeId(0)], &[NodeId(1)]]);
}

#[test]
fn a_kept_path_stays_cached_while_other_reads_churn_the_cache() {
    let c = reader();
    assert_eq!(read(&c, 0).unwrap(), data(0));
    let report = c.control(1).keep_local("/f0", true).unwrap();
    assert_eq!((report.chunks, report.fetched), (1, 0));

    // Three more files through a cache that holds two push the oldest out: f0, unless kept.
    for i in [1, 2, 3] {
        assert_eq!(read(&c, i).unwrap(), data(i));
    }
    offline(&c);
    assert_eq!(read(&c, 0).unwrap(), data(0));
    assert!(read(&c, 1).is_err(), "an unkept file left the cache");
    let s = c.control(1).status().unwrap();
    assert_eq!(s.kept_local, ["/f0"]);
    assert_eq!(s.cache.kept_chunks, 1);
}

#[test]
fn keeping_fetches_what_this_member_lacks() {
    let c = reader();
    let report = c.control(1).keep_local("/f2", true).unwrap();
    assert_eq!((report.chunks, report.fetched, report.missing), (1, 1, 0));
    offline(&c);
    assert_eq!(read(&c, 2).unwrap(), data(2));
}

#[test]
fn freeing_space_evicts_the_rest_and_keeps_the_pinned() {
    let c = reader();
    c.control(1).keep_local("/f0", true).unwrap();
    assert_eq!(read(&c, 1).unwrap(), data(1));
    let before = c.control(1).status().unwrap().cache;
    assert!(before.used_bytes > before.kept_bytes, "{before:?}");

    let freed = c.control(1).free_space().unwrap();
    assert_eq!(freed, before.used_bytes - before.kept_bytes);
    let after = c.control(1).status().unwrap().cache;
    assert_eq!((after.used_bytes, after.chunks), (after.kept_bytes, 1));
    offline(&c);
    assert_eq!(read(&c, 0).unwrap(), data(0));
    assert!(read(&c, 1).is_err());
}

#[test]
fn a_released_path_is_evicted_like_any_other_and_a_directory_keeps_its_files() {
    let c = reader();
    let dir = c.fs(0).mkdir(ROOT_INO, "d", 0o755).unwrap().ino;
    for (i, seed) in [(0, 40u64), (1, 41)] {
        let f = c.fs(0).create(dir, &format!("g{i}"), 0o644).unwrap();
        c.fs(0).write(f.ino, 0, &noise(50_000, seed)).unwrap();
    }
    c.sync();
    let report = c.control(1).keep_local("/d", true).unwrap();
    assert_eq!(report.chunks, 2);
    assert_eq!(c.control(1).status().unwrap().cache.kept_chunks, 2);

    c.control(1).keep_local("/d", false).unwrap();
    let s = c.control(1).status().unwrap();
    assert!(s.kept_local.is_empty());
    assert_eq!(s.cache.kept_chunks, 0);
    c.control(1).free_space().unwrap();
    assert_eq!(c.control(1).status().unwrap().cache.chunks, 0);
}

#[test]
fn a_kept_path_must_exist_and_survives_a_restart() {
    let c = reader();
    assert!(c.control(1).keep_local("/nothing", true).is_err());
    c.control(1).keep_local("/f0", true).unwrap();
    c.crash(1);
    c.restart(1);
    c.sync();
    assert_eq!(c.control(1).status().unwrap().kept_local, ["/f0"]);
}
