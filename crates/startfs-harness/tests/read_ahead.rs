//! A read of a file this node does not hold fetches the chunks that follow in the same round trip.

use startfs_core::net::NodeId;
use startfs_core::ops::*;
use startfs_core::protocol::Message;
use startfs_harness::{Cluster, noise};

const READER: NodeId = NodeId(0);
const FILE: usize = 16 << 20;
const PIECE: u32 = 128 << 10;

/// Node 0 offers no space, so it holds no chunk; node 1 holds the whole file, which spans
/// several chunks.
fn cluster_with_file() -> (Cluster, Ino) {
    let c = Cluster::with_offers(2, &[0, 1 << 30]);
    let f = c.fs(1).create(ROOT_INO, "big", 0o644).unwrap();
    c.fs(1).write(f.ino, 0, &noise(FILE, 7)).unwrap();
    c.sync();
    let ino = c.fs(0).lookup(ROOT_INO, "big").unwrap().ino;
    (c, ino)
}

fn chunk_requests(c: &Cluster) -> usize {
    c.network()
        .traffic()
        .into_iter()
        .filter(|(from, _, raw)| {
            *from == READER && matches!(Message::decode(raw), Ok(Message::ChunkRequest(_)))
        })
        .count()
}

#[test]
fn a_sequential_read_asks_for_the_next_chunks_along_with_the_first() {
    let (c, ino) = cluster_with_file();
    let want = noise(FILE, 7);
    let first = c.fs(0).read(ino, 0, PIECE).unwrap();
    assert_eq!(first, want[..PIECE as usize]);
    assert!(
        chunk_requests(&c) > 1,
        "only {} chunk requested",
        chunk_requests(&c)
    );

    // Reading on finds what was fetched ahead, and the bytes are the file's.
    let mut got = first;
    while (got.len() as u64) < FILE as u64 {
        got.extend(c.fs(0).read(ino, got.len() as u64, PIECE).unwrap());
    }
    assert!(got == want);
}

#[test]
fn a_read_in_the_middle_of_a_chunk_asks_for_that_chunk_only() {
    let (c, ino) = cluster_with_file();
    let at = 512 << 10;
    let got = c.fs(0).read(ino, at, 4096).unwrap();
    assert_eq!(got, noise(FILE, 7)[at as usize..][..4096]);
    assert_eq!(chunk_requests(&c), 1);
}
