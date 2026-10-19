//! A member that restarts or comes back must not make every other node re-ask about every
//! chunk (#82): only chunks whose placement or cover changed are checked again, and chunks that
//! are merely being asked about are not reported as short of copies.

use startfs_core::net::NodeId;
use startfs_core::ops::*;
use startfs_core::protocol::Message;
use startfs_harness::{Cluster, noise};
use std::time::Duration;

const FILES: u64 = 60;

fn healthy_cluster(copies: u32, nodes: usize) -> Cluster {
    let c = Cluster::with_filesystem(copies, nodes, "");
    for i in 0..FILES {
        let f = c.fs(0).create(ROOT_INO, &format!("f{i}"), 0o644).unwrap();
        c.fs(0).write(f.ino, 0, &noise(8 * 1024, i + 1)).unwrap();
    }
    c.sync();
    c.elapse(Duration::from_secs(60));
    c
}

/// Let `secs` pass in steps of 10 s, so every node keeps hearing the ones it can reach.
fn pass(c: &Cluster, secs: u64) {
    for _ in 0..secs / 10 {
        c.advance(Duration::from_secs(10));
        c.sync();
    }
}

/// Questions ("do you hold this chunk?") sent so far by nodes other than `except`.
fn questions_by_others(c: &Cluster, except: usize) -> usize {
    c.network()
        .traffic()
        .iter()
        .filter(|(from, _, _)| *from != NodeId(except as u32))
        .filter(|(_, _, raw)| matches!(Message::decode(raw), Ok(Message::ChunkPush(_, None))))
        .count()
}

#[test]
fn a_restarted_member_does_not_make_the_others_ask_about_every_chunk() {
    let c = healthy_cluster(3, 5);
    let before = questions_by_others(&c, 4);
    c.crash(4);
    pass(&c, 60);
    c.restart(4);
    pass(&c, 120);
    let asked = questions_by_others(&c, 4) - before;
    // Every chunk is held on 3 of 5 nodes; a member that is merely down changes no placement.
    assert!(asked <= FILES as usize / 2, "{asked} questions");
}

#[test]
fn a_member_cut_off_and_back_does_not_make_the_others_ask_about_every_chunk() {
    let c = healthy_cluster(3, 5);
    let before = questions_by_others(&c, 4);
    c.network().partition(&[&[NodeId(4)]]);
    pass(&c, 240);
    c.network().heal();
    pass(&c, 120);
    let asked = questions_by_others(&c, 4) - before;
    assert!(asked <= FILES as usize / 2, "{asked} questions");
}

#[test]
fn chunks_still_being_asked_about_after_a_restart_are_not_reported_as_short_of_copies() {
    let c = healthy_cluster(3, 5);
    c.crash(2);
    pass(&c, 30);
    c.restart(2);
    // Message by message, as the restarted node learns where its chunks are held.
    let mut seen = Vec::new();
    for _ in 0..40 {
        c.advance(Duration::from_millis(250));
        c.tick();
        let s = c.control(2).status().unwrap();
        seen.push((s.verdict, s.unreplicated.len(), s.repair, s.rechecking));
    }
    assert!(
        seen.iter().any(|s| s.3 > 0),
        "never said it was re-checking: {seen:?}"
    );
    assert!(
        seen.iter().all(|s| s.1 == 0 && s.2.is_none()),
        "short of copies while only asking: {seen:?}"
    );
}

fn total_replicas(c: &Cluster) -> u64 {
    (0..c.node_count())
        .map(|i| c.control(i).status().map_or(0, |s| s.usage.chunks))
        .sum()
}

#[test]
fn a_member_lost_for_good_has_its_chunks_made_again_on_the_others() {
    let c = healthy_cluster(3, 5);
    let before = total_replicas(&c);
    c.crash(4);
    // Past the repair grace period (30 minutes), with every node hearing the others.
    pass(&c, 35 * 60);
    let after = total_replicas(&c);
    assert_eq!(after, before, "{after} replicas, were {before}");
    for i in 0..4 {
        let s = c.control(i).status().unwrap();
        assert!(s.unreplicated.is_empty(), "node {i}: {:?}", s.unreplicated);
    }
}

#[test]
fn a_member_that_joins_is_given_the_chunks_placement_now_sends_it() {
    let c = Cluster::start(5);
    let token = c.control(0).create("home", 3).unwrap();
    for i in 1..4 {
        c.control(i).join(&token, Some(10 << 30)).unwrap();
    }
    c.control(0).set_offer(10 << 30).unwrap();
    c.sync();
    for i in 0..FILES {
        let f = c.fs(0).create(ROOT_INO, &format!("f{i}"), 0o644).unwrap();
        c.fs(0).write(f.ino, 0, &noise(8 * 1024, i + 1)).unwrap();
    }
    c.sync();
    pass(&c, 60);
    c.control(4).join(&token, Some(10 << 30)).unwrap();
    pass(&c, 120);
    let held = c.control(4).status().unwrap().usage.chunks;
    assert!(held > 0, "the new member holds nothing");
    for i in 0..5 {
        let s = c.control(i).status().unwrap();
        assert!(s.unreplicated.is_empty(), "node {i}: {:?}", s.unreplicated);
    }
}
