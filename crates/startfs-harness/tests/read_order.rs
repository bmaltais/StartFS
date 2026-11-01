//! Which peer a read asks first for a chunk it lacks.

use startfs_core::net::NodeId;
use startfs_core::ops::*;
use startfs_core::protocol::Message;
use startfs_harness::{Cluster, noise};
use std::time::Duration;

const READER: NodeId = NodeId(0);

/// Node 0 offers no space, so it holds no chunk and every read goes to a peer.
/// Every other node holds every chunk.
fn cluster_with_files(files: u64) -> Cluster {
    let c = Cluster::with_offers(3, &[0, 1 << 30, 1 << 30, 1 << 30]);
    for i in 0..files {
        let f = c.fs(1).create(ROOT_INO, &format!("f{i}"), 0o644).unwrap();
        c.fs(1).write(f.ino, 0, &noise(8 * 1024, i + 1)).unwrap();
    }
    c.sync();
    c
}

fn read_file(c: &Cluster, i: u64) {
    let ino = c.fs(0).lookup(ROOT_INO, &format!("f{i}")).unwrap().ino;
    assert_eq!(
        c.fs(0).read(ino, 0, u32::MAX).unwrap(),
        noise(8 * 1024, i + 1)
    );
}

/// Peers the reader has sent a chunk request to so far, in order.
fn asked(c: &Cluster) -> Vec<NodeId> {
    c.network()
        .traffic()
        .into_iter()
        .filter(|(from, _, raw)| {
            *from == READER && matches!(Message::decode(raw), Ok(Message::ChunkRequest(_)))
        })
        .map(|(_, to, _)| to)
        .collect()
}

#[test]
fn a_read_asks_the_peer_that_answers_fastest_once_it_has_timed_them_all() {
    let c = cluster_with_files(8);
    c.network()
        .set_delay(READER, NodeId(1), Duration::from_millis(80));
    c.network()
        .set_delay(READER, NodeId(2), Duration::from_millis(5));
    c.network()
        .set_delay(READER, NodeId(3), Duration::from_millis(30));

    // The first reads time the peers one by one.
    for i in 0..3 {
        read_file(&c, i);
    }
    let timing = asked(&c).len();

    for i in 3..8 {
        read_file(&c, i);
    }
    assert_eq!(asked(&c)[timing..], [NodeId(2); 5]);
}

#[test]
fn a_peer_that_was_slow_is_tried_again_once_the_reader_forgets_it() {
    let c = cluster_with_files(12);
    c.network()
        .set_delay(READER, NodeId(1), Duration::from_millis(80));
    c.network()
        .set_delay(READER, NodeId(2), Duration::from_millis(5));
    c.network()
        .set_delay(READER, NodeId(3), Duration::from_millis(30));
    for i in 0..4 {
        read_file(&c, i);
    }

    // The slow link is repaired, but until the estimate goes stale nothing tells the reader.
    c.network().set_delay(READER, NodeId(1), Duration::ZERO);
    let before = asked(&c).len();
    read_file(&c, 4);
    assert_eq!(asked(&c)[before..], [NodeId(2)]);

    c.elapse(Duration::from_secs(15 * 60));
    let before = asked(&c).len();
    read_file(&c, 5);
    assert_eq!(asked(&c)[before..], [NodeId(1)]);
}

/// How long, on the simulated clock, one read takes.
fn read_time(c: &Cluster, i: u64) -> Duration {
    let start = c.now();
    read_file(c, i);
    Duration::from_nanos(c.now() - start)
}

/// Time the three peers (5, 30 and 30 ms away), then cut the nearest one off.
fn cluster_with_silent_nearest(files: u64) -> Cluster {
    let c = cluster_with_files(files);
    for (peer, ms) in [(1, 5), (2, 30), (3, 30)] {
        c.network()
            .set_delay(READER, NodeId(peer), Duration::from_millis(ms));
    }
    for i in 0..3 {
        read_file(&c, i);
    }
    c.network()
        .set_delay(READER, NodeId(1), Duration::from_secs(3600));
    c
}

#[test]
fn a_read_does_not_wait_out_a_silent_peer_before_asking_another() {
    let c = cluster_with_silent_nearest(8);
    let took = read_time(&c, 3);
    // The silent peer gets 20 ms, then a 30 ms peer answers; without hedging the read waits out
    // the daemon's whole reply wait (300 ms of simulated time) first.
    assert!(took < Duration::from_millis(150), "read took {took:?}");
}

#[test]
fn a_peer_that_let_a_read_down_is_asked_later_than_one_that_answers() {
    let c = cluster_with_silent_nearest(8);
    for i in 3..6 {
        read_file(&c, i);
    }
    let before = asked(&c).len();
    read_file(&c, 6);
    assert_ne!(asked(&c)[before..][0], NodeId(1));
}

#[test]
fn a_read_does_not_ask_a_member_that_is_down_while_others_are_up() {
    // One copy each; node 3 is cut off before the files are written, so placement would give
    // some of them to it but their copy is on a member that is up.
    let c = Cluster::with_offers(1, &[0, 1 << 30, 1 << 30, 1 << 30]);
    c.network()
        .partition(&[&[READER, NodeId(1), NodeId(2)], &[NodeId(3)]]);
    c.elapse(Duration::from_secs(10 * 60));
    for i in 0..12 {
        let f = c.fs(1).create(ROOT_INO, &format!("f{i}"), 0o644).unwrap();
        c.fs(1).write(f.ino, 0, &noise(8 * 1024, i + 1)).unwrap();
    }
    c.sync();
    // A request to node 3 is dropped by the partition, so it shows as time waited.
    for i in 0..12 {
        let took = read_time(&c, i);
        assert!(took < Duration::from_millis(100), "read {i} took {took:?}");
    }
}
