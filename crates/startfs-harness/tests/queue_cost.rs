//! A tick's cost does not grow with the length of the replication queue, and a peer is not asked
//! about more chunks than it can answer (#157).

use startfs_core::net::NodeId;
use startfs_core::ops::*;
use startfs_core::protocol::Message;
use startfs_harness::{Cluster, noise};
use std::time::{Duration, Instant};

const GIB: u64 = 1 << 30;
const PEER: NodeId = NodeId(1);

/// A cluster whose node 0 has `queued` chunks to replicate to a peer that never answers.
fn silent_peer(queued: usize) -> Cluster {
    let c = Cluster::with_offers(2, &[GIB, GIB]);
    c.network()
        .set_delay(NodeId(0), PEER, Duration::from_secs(3600));
    let fs = c.fs(0);
    for i in 0..queued {
        let f = fs.create(ROOT_INO, &format!("f{i}"), 0o644).unwrap();
        fs.write(f.ino, 0, &noise(70 << 10, i as u64 + 1)).unwrap();
    }
    c
}

/// The median wall time of a tick once the queue has had a few rounds to be looked at.
fn tick_cost(c: &Cluster) -> Duration {
    for _ in 0..20 {
        c.advance(Duration::from_millis(1100));
        c.tick();
    }
    let mut costs: Vec<Duration> = (0..9)
        .map(|_| {
            c.advance(Duration::from_millis(1100));
            let t = Instant::now();
            c.tick();
            t.elapsed()
        })
        .collect();
    costs.sort();
    costs[costs.len() / 2]
}

#[test]
fn a_long_queue_does_not_make_ticks_longer() {
    let short = tick_cost(&silent_peer(500));
    let long = tick_cost(&silent_peer(8000));
    assert!(
        long < short * 3 + Duration::from_millis(10),
        "a tick took {short:?} with 500 chunks queued and {long:?} with 8000"
    );
}

#[test]
fn a_peer_is_asked_about_no_more_chunks_than_it_can_answer() {
    let c = silent_peer(3000);
    for _ in 0..10 {
        c.advance(Duration::from_millis(1100));
        c.tick();
    }
    let asks = c
        .network()
        .traffic()
        .iter()
        .filter(|(from, to, _)| *from == NodeId(0) && *to == PEER)
        .filter(|(_, _, raw)| matches!(Message::decode(raw), Ok(Message::ChunkPush(..))))
        .count();
    assert!(asks <= 1000, "{asks} asks went to a peer that answers none");
}
