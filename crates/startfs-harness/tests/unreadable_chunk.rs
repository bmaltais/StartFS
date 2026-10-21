//! A read that cannot get a chunk for a passing reason is not reported as damage (#158).

use startfs_core::net::NodeId;
use startfs_core::ops::*;
use startfs_harness::{Cluster, noise};

const FILE: usize = 3 << 20;

#[test]
fn a_chunk_that_cannot_be_opened_is_not_reported_as_corrupt() {
    let c = Cluster::with_offers(2, &[1 << 30, 1 << 30]);
    let f = c.fs(0).create(ROOT_INO, "big", 0o644).unwrap();
    let data = noise(FILE, 9);
    c.fs(0).write(f.ino, 0, &data).unwrap();
    c.fs(0).flush(f.ino).unwrap();
    c.sync();

    // The copies are fine, but this node cannot open its own and cannot reach the other.
    c.network().partition(&[&[NodeId(0)], &[NodeId(1)]]);
    c.set_chunks_readable(0, false);
    let err = c.fs(0).read(f.ino, 0, 16).unwrap_err();
    assert_ne!(err.errno(), 5, "reported as EIO: {err:?}");

    c.set_chunks_readable(0, true);
    c.network().heal();
    assert_eq!(c.fs(0).read(f.ino, 0, 16), Ok(data[..16].to_vec()));
}
