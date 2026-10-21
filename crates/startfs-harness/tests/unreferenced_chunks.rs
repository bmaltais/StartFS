//! A chunk no file refers to any more is not replicated: a build that writes and deletes
//! many files must not push the deleted ones to every peer (#156).

use startfs_core::net::NodeId;
use startfs_core::ops::*;
use startfs_core::protocol::Message;
use startfs_harness::{Cluster, noise};
use std::time::Duration;

const FILE: usize = 1 << 20;

fn chunk_pushes(c: &Cluster) -> usize {
    c.network()
        .traffic()
        .iter()
        .filter(|(from, _, raw)| {
            *from == NodeId(0) && matches!(Message::decode(raw), Ok(Message::ChunkPush(_, Some(_))))
        })
        .count()
}

#[test]
fn a_file_deleted_before_replication_is_never_pushed() {
    let c = Cluster::with_offers(2, &[1 << 30, 1 << 30]);
    let fs = c.fs(0);
    let gone = fs.create(ROOT_INO, "gone", 0o644).unwrap();
    fs.write(gone.ino, 0, &noise(FILE, 1)).unwrap();
    fs.flush(gone.ino).unwrap();
    fs.unlink(ROOT_INO, "gone").unwrap();

    c.sync();
    c.elapse(Duration::from_secs(30));

    assert_eq!(chunk_pushes(&c), 0);
    assert_eq!(c.control(0).status().unwrap().staging.chunks, 0);
}

#[test]
fn a_file_that_is_kept_is_still_replicated() {
    let c = Cluster::with_offers(2, &[1 << 30, 1 << 30]);
    let fs = c.fs(0);
    let gone = fs.create(ROOT_INO, "gone", 0o644).unwrap();
    fs.write(gone.ino, 0, &noise(FILE, 1)).unwrap();
    fs.flush(gone.ino).unwrap();
    let kept = fs.create(ROOT_INO, "kept", 0o644).unwrap();
    fs.write(kept.ino, 0, &noise(FILE, 2)).unwrap();
    fs.flush(kept.ino).unwrap();
    fs.unlink(ROOT_INO, "gone").unwrap();

    c.sync();
    c.elapse(Duration::from_secs(30));

    assert!(chunk_pushes(&c) > 0);
    assert_eq!(c.control(0).status().unwrap().staging.chunks, 0);
    assert_eq!(
        c.fs(1).read(kept.ino, 0, 16),
        Ok(noise(FILE, 2)[..16].to_vec())
    );
}
