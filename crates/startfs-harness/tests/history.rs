//! Versioned history: an overwritten or deleted file keeps its earlier
//! versions for the filesystem's window, so they can be listed and restored.

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

    pass(&c, 5 * HOUR, HOUR);
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

    pass(&c, 6 * HOUR, HOUR);
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
