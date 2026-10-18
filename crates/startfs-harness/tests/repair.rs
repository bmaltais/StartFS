//! Failure repair, leave and recovery, driven through FsOps and the control
//! interface on a simulated cluster with a manual clock.

use startfs_core::control::NodeState;
use startfs_core::members::MemberId;
use startfs_core::net::NodeId;
use startfs_core::ops::*;
use startfs_harness::Cluster;
use std::time::Duration;

const BIG: u64 = 10 << 30;
const MINUTE: Duration = Duration::from_secs(60);

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
fn cluster(copies: u32, nodes: usize) -> Cluster {
    cluster_with(copies, nodes, "")
}

fn cluster_with(copies: u32, nodes: usize, config: &str) -> Cluster {
    let c = Cluster::start_with_config(nodes, config);
    let token = c.control(0).create("home", copies).unwrap();
    for i in 1..nodes {
        c.control(i).join(&token, Some(BIG)).unwrap();
    }
    c.sync();
    c
}

fn put(fs: &impl FsOps, name: &str, data: &[u8]) -> Ino {
    let f = fs.create(ROOT_INO, name, 0o644).unwrap();
    assert_eq!(fs.write(f.ino, 0, data).unwrap(), data.len());
    f.ino
}

/// Chunks held as replicas on each node, as `status` reports them.
fn replicas(c: &Cluster) -> Vec<u64> {
    (0..c.node_count())
        .map(|i| c.control(i).status().map_or(0, |s| s.usage.chunks))
        .collect()
}

fn write_files(c: &Cluster, count: u64) {
    for i in 0..count {
        put(&c.fs(0), &format!("f{i}"), &noise(8 * 1024, i + 1));
    }
    c.sync();
}

#[test]
fn after_the_grace_period_every_chunk_is_back_to_n_copies_without_user_action() {
    let c = cluster(2, 4);
    write_files(&c, 24);
    let before = replicas(&c);
    assert!(before[3] > 0, "{before:?}");

    c.crash(3);
    c.advance(31 * MINUTE);
    c.sync();

    let after = replicas(&c);
    assert_eq!(
        after[..3].iter().sum::<u64>(),
        before.iter().sum::<u64>(),
        "{before:?} -> {after:?}"
    );
}

#[test]
fn an_outage_shorter_than_the_grace_period_triggers_no_repair_while_two_copies_remain() {
    // Without stand-in copies, which would cover for the member from five minutes on.
    let c = cluster_with(3, 4, "standin_days = 0");
    write_files(&c, 24);
    let before = replicas(&c);

    c.crash(3);
    c.advance(29 * MINUTE);
    c.sync();

    let after = replicas(&c);
    assert_eq!(&after[..3], &before[..3], "{before:?} -> {after:?}");
}

#[test]
fn a_chunk_left_with_one_reachable_copy_is_raised_to_two_without_waiting_for_the_grace_period() {
    let c = cluster(2, 4);
    write_files(&c, 24);
    let before = replicas(&c);
    assert!(before[3] > 0, "{before:?}");

    c.crash(3);
    // Long enough to count as down, far short of the grace period.
    c.advance(2 * MINUTE);
    c.sync();

    let after = replicas(&c);
    assert_eq!(
        after[..3].iter().sum::<u64>(),
        before.iter().sum::<u64>(),
        "{before:?} -> {after:?}"
    );
}

#[test]
fn stand_in_copies_made_during_a_short_outage_are_trimmed_when_the_node_returns() {
    let c = cluster(2, 4);
    write_files(&c, 24);
    let before = replicas(&c);
    c.crash(3);
    c.advance(2 * MINUTE);
    c.sync();

    c.restart(3);
    for _ in 0..3 {
        c.advance(Duration::from_secs(10));
        c.sync();
    }

    assert_eq!(replicas(&c), before);
}

#[test]
fn a_returning_nodes_copies_count_again_and_the_surplus_is_trimmed() {
    // Without stand-in copies, which would keep the surplus for days.
    let c = cluster_with(2, 4, "standin_days = 0");
    write_files(&c, 24);
    let before = replicas(&c);
    c.crash(3);
    c.advance(31 * MINUTE);
    c.sync();

    c.restart(3);
    for _ in 0..3 {
        c.advance(Duration::from_secs(10));
        c.sync();
    }

    // Placement is as before the outage: the stand-in copies are gone again.
    assert_eq!(replicas(&c), before);
    for i in 0..4 {
        assert!(c.control(i).status().unwrap().unreplicated.is_empty());
    }
}

