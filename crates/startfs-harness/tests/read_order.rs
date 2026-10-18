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
