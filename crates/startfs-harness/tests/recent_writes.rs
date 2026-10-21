//! A file just written can be rewritten without asking a peer for its chunks.

use startfs_core::net::NodeId;
use startfs_core::ops::*;
use startfs_harness::{Cluster, noise};

const FILE: usize = 4 << 20;

/// Node 0 offers no space, so after replication it holds none of the file's chunks as replicas.
#[test]
fn a_rewrite_of_released_chunks_needs_no_peer() {
    let c = Cluster::with_offers(2, &[0, 1 << 30]);
    let f = c.fs(0).create(ROOT_INO, "big", 0o644).unwrap();
    c.fs(0).write(f.ino, 0, &noise(FILE, 5)).unwrap();
    c.fs(0).flush(f.ino).unwrap();
    c.sync();

    c.network().partition(&[&[NodeId(0)], &[NodeId(1)]]);
    c.fs(0).write(f.ino, 0, b"patched").unwrap();
    c.fs(0).flush(f.ino).unwrap();
    assert_eq!(c.fs(0).read(f.ino, 0, 7), Ok(b"patched".to_vec()));
}
