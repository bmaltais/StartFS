//! The one-line answer to "is my data safe right now?", driven through the
//! control interface on a simulated cluster.

use startfs_core::control::{NodeState, Verdict};
use startfs_core::net::NodeId;
use startfs_core::ops::*;
use startfs_core::sync::HELLO_INTERVAL;
use startfs_harness::{Cluster, noise};
use std::time::Duration;

const BIG: u64 = 10 << 30;

fn write(c: &Cluster, node: usize) {
    let fs = c.fs(node);
    let f = fs.create(ROOT_INO, "f", 0o644).unwrap();
    fs.write(f.ino, 0, &noise(60_000, 7)).unwrap();
}

fn verdict(c: &Cluster, node: usize) -> Option<Verdict> {
    c.control(node).status().unwrap().verdict
}

#[test]
fn there_is_no_verdict_without_a_filesystem() {
    let c = Cluster::start(1);
    assert_eq!(verdict(&c, 0), None);
}

#[test]
fn fully_replicated_data_is_safe() {
    let c = Cluster::with_offers(2, &[BIG, BIG]);
    write(&c, 0);
    c.sync();
    assert_eq!(verdict(&c, 0), Some(Verdict::Safe));
    assert_eq!(verdict(&c, 1), Some(Verdict::Safe));
}

#[test]
fn a_file_with_a_single_copy_puts_the_data_at_risk_until_replicated() {
    let c = Cluster::with_offers(2, &[BIG, BIG, 0]);
    write(&c, 2);
    assert_eq!(verdict(&c, 2), Some(Verdict::AtRisk));
    c.sync();
    assert_eq!(verdict(&c, 2), Some(Verdict::Safe));
}

#[test]
fn a_silent_node_leaves_the_data_degraded_though_every_file_is_replicated() {
    let c = Cluster::with_offers(2, &[BIG, BIG]);
    write(&c, 0);
    c.sync();
    c.network().partition(&[&[NodeId(0)], &[NodeId(1)]]);
    for _ in 0..4 {
        c.advance(HELLO_INTERVAL);
        c.sync();
    }
    assert_eq!(verdict(&c, 0), Some(Verdict::Degraded));
    c.network().heal();
    c.advance(HELLO_INTERVAL);
    c.sync();
    assert_eq!(verdict(&c, 0), Some(Verdict::Safe));
}

#[test]
fn a_lost_node_whose_chunks_are_re_replicated_leaves_the_data_safe() {
    let c = Cluster::with_filesystem(3, 4, "");
    for i in 0..10 {
        let fs = c.fs(0);
        let f = fs.create(ROOT_INO, &format!("f{i}"), 0o644).unwrap();
        fs.write(f.ino, 0, &noise(60_000, i)).unwrap();
    }
    c.sync();
    c.crash(3);
    c.advance(Duration::from_secs(31 * 60));
    c.sync();
    for _ in 0..600 {
        c.advance(Duration::from_secs(1));
        c.sync();
    }
    let s = c.control(0).status().unwrap();
    assert_eq!(s.members[3].state, NodeState::Lost);
    let seen = s.members[3].last_seen.unwrap();
    assert!(seen.exact && seen.secs_ago >= 31 * 60);
    assert_eq!(s.members[0].last_seen, None);
    assert_eq!(s.verdict, Some(Verdict::Safe));
}

#[test]
fn a_restarted_node_learns_how_long_a_member_has_been_gone_from_the_others() {
    let c = Cluster::with_filesystem(3, 4, "");
    c.sync();
    c.crash(3);
    c.advance(Duration::from_secs(10 * 60));
    c.sync();
    c.crash(0);
    c.restart(0);
    c.advance(HELLO_INTERVAL);
    c.sync();
    let seen = c.control(0).status().unwrap().members[3].last_seen.unwrap();
    assert!(seen.exact, "the others remember what node 0 forgot");
    assert!(seen.secs_ago >= 10 * 60, "{}s", seen.secs_ago);
}

#[test]
fn too_few_storage_nodes_for_n_copies_is_best_effort_not_a_failure() {
    let c = Cluster::with_offers(3, &[BIG, BIG]);
    write(&c, 0);
    c.sync();
    assert_eq!(verdict(&c, 0), Some(Verdict::BestEffort));
}

#[test]
fn chunks_a_sleeping_member_last_reported_as_a_single_copy_put_the_data_at_risk() {
    let c = Cluster::with_offers(2, &[BIG, BIG, 0]);
    write(&c, 2);
    // Its report reaches the others, then it sleeps before they confirm a copy.
    c.advance(HELLO_INTERVAL);
    c.tick();
    c.tick();
    c.crash(2);
    c.elapse(Duration::from_secs(20 * 60));
    let s = c.control(0).status().unwrap();
    assert_eq!(s.members[2].state, NodeState::Down);
    assert_eq!(s.stranded.len(), 1, "{:?}", s.stranded);
    assert_eq!(s.stranded[0].member, s.members[2].id);
    assert!(s.stranded[0].secs_ago >= 20 * 60 - 240);
    assert_eq!(s.verdict, Some(Verdict::AtRisk));
    c.restart(2);
    c.elapse(Duration::from_secs(60));
    let s = c.control(0).status().unwrap();
    assert!(s.stranded.is_empty(), "{:?}", s.stranded);
    assert_eq!(s.verdict, Some(Verdict::Safe));
}

#[test]
fn a_sleeping_member_that_had_replicated_everything_strands_nothing() {
    let c = Cluster::with_offers(2, &[BIG, BIG, 0]);
    write(&c, 2);
    c.sync();
    c.crash(2);
    c.elapse(Duration::from_secs(20 * 60));
    let s = c.control(0).status().unwrap();
    assert!(s.stranded.is_empty(), "{:?}", s.stranded);
    assert_eq!(s.verdict, Some(Verdict::Degraded));
}

#[test]
fn a_restarted_node_learns_from_the_others_what_a_sleeping_member_alone_may_hold() {
    let c = Cluster::with_offers(2, &[BIG, BIG, 0]);
    write(&c, 2);
    c.advance(HELLO_INTERVAL);
    c.tick();
    c.tick();
    c.crash(2);
    c.elapse(Duration::from_secs(20 * 60));
    c.crash(0);
    c.restart(0);
    c.advance(HELLO_INTERVAL);
    c.sync();
    let s = c.control(0).status().unwrap();
    assert_eq!(s.stranded.len(), 1, "{:?}", s.stranded);
    assert!(s.stranded[0].secs_ago >= 20 * 60 - 240);
    assert_eq!(s.verdict, Some(Verdict::AtRisk));
}
