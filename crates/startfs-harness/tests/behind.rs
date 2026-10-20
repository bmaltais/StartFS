//! A member whose work is backed up is up, but behind: `status` says so, and
//! the others send it nothing new until it catches up. Weights decide how much
//! of the placement a member takes, apart from the space it offers.

use startfs_core::net::NodeId;
use startfs_core::ops::*;
use startfs_core::protocol::Message;
use startfs_harness::{Cluster, noise};
use std::time::Duration;

const G: u64 = 1 << 30;

fn put(fs: &impl FsOps, name: &str, data: &[u8]) {
    let f = fs.create(ROOT_INO, name, 0o644).unwrap();
    assert_eq!(fs.write(f.ino, 0, data).unwrap(), data.len());
}

/// Ten seconds in which node `stuck` only keeps up its liveness traffic.
fn stuck_round(c: &Cluster, stuck: usize) {
    c.advance(Duration::from_secs(10));
    c.tick_except(stuck);
    c.keepalive(stuck);
}

#[test]
fn a_member_whose_tick_is_stuck_stays_up_but_is_reported_behind() {
    let c = Cluster::with_filesystem(2, 3, "");
    for _ in 0..8 {
        stuck_round(&c, 2);
    }

    let s = c.control(0).status().unwrap();
    let stuck = &s.members[2];
    assert_eq!(stuck.state, startfs_core::control::NodeState::Online);
    let load = stuck.behind.expect("reported behind");
    assert!(load.tick_secs_ago >= 60, "{load:?}");
    assert!(load.inbox_messages > 0, "{load:?}");
    assert!(s.members[1].behind.is_none());
    assert!(
        s.warnings.iter().any(|w| w.contains("behind")),
        "{:?}",
        s.warnings
    );

    // Once it is working again, the report goes.
    for _ in 0..3 {
        c.advance(Duration::from_secs(10));
        c.sync();
    }
    assert!(c.control(0).status().unwrap().members[2].behind.is_none());
}

#[test]
fn a_member_with_a_deep_inbox_is_sent_nothing_new_until_it_drains() {
    let c = Cluster::with_filesystem(2, 3, "behind_inbox_messages = 4");
    for _ in 0..4 {
        stuck_round(&c, 2);
    }
    assert!(c.control(0).status().unwrap().members[2].behind.is_some());

    let sent = c.network().traffic().len();
    for i in 0..10 {
        put(&c.fs(0), &format!("f{i}"), &noise(8 * 1024, i + 1));
    }
    for _ in 0..3 {
        stuck_round(&c, 2);
    }
    let pushes = c.network().traffic()[sent..]
        .iter()
        .filter(|(_, to, raw)| {
            *to == NodeId(2) && matches!(Message::decode(raw), Ok(Message::ChunkPush(..)))
        })
        .count();
    assert_eq!(pushes, 0);

    // It drains, and the copies it was spared are made.
    for _ in 0..6 {
        c.advance(Duration::from_secs(10));
        c.sync();
    }
    let s = c.control(0).status().unwrap();
    assert!(s.unreplicated.is_empty(), "{:?}", s.unreplicated);
    assert!(c.control(2).status().unwrap().usage.chunks > 0);
}

#[test]
fn placement_follows_the_weight_and_not_the_offer() {
    let c = Cluster::with_offers(1, &[G, 3 * G, 0]);
    // The big node offers three times the space but takes only as much as the small one.
    c.control(1).set_weight(G).unwrap();
    c.advance(Duration::from_secs(10));
    c.sync();
    for i in 0..200 {
        put(&c.fs(2), &format!("f{i}"), &noise(8 * 1024, i + 1));
    }
    c.sync();

    let held: Vec<u64> = (0..2)
        .map(|i| c.control(i).status().unwrap().usage.chunks)
        .collect();
    assert_eq!(held.iter().sum::<u64>(), 200, "{held:?}");
    // Half each (100 +- 30), where the offers would have given a quarter and three.
    assert!((70..=130).contains(&held[0]), "{held:?}");
}
