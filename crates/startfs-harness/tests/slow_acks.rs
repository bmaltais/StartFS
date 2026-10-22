//! A peer whose answers are slow, not lost, is not sent the same chunk bytes again while the first
//! copy can still be on its way: a resend lands in the queue of the very peer that is behind (#166).

use startfs_core::net::NodeId;
use startfs_core::ops::*;
use startfs_core::protocol::Message;
use startfs_harness::{Cluster, noise};
use std::time::Duration;

const GIB: u64 = 1 << 30;
const PEER: NodeId = NodeId(1);

fn data_pushes(c: &Cluster) -> usize {
    c.network()
        .traffic()
        .iter()
        .filter(|(from, to, _)| *from == NodeId(0) && *to == PEER)
        .filter(|(_, _, raw)| matches!(Message::decode(raw), Ok(Message::ChunkPush(_, Some(_)))))
        .count()
}

#[test]
fn slow_answers_do_not_make_the_sender_resend_the_bytes() {
    let c = Cluster::with_offers(2, &[GIB, GIB]);
    // Every message to or from the peer takes 8 s, so an ask is answered after 16 s and
    // the chunk's acknowledgement comes 16 s after it was sent.
    c.network()
        .set_delay(NodeId(0), PEER, Duration::from_secs(8));
    c.network()
        .set_delay(PEER, NodeId(0), Duration::from_secs(8));
    let fs = c.fs(0);
    let f = fs.create(ROOT_INO, "big", 0o644).unwrap();
    fs.write(f.ino, 0, &noise(1 << 20, 1)).unwrap();
    fs.flush(f.ino).unwrap();

    for _ in 0..60 {
        c.advance(Duration::from_secs(1));
        c.tick();
    }
    assert!(
        data_pushes(&c) <= 2,
        "the chunk's bytes were sent {} times to a peer that was only slow",
        data_pushes(&c)
    );
}

#[test]
fn bytes_that_were_lost_are_sent_again() {
    let c = Cluster::with_offers(2, &[GIB, GIB]);
    let fs = c.fs(0);
    let f = fs.create(ROOT_INO, "big", 0o644).unwrap();
    fs.write(f.ino, 0, &noise(1 << 20, 2)).unwrap();
    fs.flush(f.ino).unwrap();

    // Everything sent while the link is down is lost.
    c.network().set_drop(NodeId(0), PEER, true);
    for _ in 0..30 {
        c.advance(Duration::from_secs(1));
        c.tick();
    }
    c.network().set_drop(NodeId(0), PEER, false);
    for _ in 0..400 {
        c.advance(Duration::from_secs(1));
        c.tick();
    }
    assert_eq!(c.control(0).status().unwrap().staging.chunks, 0);
    assert_eq!(
        c.fs(1).read(f.ino, 0, 8),
        Ok(noise(1 << 20, 2)[..8].to_vec())
    );
}
