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
