//! `startfs purge`: reclaim deleted data on every member now, not after the
//! retention period, without giving up the reclaim gate.

use startfs_core::net::NodeId;
use startfs_core::ops::*;
use startfs_harness::{Cluster, noise};
use std::time::Duration;

const HOUR: Duration = Duration::from_secs(3600);
const DAY: Duration = Duration::from_secs(24 * 3600);

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

fn purging(c: &Cluster, i: usize) -> bool {
    c.control(i).status().unwrap().gc.purging
}

#[test]
fn a_purge_frees_deleted_data_on_every_member_without_waiting_for_the_retention() {
    let c = Cluster::with_filesystem(2, 3, "");
    put(&c.fs(0), "doomed", &noise(8 * 1024, 1));
    c.sync();
    let held = total_replicas(&c);
    assert!(held >= 2, "{held}");
    c.fs(0).unlink(ROOT_INO, "doomed").unwrap();
    c.elapse(HOUR);
    assert_eq!(total_replicas(&c), held, "kept for the retention period");

    // Asked of one member, done on all of them.
    let report = c.control(2).purge().unwrap();
    assert_eq!(report.asked.len(), 3);
    c.elapse(Duration::from_secs(300));

    assert_eq!(total_replicas(&c), 0);
    let reclaimed: u64 = (0..3)
        .map(|i| c.control(i).status().unwrap().gc.reclaimed_chunks)
        .sum();
    assert_eq!(reclaimed, held);
    // Once nothing is held back the purge is over.
    for i in 0..3 {
        assert!(!purging(&c, i), "node {i}");
    }
}

#[test]
fn a_purge_never_removes_data_a_file_still_uses() {
    let c = Cluster::with_filesystem(2, 3, "");
    let keep = noise(60 * 1024, 2);
    let kept = put(&c.fs(0), "keeper", &keep);
    put(&c.fs(0), "doomed", &noise(60 * 1024, 3));
    c.sync();
    let both = total_replicas(&c);
    c.fs(0).unlink(ROOT_INO, "doomed").unwrap();

    c.control(0).purge().unwrap();
    c.elapse(Duration::from_secs(300));

    assert_eq!(total_replicas(&c), both / 2);
    for i in 0..3 {
        assert_eq!(c.fs(i).read(kept, 0, u32::MAX).unwrap(), keep, "node {i}");
    }
}

#[test]
fn a_purge_is_refused_while_a_member_is_away_and_works_once_it_is_back() {
    let c = Cluster::with_filesystem(3, 3, "");
    put(&c.fs(0), "doomed", &noise(8 * 1024, 4));
    c.sync();
    let held = total_replicas(&c);
    c.network()
        .partition(&[&[NodeId(0), NodeId(1)], &[NodeId(2)]]);
    c.fs(0).unlink(ROOT_INO, "doomed").unwrap();
    c.elapse(Duration::from_secs(300));

    // Purging without it would leave its copy to be handed back to the others.
    let err = c.control(0).purge().unwrap_err().to_string();
    assert!(err.contains("unreachable"), "{err}");
    assert!(!purging(&c, 0));
    assert_eq!(total_replicas(&c), held);

    c.network().heal();
    c.elapse(Duration::from_secs(300));
    c.control(0).purge().unwrap();
    c.elapse(Duration::from_secs(300));
    assert_eq!(total_replicas(&c), 0);
}

#[test]
fn deleting_a_file_and_purging_at_once_frees_it_on_every_member() {
    let c = Cluster::with_filesystem(3, 3, "");
    put(&c.fs(0), "doomed", &noise(60 * 1024, 8));
    c.sync();
    assert!(total_replicas(&c) >= 3);
    // The other members may not have seen the delete when the purge reaches them.
    c.fs(0).unlink(ROOT_INO, "doomed").unwrap();
    c.control(0).purge().unwrap();
    c.elapse(Duration::from_secs(300));
    assert_eq!(total_replicas(&c), 0);
}

#[test]
fn a_purge_with_nothing_to_free_changes_nothing_and_ends() {
    let c = Cluster::with_filesystem(2, 3, "");
    put(&c.fs(0), "file", &noise(60 * 1024, 5));
    c.sync();
    let held = total_replicas(&c);
    c.control(1).purge().unwrap();
    c.elapse(Duration::from_secs(300));
    assert_eq!(total_replicas(&c), held);
    for i in 0..3 {
        assert!(!purging(&c, i), "node {i}");
    }
}

#[test]
fn files_deleted_after_a_purge_keep_their_retention() {
    let c = Cluster::with_filesystem(2, 3, "");
    put(&c.fs(0), "first", &noise(60 * 1024, 6));
    put(&c.fs(0), "second", &noise(60 * 1024, 7));
    c.sync();
    let both = total_replicas(&c);
    c.fs(0).unlink(ROOT_INO, "first").unwrap();
    c.elapse(Duration::from_secs(300));

    c.control(0).purge().unwrap();
    c.elapse(Duration::from_secs(300));
    assert_eq!(
        total_replicas(&c),
        both / 2,
        "the purge freed the first file"
    );
    for i in 0..3 {
        assert!(!purging(&c, i), "node {i}");
    }

    // The purge is over: this delete is recoverable for the whole retention again.
    c.fs(0).unlink(ROOT_INO, "second").unwrap();
    c.elapse(6 * DAY);
    assert_eq!(
        total_replicas(&c),
        both / 2,
        "freed before the retention ended"
    );
    c.elapse(2 * DAY);
    assert_eq!(total_replicas(&c), 0);
}
