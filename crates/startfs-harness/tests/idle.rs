//! A quiet node sleeps between rounds of background work; one with work to do does not.

use startfs_core::daemon::{BUSY_WAIT, IDLE_WAIT};
use startfs_core::ops::*;
use startfs_harness::{Cluster, noise};
use std::time::Duration;

fn settle(c: &Cluster) {
    for _ in 0..30 {
        c.sync();
        c.elapse(Duration::from_secs(2));
    }
    c.sync();
}

#[test]
fn a_quiet_cluster_sleeps_the_full_idle_wait() {
    let c = Cluster::with_filesystem(2, 3, "");
    let f = c.fs(0).create(ROOT_INO, "a", 0o644).unwrap();
    c.fs(0).write(f.ino, 0, &noise(1 << 20, 1)).unwrap();
    c.fs(0).flush(f.ino).unwrap();
    settle(&c);

    for i in 0..3 {
        assert_eq!(c.idle_wait(i), IDLE_WAIT, "node {i} has nothing to do");
    }
}

#[test]
fn a_node_with_buffered_writes_comes_back_soon() {
    let c = Cluster::with_filesystem(2, 2, "");
    settle(&c);
    assert_eq!(c.idle_wait(0), IDLE_WAIT);

    let f = c.fs(0).create(ROOT_INO, "a", 0o644).unwrap();
    c.fs(0).write(f.ino, 0, b"not committed yet").unwrap();
    assert_eq!(
        c.idle_wait(0),
        BUSY_WAIT,
        "the commit delay is a timer nothing wakes"
    );

    c.fs(0).flush(f.ino).unwrap();
    settle(&c);
    assert_eq!(c.idle_wait(0), IDLE_WAIT);
}

#[test]
fn a_chunk_waiting_to_be_replicated_is_not_left_to_the_idle_wait() {
    let c = Cluster::with_filesystem(2, 2, "");
    settle(&c);
    let f = c.fs(0).create(ROOT_INO, "a", 0o644).unwrap();
    c.fs(0).write(f.ino, 0, &noise(1 << 20, 2)).unwrap();
    c.fs(0).flush(f.ino).unwrap();

    // The chunk is staged and not yet copied: the replicator has it due.
    assert_eq!(c.idle_wait(0), BUSY_WAIT);
    settle(&c);
    assert_eq!(c.idle_wait(0), IDLE_WAIT);
}
