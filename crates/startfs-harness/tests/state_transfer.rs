//! A node that missed a log compaction is caught up with the sender's merge
//! state, sent in bounded pieces, on a simulated cluster with a manual clock.

use startfs_core::net::NodeId;
use startfs_core::ops::*;
use startfs_core::protocol::Message;
use startfs_harness::Cluster;
use std::time::Duration;

const BIG: u64 = 10 << 30;
const HOUR: Duration = Duration::from_secs(3600);
const CHUNK_BYTES: usize = 2048;
const FILES: usize = 300;

/// Three nodes where node 2 is cut off while the others create many files
/// and compact their logs, so that node 2 can only be caught up with state.
fn island_behind_a_compaction() -> Cluster {
    let c = Cluster::start_with_config(3, &format!("state_chunk_bytes = {CHUNK_BYTES}"));
    let token = c.control(0).create("home", 2).unwrap();
    for i in 1..3 {
        c.control(i).join(&token, Some(BIG)).unwrap();
    }
    c.sync();
    c.network()
        .partition(&[&[NodeId(0), NodeId(1)], &[NodeId(2)]]);
    for i in 0..FILES {
        c.fs(0)
            .create(ROOT_INO, &format!("file-{i:03}"), 0o644)
            .unwrap();
    }
    for _ in 0..6 {
        c.advance(HOUR / 2);
        c.sync();
    }
    assert!(c.control(0).status().unwrap().log.entries < 3);
    c
}

fn names(fs: &impl FsOps) -> Vec<String> {
    let mut v: Vec<String> = fs
        .readdir(ROOT_INO)
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    v.sort();
    v
}

fn settle(c: &Cluster) {
    for _ in 0..12 {
        c.advance(Duration::from_secs(600));
        c.sync();
    }
}

fn assert_converged(c: &Cluster) {
    let want = names(&c.fs(0));
    assert_eq!(want.len(), FILES);
    for i in 1..3 {
        assert_eq!(names(&c.fs(i)), want, "node {i}");
    }
}

/// What the cluster sent to node 2 so far, decoded.
fn sent_to_island(c: &Cluster) -> Vec<(NodeId, usize, Message)> {
    c.network()
        .traffic()
        .into_iter()
        .filter(|(_, to, _)| *to == NodeId(2))
        .map(|(from, _, raw)| (from, raw.len(), Message::decode(&raw).unwrap()))
        .collect()
}

#[test]
fn a_node_behind_a_compaction_is_caught_up_in_bounded_messages() {
    let c = island_behind_a_compaction();
    c.network().heal();
    settle(&c);
    assert_converged(&c);

    let chunks: Vec<usize> = sent_to_island(&c)
        .into_iter()
        .filter(|(_, _, m)| matches!(m, Message::StateChunk(_)))
        .map(|(_, len, _)| len)
        .collect();
    assert!(chunks.len() >= 10, "only {} chunks", chunks.len());
    // The bound covers the objects; the pinned frontier and framing add a little.
    assert!(
        chunks.iter().all(|len| *len <= CHUNK_BYTES + 256),
        "{chunks:?}"
    );
}

#[test]
fn a_partition_in_the_middle_of_a_transfer_loses_nothing() {
    let c = island_behind_a_compaction();
    c.network().heal();
    let chunks_sent = |c: &Cluster| {
        sent_to_island(c)
            .iter()
            .filter(|(_, _, m)| matches!(m, Message::StateChunk(_)))
            .count()
    };
    for _ in 0..200 {
        if chunks_sent(&c) >= 3 {
            break;
        }
        c.advance(Duration::from_secs(1));
        c.tick();
    }
    assert!(chunks_sent(&c) >= 3, "the transfer never started");

    c.network()
        .partition(&[&[NodeId(0), NodeId(1)], &[NodeId(2)]]);
    for _ in 0..30 {
        c.advance(Duration::from_secs(1));
        c.tick();
    }
    assert_ne!(
        names(&c.fs(2)).len(),
        FILES,
        "the transfer had already ended"
    );

    c.network().heal();
    settle(&c);
    assert_converged(&c);
}

#[test]
fn hellos_keep_flowing_while_a_transfer_is_in_progress() {
    let c = island_behind_a_compaction();
    c.network().heal();
    for _ in 0..200 {
        c.advance(Duration::from_secs(1));
        c.tick();
    }
    settle(&c);

    let from_sender: Vec<Message> = sent_to_island(&c)
        .into_iter()
        .filter(|(from, _, _)| *from == NodeId(0))
        .map(|(_, _, m)| m)
        .collect();
    let first = from_sender
        .iter()
        .position(|m| matches!(m, Message::StateChunk(_)))
        .expect("a transfer happened");
    let last = from_sender
        .iter()
        .rposition(|m| matches!(m, Message::StateChunk(_)))
        .unwrap();
    assert!(
        from_sender[first..last]
            .iter()
            .any(|m| matches!(m, Message::Hello(_))),
        "no Hello between the first and last chunk"
    );
}
