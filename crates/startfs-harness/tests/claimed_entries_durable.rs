//! What a member tells its peers it holds survives a power cut there. Peers drop their own copy
//! of the log below what every member said it holds, so a member that claimed entries a power
//! cut then took back would leave them nowhere (#166: a member applies peers' entries lazily and
//! syncs them in groups).

use startfs_core::members::MemberId;
use startfs_core::net::NodeId;
use startfs_core::ops::*;
use startfs_core::protocol::Message;
use startfs_harness::Cluster;
use std::collections::BTreeMap;
use std::time::Duration;

const RECEIVER: usize = 1;

/// Per origin, the highest sequence number `node` said it holds (or passed on) in `traffic`.
fn claimed_by(traffic: &[(NodeId, NodeId, Vec<u8>)], node: NodeId) -> BTreeMap<MemberId, u64> {
    let mut claimed: BTreeMap<MemberId, u64> = BTreeMap::new();
    let mut raise = |origin: MemberId, next: u64| {
        let c = claimed.entry(origin).or_default();
        *c = (*c).max(next);
    };
    for (_, _, raw) in traffic.iter().filter(|(from, _, _)| *from == node) {
        match Message::decode(raw) {
            Ok(Message::Hello(frontier)) => {
                for (origin, next) in frontier {
                    raise(origin, next);
                }
            }
            Ok(Message::Entries(entries)) => {
                for e in entries {
                    raise(e.origin, e.seq + 1);
                }
            }
            _ => {}
        }
    }
    claimed
}

#[test]
fn what_a_member_claimed_survives_a_power_cut_there() {
    let c = Cluster::start(3);
    let token = c.control(0).create("home", 3).unwrap();
    c.control(1).join(&token, None).unwrap();
    c.control(2).join(&token, None).unwrap();
    c.sync();

    let start = c.network().traffic().len();
    for round in 0..21 {
        for i in 0..5 {
            c.fs(0)
                .create(ROOT_INO, &format!("f{round}-{i}"), 0o644)
                .unwrap();
        }
        // The writer sends; the receiver applies, answers and passes on, a tick at a time. The
        // last round leaves no time for the receiver to make what it applied durable.
        c.tick_one(0);
        if round < 20 {
            c.advance(Duration::from_millis(30));
        }
        c.tick_one(RECEIVER);
    }
    let claimed = claimed_by(&c.network().traffic()[start..], NodeId(RECEIVER as u32));
    assert!(!claimed.is_empty(), "the receiver said what it holds");
    assert!(
        c.fs(RECEIVER).has_unsynced(),
        "the power goes while the receiver holds entries not yet durable"
    );

    c.power_cut(RECEIVER);
    c.restart(RECEIVER);
    let after = c.network().traffic().len();
    // Its first tick says what it holds now; nobody has sent it anything since the restart.
    c.tick_one(RECEIVER);
    let held = c.network().traffic()[after..]
        .iter()
        .filter(|(from, _, _)| *from == NodeId(RECEIVER as u32))
        .find_map(|(_, _, raw)| match Message::decode(raw) {
            Ok(Message::Hello(frontier)) => Some(frontier),
            _ => None,
        })
        .expect("the restarted receiver says what it holds");

    for (origin, next) in &claimed {
        let now = held.get(origin).copied().unwrap_or(0);
        assert!(
            now >= *next,
            "origin {origin:?}: claimed up to {next} before the power cut, holds {now} after"
        );
    }
}
