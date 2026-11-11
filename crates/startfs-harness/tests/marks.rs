//! Marks: a named instant that holds every version needed to restore to it past the
//! filesystem's window, and that `restore` accepts by name.

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
    c.control(0).mark_create("before").unwrap();
    pass(&c, MINUTE, Duration::from_secs(10));
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
