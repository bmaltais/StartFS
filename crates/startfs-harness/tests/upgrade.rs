//! Upgrades the cluster coordinates itself (#127, ADR 0007): the members stage a
//! release, then restart one at a time (same protocol) or all together on one
//! go-signal (different protocol). Tests start from `startfs update` on one member.

use startfs_core::control::{NodeState, Verdict};
use startfs_core::net::NodeId;
use startfs_core::ops::*;
use startfs_core::protocol::{Message, PROTOCOL_VERSION, UpgradeNotice};
use startfs_core::upgrade::Progress;
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
    until(&c, || all_on(&c, NEXT));

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

/// Every node runs `tag`.
fn all_on(c: &Cluster, tag: &str) -> bool {
    (0..c.node_count()).all(|i| c.release_of(i) == tag)
}

/// Node `i` is down for an upgrade right now.
fn is_down_for_upgrade(c: &Cluster, i: usize) -> bool {
    c.restarts().iter().any(|r| r.node == i && r.back.is_none())
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

#[test]
fn a_release_published_mid_roll_does_not_split_the_cluster() {
    let c = Cluster::with_filesystem(2, 3, "");
    c.publish(NEXT);
    c.control(2).update(None).unwrap();
    c.publish("v99.2.0");
    until(&c, || all_on(&c, NEXT));
    c.elapse(HOUR);

    for i in 0..3 {
        assert_eq!(c.release_of(i), NEXT, "node {i}");
    }
}

#[test]
fn update_says_so_when_the_cluster_is_already_on_the_release_and_does_nothing() {
    let c = Cluster::with_filesystem(2, 3, "");
    c.publish(NEXT);
    c.publish_prerelease("v99.2.0-rc.1");
    // No release means the latest stable one, never a pre-release.
    assert_eq!(c.control(0).update(None).unwrap().release, NEXT);
    until(&c, || all_on(&c, NEXT));
    c.elapse(10 * MINUTE);
    let restarts = c.restarts().len();

    let again = c.control(1).update(Some(NEXT)).unwrap();
    assert!(again.already_on_it);
    let latest = c.control(2).update(None).unwrap();
    assert_eq!(
        (latest.release.as_str(), latest.already_on_it),
        (NEXT, true)
    );
    c.elapse(HOUR);

    assert_eq!(c.restarts().len(), restarts);
}

#[test]
fn update_refuses_what_is_not_a_release_tag() {
    let c = Cluster::with_filesystem(2, 2, "");
    let err = c.control(0).update(Some("1.2")).unwrap_err();
    assert!(err.to_string().contains("not a release tag"), "{err}");
    let err = c.control(0).update(None).unwrap_err();
    assert!(err.to_string().contains("latest release"), "{err}");
}

/// A cluster on the protocol before the current one, with a release that changes it.
fn lockstep_cluster(nodes: usize) -> Cluster {
    let c = Cluster::with_filesystem(2, nodes, "");
    write_files(&c, 12);
    c.set_protocol(PROTOCOL_VERSION - 1);
    c.sync();
    c.publish(NEXT);
    c
}

#[test]
fn a_lockstep_upgrade_swaps_every_member_together_once_all_have_staged() {
    let c = lockstep_cluster(3);
    // Node 2's mount is in use, so it cannot be told to go yet.
    c.set_mount_in_use(2, true);
    c.control(1).update(None).unwrap();
    c.elapse(30 * MINUTE);

    assert!(
        c.restarts().is_empty(),
        "nothing restarts before all are ready"
    );
    for i in 0..3 {
        assert_eq!(c.staged_on(i).as_deref(), Some(NEXT), "node {i}");
    }
    let s = c.control(0).status().unwrap();
    assert_eq!(s.upgrade.as_ref().map(|u| u.go), Some(false));
    assert_eq!(s.members[2].upgrade, Some(Progress::WaitingOnMount));

    c.set_mount_in_use(2, false);
    c.elapse(HOUR);

    let restarts = c.restarts();
    assert_eq!(restarts.len(), 3, "{restarts:?}");
    let first = restarts.iter().map(|r| r.from).min().unwrap();
    let last = restarts.iter().map(|r| r.from).max().unwrap();
    assert!(last - first < 60_000_000_000, "{restarts:?}");
    for i in 0..3 {
        assert_eq!(c.release_of(i), NEXT, "node {i}");
    }
    let s = c.control(0).status().unwrap();
    assert_eq!(s.verdict, Some(Verdict::Safe));
    assert!(s.upgrade.is_some_and(|u| u.complete));
}

#[test]
fn staged_releases_expire_when_the_issuer_disappears_before_the_go_signal() {
    let c = lockstep_cluster(3);
    c.set_mount_in_use(2, true);
    c.control(0).update(None).unwrap();
    c.elapse(10 * MINUTE);
    assert_eq!(c.staged_on(1).as_deref(), Some(NEXT));

    c.crash(0);
    c.elapse(30 * MINUTE);

    assert_eq!(c.staged_on(1), None);
    assert_eq!(c.staged_on(2), None);
    c.set_mount_in_use(2, false);
    c.elapse(HOUR);
    assert!(c.restarts().is_empty(), "{:?}", c.restarts());
    for i in 1..3 {
        assert_eq!(c.control(i).status().unwrap().upgrade, None, "node {i}");
    }
}

#[test]
fn a_member_whose_swap_fails_rolls_back_alone_and_status_shows_the_split() {
    let c = lockstep_cluster(3);
    c.fail_next_swap(2);
    c.control(0).update(None).unwrap();
    c.elapse(2 * HOUR);

    for i in 0..2 {
        assert_eq!(c.release_of(i), NEXT, "node {i}");
    }
    assert_ne!(c.release_of(2), NEXT);
    let s = c.control(0).status().unwrap();
    assert_eq!(s.upgrade.as_ref().map(|u| u.split.len()), Some(1), "{s:?}");
    assert_eq!(s.members[2].protocol, Some(PROTOCOL_VERSION - 1));
    let stuck = c.control(2).status().unwrap();
    assert!(
        matches!(stuck.members[2].upgrade, Some(Progress::Failed(_))),
        "{:?}",
        stuck.members[2].upgrade
    );
}

#[test]
fn a_member_that_cannot_stage_the_release_holds_the_lockstep_upgrade_and_says_why() {
    let c = lockstep_cluster(3);
    c.break_release(2, NEXT, "checksum mismatch");
    c.control(0).update(None).unwrap();
    c.elapse(HOUR);

    assert!(c.restarts().is_empty(), "{:?}", c.restarts());
    let s = c.control(0).status().unwrap();
    assert_eq!(
        s.members[2].upgrade,
        Some(Progress::Failed("checksum mismatch".into()))
    );
}

#[test]
fn an_absent_member_does_not_hold_up_a_lockstep_upgrade_and_catches_up_when_it_returns() {
    let c = lockstep_cluster(3);
    c.crash(2);
    c.control(0).update(None).unwrap();
    c.elapse(HOUR);
    assert_eq!(
        (c.release_of(0), c.release_of(1)),
        (NEXT.into(), NEXT.into())
    );
    assert_ne!(c.release_of(2), NEXT);

    c.restart(2);
    c.elapse(HOUR);

    assert_eq!(c.release_of(2), NEXT);
    assert_eq!(c.control(0).status().unwrap().verdict, Some(Verdict::Safe));
}

#[test]
fn a_notice_moves_a_member_only_up_and_only_when_a_known_member_sent_it() {
    let c = Cluster::with_filesystem(2, 2, "");
    c.publish(NEXT);
    c.publish_with_protocol("v0.0.1", PROTOCOL_VERSION + 1);
    let notice = |release: &str| {
        Message::UpgradeNotice(UpgradeNotice {
            release: release.into(),
            protocol: PROTOCOL_VERSION + 1,
        })
        .encode()
    };

    // From a stranger, however plausible.
    let stranger = c.network().transport(NodeId(99));
    startfs_core::net::Transport::send(&stranger, NodeId(0), notice(NEXT));
    c.elapse(30 * MINUTE);
    assert_eq!(c.staged_on(0), None);
    assert!(c.restarts().is_empty());

    // From a member, but for a release earlier than the one it runs.
    let member = c.transport(1);
    startfs_core::net::Transport::send(&member, NodeId(0), notice("v0.0.1"));
    c.elapse(30 * MINUTE);
    assert_eq!(c.staged_on(0), None);
    assert!(c.restarts().is_empty());
}

/// A notice from a member for a later release on another protocol is followed: the
/// member stages the release and swaps in without anyone ordering it, and then tells
/// the member that sent it, which can no longer hear it, in turn.
#[test]
fn a_notice_from_a_member_for_a_later_release_is_followed() {
    let c = Cluster::with_filesystem(2, 2, "");
    c.publish_with_protocol(NEXT, PROTOCOL_VERSION + 1);
    let notice = Message::UpgradeNotice(UpgradeNotice {
        release: NEXT.into(),
        protocol: PROTOCOL_VERSION + 1,
    })
    .encode();
    startfs_core::net::Transport::send(&c.transport(1), NodeId(0), notice);
    until(&c, || all_on(&c, NEXT));
}

#[test]
fn a_busy_mount_stages_but_waits_for_the_user_and_is_reported() {
    let c = Cluster::with_filesystem(2, 3, "");
    write_files(&c, 12);
    c.publish(NEXT);
    c.set_mount_in_use(1, true);
    c.control(0).update(None).unwrap();
    until(&c, || c.release_of(0) == NEXT && c.release_of(2) == NEXT);
    c.elapse(30 * MINUTE);

    // The others carry on; the busy node is staged and says what it waits for.
    assert_eq!(c.staged_on(1).as_deref(), Some(NEXT));
    assert_eq!(c.restarts().iter().filter(|r| r.node == 1).count(), 0);
    let s = c.control(0).status().unwrap();
    assert_eq!(s.members[1].upgrade, Some(Progress::WaitingOnMount));
    assert!(!s.upgrade.unwrap().complete);

    // The user forces it on that node.
    c.control(1).update_with(None, true).unwrap();
    until(&c, || c.release_of(1) == NEXT);
}

#[test]
fn a_busy_mount_that_is_freed_lets_the_member_restart_without_being_told() {
    let c = Cluster::with_filesystem(2, 2, "");
    c.publish(NEXT);
    c.set_mount_in_use(0, true);
    c.control(1).update(None).unwrap();
    c.elapse(30 * MINUTE);
    assert_ne!(c.release_of(0), NEXT);

    c.set_mount_in_use(0, false);
    until(&c, || all_on(&c, NEXT));
}

#[test]
fn a_member_that_fails_a_rolling_swap_comes_back_on_the_old_release_and_does_not_hold_the_others_up()
 {
    let c = Cluster::with_filesystem(2, 3, "");
    write_files(&c, 12);
    c.publish(NEXT);
    c.fail_next_swap(1);
    c.control(0).update(None).unwrap();
    until(&c, || c.release_of(0) == NEXT && c.release_of(2) == NEXT);
    c.elapse(30 * MINUTE);

    assert_ne!(c.release_of(1), NEXT);
    let s = c.control(0).status().unwrap();
    assert!(
        matches!(s.members[1].upgrade, Some(Progress::Failed(_))),
        "{:?}",
        s.members[1].upgrade
    );
    // It is not tried again by itself.
    assert_eq!(c.restarts().iter().filter(|r| r.node == 1).count(), 1);
}
