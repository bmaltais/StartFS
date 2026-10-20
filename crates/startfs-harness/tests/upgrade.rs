//! Upgrades the cluster coordinates itself (#127, ADR 0007): the members stage a
//! release, then restart one at a time (same protocol) or all together on one
//! go-signal (different protocol). Tests start from `startfs update` on one member.

use startfs_core::control::{NodeState, Verdict};
use startfs_core::ops::*;
use startfs_harness::{Cluster, noise};
use std::time::Duration;

const MINUTE: Duration = Duration::from_secs(60);
const HOUR: Duration = Duration::from_secs(3600);

/// A release newer than any the harness nodes run, so the tests outlive version bumps.
const NEXT: &str = "v99.1.0";

fn write_files(c: &Cluster, count: u64) {
    for i in 0..count {
        let f = c.fs(0).create(ROOT_INO, &format!("f{i}"), 0o644).unwrap();
        c.fs(0).write(f.ino, 0, &noise(8 * 1024, i + 1)).unwrap();
    }
    c.sync();
}

#[test]
fn a_rolling_upgrade_restarts_the_members_one_at_a_time() {
    let c = Cluster::with_filesystem(2, 3, "");
    write_files(&c, 12);
    c.publish(NEXT);

    let report = c.control(1).update(None).unwrap();
    assert_eq!(report.release, NEXT);
    assert!(!report.already_on_it);
    c.elapse(2 * HOUR);

    for i in 0..3 {
        assert_eq!(c.release_of(i), NEXT, "node {i}");
    }
    let restarts = c.restarts();
    assert_eq!(
        restarts.iter().map(|r| r.node).collect::<Vec<_>>(),
        [0, 1, 2],
        "{restarts:?}"
    );
    for pair in restarts.windows(2) {
        assert!(
            pair[0].back.is_some_and(|back| back <= pair[1].from),
            "{restarts:?}"
        );
    }
}

/// Let time pass in steps of ten seconds until `done`, for at most two hours.
fn until(c: &Cluster, mut done: impl FnMut() -> bool) {
    for _ in 0..720 {
        if done() {
            return;
        }
        c.advance(Duration::from_secs(10));
        c.sync();
    }
    panic!("the cluster never got there");
}

/// Node `i` is down for an upgrade right now.
fn is_down_for_upgrade(c: &Cluster, i: usize) -> bool {
    c.restarts()
        .iter()
        .any(|r| r.node == i && r.back.is_none())
}

/// Chunks held as replicas on each node.
fn replicas(c: &Cluster) -> Vec<u64> {
    (0..c.node_count())
        .map(|i| c.control(i).status().map_or(0, |s| s.usage.chunks))
        .collect()
}

#[test]
fn a_member_in_its_upgrade_pause_leaves_the_verdict_alone_and_gets_no_stand_in_copies() {
    let c = Cluster::with_filesystem(2, 3, "");
    write_files(&c, 24);
    c.publish(NEXT);
    c.set_restart_takes(4 * MINUTE);
    c.control(0).update(None).unwrap();

    until(&c, || is_down_for_upgrade(&c, 0));
    let before = replicas(&c);
    for _ in 0..4 {
        c.advance(Duration::from_secs(45));
        c.sync();
    }

    assert!(is_down_for_upgrade(&c, 0), "the pause test needs it down");
    for viewer in [1, 2] {
        let s = c.control(viewer).status().unwrap();
        assert_eq!(s.verdict, Some(Verdict::Safe), "node {viewer}");
        assert_eq!(s.members[0].state, NodeState::Upgrading, "node {viewer}");
    }
    assert_eq!(&replicas(&c)[1..], &before[1..], "no stand-in copies");
}

#[test]
fn a_member_that_is_not_back_when_its_pause_ends_is_just_asleep() {
    let c = Cluster::with_filesystem(2, 3, "");
    write_files(&c, 24);
    c.publish(NEXT);
    c.set_restart_takes(HOUR);
    c.control(0).update(None).unwrap();

    until(&c, || is_down_for_upgrade(&c, 0));
    c.elapse(10 * MINUTE);

    let s = c.control(1).status().unwrap();
    assert_eq!(s.members[0].state, NodeState::Down);
    assert!(s.members[0].standin_secs_left.is_some(), "covered for");
}