#[test]
fn repair_is_rate_limited_per_node() {
    // About two and a half 8 KiB chunks a second per node.
    let c = cluster_with(2, 4, "repair_bytes_per_sec = 20000");
    write_files(&c, 60);
    let before = replicas(&c);
    let target = before.iter().sum::<u64>();
    let have = |c: &Cluster| replicas(c)[..3].iter().sum::<u64>();

    c.crash(3);
    c.advance(31 * MINUTE);
    c.sync();
    let mut seen = have(&c);
    assert!(seen < target, "repair finished at once: {before:?}");

    let mut seconds = 0;
    while seen < target {
        c.advance(Duration::from_secs(1));
        c.sync();
        let now = have(&c);
        // Three surviving nodes, each good for at most 3 chunks a second.
        assert!(now - seen <= 3 * 3, "{seen} -> {now} in one second");
        seen = now;
        seconds += 1;
        assert!(seconds < 60, "repair does not finish");
    }
    assert!(seconds >= 3, "repair finished in {seconds}s");
}

#[test]
fn status_shows_repair_progress_and_clears_it_when_done() {
    let c = cluster_with(2, 4, "repair_bytes_per_sec = 20000");
    write_files(&c, 60);
    c.crash(3);
    for s in (0..3).map(|i| c.control(i).status().unwrap()) {
        assert_eq!(s.repair, None);
    }

    c.advance(31 * MINUTE);
    c.sync();
    let queued = |c: &Cluster| -> u32 {
        (0..3)
            .map(|i| {
                c.control(i)
                    .status()
                    .unwrap()
                    .repair
                    .map_or(0, |r| r.at_risk + r.degraded)
            })
            .sum()
    };
    let first = queued(&c);
    assert!(first > 0, "no repair shown while it is under way");

    let mut last = first;
    for _ in 0..60 {
        c.advance(Duration::from_secs(1));
        c.sync();
        let now = queued(&c);
        assert!(now <= last, "{last} -> {now}");
        last = now;
    }
    assert_eq!(last, 0);
    // The lost node is shown as such.
    let s = c.control(0).status().unwrap();
    assert_eq!(s.members[3].state, NodeState::Lost);
}

#[test]
fn at_risk_chunks_are_repaired_before_degraded_ones() {
    let c = cluster_with(3, 5, "repair_bytes_per_sec = 20000");
    write_files(&c, 80);

    // Two nodes die: chunks that lived on both are down to one copy.
    c.crash(3);
    c.crash(4);
    c.advance(31 * MINUTE);
    c.sync();
    // What the survivors still have to repair: (at risk, degraded) chunks.
    let queue = |c: &Cluster| -> (u32, u32) {
        (0..3)
            .filter_map(|i| c.control(i).status().unwrap().repair)
            .fold((0, 0), |(a, d), r| (a + r.at_risk, d + r.degraded))
    };
    // Copies still to make: an at-risk chunk needs two, a degraded one needs one.
    let work = |(a, d): (u32, u32)| 2 * a + d;
    let start = queue(&c);
    assert!(start.0 > 0 && start.1 > 0, "{start:?}");

    // Until every at-risk chunk has a second copy, almost every copy made went
    // to an at-risk chunk (a node may spend a spare second's budget on the rest).
    let mut seconds = 0;
    while queue(&c).0 > 0 {
        c.advance(Duration::from_secs(1));
        c.sync();
        seconds += 1;
        assert!(seconds < 120, "at-risk chunks are never repaired");
    }
    let made = work(start) - work(queue(&c));
    assert!(
        made <= 2 * start.0 + 3 * 3,
        "{made} copies made for {start:?}"
    );
}

#[test]
fn the_grace_period_is_configurable() {
    let c = cluster_with(3, 4, "repair_grace_secs = 600\nstandin_days = 0");
    write_files(&c, 24);
    let before = replicas(&c);

    c.crash(3);
    c.advance(9 * MINUTE);
    c.sync();
    assert_eq!(&replicas(&c)[..3], &before[..3], "repaired too early");

    c.advance(2 * MINUTE);
    c.sync();
    let after = replicas(&c);
    assert_eq!(after[..3].iter().sum::<u64>(), before.iter().sum::<u64>());
}

#[test]
fn any_n_minus_one_nodes_can_fail_once_replication_completed() {
    let c = cluster(3, 5);
    let files: Vec<_> = (0..10)
        .map(|i| {
            let data = noise(8 * 1024, i + 1);
            (put(&c.fs(0), &format!("f{i}"), &data), data)
        })
        .collect();
    c.sync();
    drop(files);

    for (down_a, down_b) in [(0, 1), (2, 3), (3, 4)] {
        c.crash(down_a);
        c.crash(down_b);
        for reader in (0..5).filter(|r| *r != down_a && *r != down_b) {
            for i in 0..10 {
                let ino = c.fs(reader).lookup(ROOT_INO, &format!("f{i}")).unwrap().ino;
                assert_eq!(
                    c.fs(reader).read(ino, 0, u32::MAX).unwrap(),
                    noise(8 * 1024, i + 1),
                    "node {reader} lost f{i} with {down_a} and {down_b} down"
                );
            }
        }
        c.restart(down_a);
        c.restart(down_b);
    }
}

