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

/// Let `by` pass in steps short enough that this node never suspects it was
/// asleep itself (a single jump looks like a suspend and every peer as unseen).
fn pass(c: &Cluster, by: Duration) {
    for _ in 0..by.as_secs() / 240 {
        c.advance(Duration::from_secs(240));
        c.sync();
    }
    settle(c);
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
    pass(&c, 10 * MINUTE);

    assert!(!degraded(&c, &[0, 1, 2]));
}

#[test]
fn the_extra_copies_stay_for_the_retention_period_after_the_sleeper_is_back_then_are_trimmed() {
    let c = cluster_with(3, 4, "");
    write_files(&c, 24);
    let before = replicas(&c);
    c.crash(3);
    pass(&c, 8 * HOUR);

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
    pass(&c, 8 * HOUR);
    c.restart(3);
    settle(&c);
    pass(&c, 12 * HOUR);
    let after_first_night = replicas(&c);

    c.crash(3);
    pass(&c, 10 * MINUTE);
    assert!(!degraded(&c, &[0, 1, 2]));
    pass(&c, 8 * HOUR);

    assert_eq!(replicas(&c)[..3], after_first_night[..3]);
    assert!(!degraded(&c, &[0, 1, 2]));
}

#[test]
fn a_member_that_never_returns_is_replaced_after_the_grace_period_as_before() {
    let c = cluster_with(3, 5, "");
    write_files(&c, 24);
    let before: u64 = replicas(&c).iter().sum();

    c.crash(4);
    pass(&c, 31 * MINUTE);
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
    pass(&c, 10 * MINUTE);
    assert!(degraded(&c, &[0, 1, 2]));

    c.restart(3);
    settle(&c);
    assert!(!degraded(&c, &[0, 1, 2, 3]));
    assert_eq!(replicas(&c), before);
}

#[test]
fn a_node_that_was_asleep_itself_does_not_take_its_peers_for_asleep() {
    let c = cluster_with(3, 4, "");
    write_files(&c, 24);
    let before = replicas(&c);

    // Every node's clock jumps a day at once, as after the whole machine slept:
    // all peers look unseen until their hellos arrive.
    c.advance(DAY);
    c.sync();
    settle(&c);

    assert_eq!(replicas(&c), before);
    assert!(!degraded(&c, &[0, 1, 2, 3]));
}

#[test]
fn status_shows_whose_stand_ins_are_kept_and_for_how_long() {
    let c = cluster_with(3, 4, "standin_days = 1");
    write_files(&c, 24);
    let left = |c: &Cluster| c.control(0).status().unwrap().members[3].standin_secs_left;
    assert_eq!(left(&c), None);

    c.crash(3);
    pass(&c, 10 * MINUTE);
    let asleep = left(&c).expect("covered while asleep");
    assert!(asleep > 23 * 3600 && asleep <= 24 * 3600, "{asleep}");

    c.restart(3);
    settle(&c);
    pass(&c, 12 * HOUR);
    let back = left(&c).expect("still kept");
    assert!(back < 12 * 3600 + 3600, "{back}");

    pass(&c, DAY);
    assert_eq!(left(&c), None);
}

#[test]
fn lowering_copies_with_stand_ins_kept_is_accepted_and_ends_at_the_new_count() {
    let c = cluster_with(3, 4, "standin_days = 1");
    write_files(&c, 24);
    c.crash(3);
    pass(&c, 8 * HOUR);
    c.restart(3);
    settle(&c);
    assert!(replicas(&c).iter().sum::<u64>() > 72);

    c.control(0).set_copies(2).unwrap();
    settle(&c);
    assert!(!degraded(&c, &[0, 1, 2, 3]));
    pass(&c, 2 * DAY);

    assert_eq!(replicas(&c).iter().sum::<u64>(), 48, "{:?}", replicas(&c));
}
