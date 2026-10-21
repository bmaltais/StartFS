//! A commit of buffered writes that cannot get a chunk is retried with growing delays, and the
//! tick that tries it does not wait long for peers.

use startfs_core::net::NodeId;
use startfs_core::ops::*;
use startfs_core::protocol::Message;
use startfs_harness::{Cluster, noise};
use std::time::Duration;

const FILE: usize = 4 << 20;

/// Node 0 holds no chunks and has written into the start of a file node 1 holds, then loses node 1.
fn stuck_commit() -> (Cluster, Ino) {
    stuck_commit_among(2)
}

/// [`stuck_commit`] with `nodes` nodes, all of them out of reach of node 0.
fn stuck_commit_among(nodes: usize) -> (Cluster, Ino) {
    let mut offers = vec![1 << 30; nodes];
    offers[0] = 0;
    let c = Cluster::with_offers(2, &offers);
    let f = c.fs(1).create(ROOT_INO, "big", 0o644).unwrap();
    c.fs(1).write(f.ino, 0, &noise(FILE, 3)).unwrap();
    c.sync();
    let big = c.fs(0).lookup(ROOT_INO, "big").unwrap().ino;
    c.fs(0).write(big, 0, b"patched").unwrap();
    // Requests go out, but no answer comes back for a long time.
    for peer in 1..nodes as u32 {
        c.network()
            .set_delay(NodeId(0), NodeId(peer), Duration::from_secs(3600));
    }
    (c, big)
}

fn chunk_requests(c: &Cluster) -> usize {
    c.network()
        .traffic()
        .into_iter()
        .filter(|(from, _, raw)| {
            *from == NodeId(0) && matches!(Message::decode(raw), Ok(Message::ChunkRequest(_)))
        })
        .count()
}

#[test]
fn a_failing_commit_is_retried_with_growing_delays() {
    let (c, _) = stuck_commit();
    for _ in 0..120 {
        c.advance(Duration::from_secs(1));
        c.tick();
    }
    let tries = chunk_requests(&c);
    // Doubling from a second to a minute: well under one a second, and a few at least.
    assert!((3..=12).contains(&tries), "{tries} fetches in two minutes");
}

#[test]
fn a_commit_goes_through_once_the_peer_is_back() {
    let (c, big) = stuck_commit();
    c.elapse(Duration::from_secs(30));
    c.network().heal();
    c.elapse(Duration::from_secs(120));
    assert_eq!(c.fs(1).read(big, 0, 7), Ok(b"patched".to_vec()));
}

#[test]
fn a_tick_does_not_wait_long_for_a_peer() {
    // Each peer waited on takes 300 ms of simulated time; ten of them would take 2.7 s.
    let (c, _) = stuck_commit_among(10);
    c.advance(Duration::from_secs(2)); // past the commit delay: the tick commits
    let before = c.now();
    c.tick();
    let took = c.now().saturating_sub(before);
    assert!(
        took >= Duration::from_secs(1).as_nanos() as u64,
        "the tick did not try the commit ({} ms)",
        took / 1_000_000
    );
    assert!(
        took <= Duration::from_millis(2300).as_nanos() as u64,
        "a tick took {} ms",
        took / 1_000_000
    );
}

#[test]
fn a_commit_that_peers_did_not_answer_says_so() {
    let (c, big) = stuck_commit();
    assert_eq!(c.fs(0).flush(big), Err(FsError::TimedOut));
}
