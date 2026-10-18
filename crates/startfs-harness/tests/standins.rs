//! Stand-in copies for members that sleep: the cluster covers for them, and
//! keeps the extra copies for a while in case they sleep again.

use startfs_core::ops::*;
use startfs_harness::Cluster;
use std::time::Duration;

const BIG: u64 = 10 << 30;
const MINUTE: Duration = Duration::from_secs(60);
const HOUR: Duration = Duration::from_secs(3600);
const DAY: Duration = Duration::from_secs(24 * 3600);

/// Incompressible bytes, so a file really needs chunk storage.
fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

/// A filesystem of `copies` copies on `nodes` nodes, all offering plenty.
fn cluster_with(copies: u32, nodes: usize, config: &str) -> Cluster {
    let c = Cluster::start_with_config(nodes, config);
    let token = c.control(0).create("home", copies).unwrap();
    for i in 1..nodes {
        c.control(i).join(&token, Some(BIG)).unwrap();
    }
    c.sync();
    c
}

fn write_files(c: &Cluster, count: u64) {
    for i in 0..count {
        let f = c.fs(0).create(ROOT_INO, &format!("f{i}"), 0o644).unwrap();
        c.fs(0).write(f.ino, 0, &noise(8 * 1024, i + 1)).unwrap();
    }
    c.sync();
}

/// Chunks held as replicas on each node, as `status` reports them.
fn replicas(c: &Cluster) -> Vec<u64> {
    (0..c.node_count())
        .map(|i| c.control(i).status().map_or(0, |s| s.usage.chunks))
        .collect()
}

/// Whether any reachable node (from `nodes`) lists chunks with too few copies.
fn degraded(c: &Cluster, nodes: &[usize]) -> bool {
    nodes
        .iter()
        .any(|i| !c.control(*i).status().unwrap().unreplicated.is_empty())
}

/// Let `by` pass with every member up, in steps short enough that none is
/// ever taken for asleep.
fn pass(c: &Cluster, by: Duration) {
    for _ in 0..by.as_secs() / 240 {
        c.advance(Duration::from_secs(240));
        c.sync();
    }
}

/// Let the nodes exchange state a few times.
fn settle(c: &Cluster) {
    for _ in 0..3 {
        c.advance(Duration::from_secs(10));
        c.sync();
    }
}

#[test]
fn a_member_asleep_past_the_stand_in_delay_is_covered_for_and_nothing_shows_degraded() {
    let c = cluster_with(3, 4, "");
    write_files(&c, 24);
    assert!(replicas(&c)[3] > 0);
    assert!(!degraded(&c, &[0, 1, 2]));

    c.crash(3);
    c.advance(10 * MINUTE);
    c.sync();

    assert!(!degraded(&c, &[0, 1, 2]));
}

#[test]
fn the_extra_copies_stay_for_the_retention_period_after_the_sleeper_is_back_then_are_trimmed() {
    let c = cluster_with(3, 4, "");
    write_files(&c, 24);
    let before = replicas(&c);
    c.crash(3);
    c.advance(8 * HOUR);
    c.sync();

    c.restart(3);
    settle(&c);
    pass(&c, 2 * DAY);
    settle(&c);
    let kept: u64 = replicas(&c).iter().sum();
    assert!(
        kept > before.iter().sum::<u64>(),
        "{before:?} -> {:?}",
        replicas(&c)
    );
    assert!(!degraded(&c, &[0, 1, 2, 3]));

    pass(&c, 2 * DAY);
    settle(&c);
    assert_eq!(replicas(&c), before);
}

#[test]
fn a_member_that_sleeps_again_within_the_retention_period_costs_no_new_copies() {
    let c = cluster_with(3, 4, "");
    write_files(&c, 24);
    c.crash(3);
    c.advance(8 * HOUR);
    c.sync();
    c.restart(3);
    settle(&c);
    pass(&c, 12 * HOUR);
    let after_first_night = replicas(&c);

    c.crash(3);
    c.advance(10 * MINUTE);
    c.sync();
    assert!(!degraded(&c, &[0, 1, 2]));
    c.advance(8 * HOUR);
    c.sync();

    assert_eq!(replicas(&c)[..3], after_first_night[..3]);
    assert!(!degraded(&c, &[0, 1, 2]));
}

#[test]
fn a_member_that_never_returns_is_replaced_after_the_grace_period_as_before() {
    let c = cluster_with(3, 5, "");
    write_files(&c, 24);
    let before: u64 = replicas(&c).iter().sum();

    c.crash(4);
    c.advance(31 * MINUTE);
    c.sync();
    settle(&c);

    let after = replicas(&c);
    assert_eq!(after[..4].iter().sum::<u64>(), before, "{after:?}");
    assert!(!degraded(&c, &[0, 1, 2, 3]));
}

#[test]
fn with_stand_ins_off_a_sleeping_member_leaves_chunks_degraded_and_nothing_is_kept() {
    let c = cluster_with(3, 4, "standin_days = 0");
    write_files(&c, 24);
    let before = replicas(&c);

    c.crash(3);
    c.advance(10 * MINUTE);
    c.sync();
    assert!(degraded(&c, &[0, 1, 2]));

    c.restart(3);
    settle(&c);
    assert!(!degraded(&c, &[0, 1, 2, 3]));
    assert_eq!(replicas(&c), before);
}
