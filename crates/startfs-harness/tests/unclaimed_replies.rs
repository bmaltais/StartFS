//! Chunk replies nobody asks for are dropped after a while, and what is kept is bounded (#68).

use startfs_core::activity::Structure;
use startfs_core::net::{NodeId, Transport};
use startfs_core::ops::*;
use startfs_core::protocol::Message;
use startfs_harness::{Cluster, noise};
use std::time::Duration;

fn unclaimed(c: &Cluster, node: usize) -> Structure {
    c.control(node)
        .activity()
        .unwrap()
        .structures
        .chunk_replies
        .unwrap()
}

fn send_reply(c: &Cluster, id: u8, len: usize) {
    c.transport(0).send(
        NodeId(1),
        Message::ChunkReply([id; 32], Some(vec![id; len])).encode(),
    );
}

/// Let `by` pass in steps short enough that the peers never look asleep.
fn step(c: &Cluster, by: Duration) {
    for _ in 0..by.as_secs() / 10 {
        c.advance(Duration::from_secs(10));
        c.sync();
    }
}

#[test]
fn a_reply_nobody_asked_for_is_dropped_after_a_while() {
    let c = Cluster::with_filesystem(2, 2, "");
    send_reply(&c, 1, 10_000);
    c.sync();
    assert_eq!(unclaimed(&c, 1).entries, 1);

    step(&c, Duration::from_secs(20));
    assert_eq!(unclaimed(&c, 1).entries, 1, "dropped too early");

    step(&c, Duration::from_secs(120));
    let left = unclaimed(&c, 1);
    assert_eq!((left.entries, left.bytes), (0, Some(0)));
}

#[test]
fn the_unclaimed_replies_are_capped_in_bytes_and_the_oldest_go_first() {
    let c = Cluster::with_filesystem(2, 2, "");
    for id in 0..40u8 {
        send_reply(&c, id, 1 << 20);
        c.advance(Duration::from_millis(10));
        c.sync();
    }
    let kept = unclaimed(&c, 1);
    let limit = kept.limit_bytes.expect("the cap is shown");
    assert!(kept.bytes.unwrap() <= limit, "{kept:?}");
    assert!(kept.entries > 0 && kept.entries < 40, "{kept:?}");
}

#[test]
fn a_reply_arriving_after_its_fetch_timed_out_is_dropped_not_kept() {
    let c = Cluster::with_filesystem(1, 2, "");
    let f = c.fs(0).create(ROOT_INO, "f", 0o644).unwrap();
    c.fs(0).write(f.ino, 0, &noise(64 * 1024, 7)).unwrap();
    c.fs(0).flush(f.ino).unwrap();
    c.sync();
    let before = unclaimed(&c, 1).entries;

    // Node 1 reads while the link to node 0 is slow: the fetch gives up first.
    c.network()
        .set_delay(NodeId(0), NodeId(1), Duration::from_secs(10));
    let ino = c.fs(1).lookup(ROOT_INO, "f").unwrap().ino;
    let read = c.fs(1).read(ino, 0, 64 * 1024);
    if read.is_ok() {
        // The chunk was already local: nothing to test here.
        return;
    }
    step(&c, Duration::from_secs(30));
    assert!(
        unclaimed(&c, 1).entries > before,
        "the late reply should sit here first"
    );

    step(&c, Duration::from_secs(120));
    assert_eq!(unclaimed(&c, 1).entries, before);
}

#[test]
fn a_fetch_in_time_still_gets_its_reply() {
    let c = Cluster::with_filesystem(1, 2, "");
    let data = noise(64 * 1024, 9);
    let f = c.fs(0).create(ROOT_INO, "f", 0o644).unwrap();
    c.fs(0).write(f.ino, 0, &data).unwrap();
    c.fs(0).flush(f.ino).unwrap();
    c.sync();
    let ino = c.fs(1).lookup(ROOT_INO, "f").unwrap().ino;
    assert_eq!(c.fs(1).read(ino, 0, data.len() as u32).unwrap(), data);
}