/// Let a few seconds of background work pass.
fn settle(c: &Cluster) {
    for _ in 0..3 {
        c.advance(Duration::from_secs(10));
        c.sync();
    }
}

#[test]
fn a_node_that_leaves_is_removed_and_its_data_is_re_replicated_first() {
    let c = cluster(2, 4);
    write_files(&c, 24);
    let before = replicas(&c);
    assert!(before[3] > 0, "{before:?}");

    c.control(3).leave().unwrap();
    settle(&c);

    let after = replicas(&c);
    assert_eq!(
        after[..3].iter().sum::<u64>(),
        before.iter().sum::<u64>(),
        "{after:?}"
    );
    assert_eq!(after[3], 0, "the leaver still holds data");
    for i in 0..3 {
        let s = c.control(i).status().unwrap();
        assert_eq!(s.members.len(), 3);
        assert_eq!(s.nodes, 3);
        assert!(s.unreplicated.is_empty());
    }
    assert_eq!(c.control(3).status().unwrap().filesystem, None);
    for i in 0..24 {
        let ino = c.fs(0).lookup(ROOT_INO, &format!("f{i}")).unwrap().ino;
        assert_eq!(
            c.fs(0).read(ino, 0, u32::MAX).unwrap(),
            noise(8 * 1024, i + 1)
        );
    }
}

#[test]
fn a_leaving_node_stays_until_its_data_is_safe_elsewhere() {
    let c = cluster(2, 3);
    write_files(&c, 12);
    let ids = |ns: &[u32]| ns.iter().map(|n| NodeId(*n)).collect::<Vec<_>>();
    let before = replicas(&c);

    // Cut off from the others, the leaver cannot hand anything over.
    c.network().partition(&[&ids(&[2]), &ids(&[0, 1])]);
    c.control(2).leave().unwrap();
    settle(&c);
    let s = c.control(2).status().unwrap();
    assert!(s.filesystem.is_some());
    assert_eq!(
        s.members.iter().find(|m| m.is_self).unwrap().state,
        NodeState::Leaving
    );
    assert_eq!(replicas(&c)[2], before[2], "gave up its data too early");

    c.network().heal();
    settle(&c);
    assert_eq!(c.control(2).status().unwrap().filesystem, None);
    assert_eq!(
        replicas(&c)[..2].iter().sum::<u64>(),
        before.iter().sum::<u64>()
    );
}

#[test]
fn another_member_can_remove_a_dead_node_and_its_data_is_re_replicated() {
    let c = cluster(2, 4);
    write_files(&c, 24);
    let before = replicas(&c);
    c.crash(3);

    // An online member cannot be removed, only asked to leave.
    assert!(c.control(0).remove(MemberId(1)).is_err());

    // No need to wait out the grace period.
    c.advance(2 * MINUTE);
    c.sync();
    c.control(0).remove(MemberId(3)).unwrap();
    settle(&c);

    let after = replicas(&c);
    assert_eq!(
        after[..3].iter().sum::<u64>(),
        before.iter().sum::<u64>(),
        "{after:?}"
    );
    for i in 0..3 {
        let s = c.control(i).status().unwrap();
        assert_eq!(s.members.len(), 3, "node {i}");
        assert!(s.unreplicated.is_empty());
    }
}

#[test]
fn a_rebuilt_machine_rejoins_with_a_new_token_and_pulls_metadata_and_chunks() {
    let c = cluster(2, 4);
    write_files(&c, 24);
    let before = replicas(&c);

    c.rebuild(3);
    // Its old identity is gone; the blank machine needs a fresh token.
    assert_eq!(c.control(3).status().unwrap().filesystem, None);
    let token = c.control(0).new_token().unwrap();
    c.control(3).join(&token, Some(BIG)).unwrap();
    settle(&c);

    for i in 0..24 {
        let ino = c.fs(3).lookup(ROOT_INO, &format!("f{i}")).unwrap().ino;
        assert_eq!(
            c.fs(3).read(ino, 0, u32::MAX).unwrap(),
            noise(8 * 1024, i + 1)
        );
    }
    let after = replicas(&c);
    assert!(after[3] > 0, "{after:?}");
    assert_eq!(
        after.iter().sum::<u64>(),
        before.iter().sum::<u64>(),
        "{after:?}"
    );
    // The old record of the machine is replaced by the new one.
    for i in 0..4 {
        let s = c.control(i).status().unwrap();
        assert_eq!(s.members.len(), 4, "node {i}");
        assert!(s.unreplicated.is_empty());
    }
}
// NEXT
