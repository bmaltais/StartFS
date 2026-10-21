//! A commit of one file's buffered writes that is waiting on a peer holds up nothing else.

use startfs_core::net::NodeId;
use startfs_core::ops::*;
use startfs_harness::{Cluster, noise};
use std::time::Duration;

const FILE: usize = 4 << 20;

/// Node 0 offers no space, so it holds no chunk; node 1 holds the file.
/// Node 0 then writes into the start of the file and loses node 1, so the
/// commit of that write cannot get the chunk it must re-read.
fn stuck_commit() -> (Cluster, Ino, Ino) {
    let c = Cluster::with_offers(2, &[0, 1 << 30]);
    let f = c.fs(1).create(ROOT_INO, "big", 0o644).unwrap();
    c.fs(1).write(f.ino, 0, &noise(FILE, 3)).unwrap();
    c.fs(1).create(ROOT_INO, "other", 0o644).unwrap();
    c.sync();
    let big = c.fs(0).lookup(ROOT_INO, "big").unwrap().ino;
    let other = c.fs(0).lookup(ROOT_INO, "other").unwrap().ino;
    c.fs(0).write(big, 0, b"patched").unwrap();
    c.network().partition(&[&[NodeId(0)], &[NodeId(1)]]);
    (c, big, other)
}

#[test]
fn a_commit_waiting_on_a_peer_does_not_stop_other_files() {
    let (c, big, other) = stuck_commit();
    let started = c.now();
    let fs = c.fs(0);
    let committing = std::thread::spawn(move || fs.flush(big));

    // Once the commit has been waiting a while on simulated time, everything
    // else must still answer without waiting for it.
    let waited = Duration::from_millis(20);
    let wall = std::time::Instant::now();
    while c.now().saturating_sub(started) < waited.as_nanos() as u64 {
        assert!(
            wall.elapsed() < Duration::from_secs(30),
            "the commit never waited"
        );
        std::thread::yield_now();
    }
    let before = c.now();
    c.fs(0).getattr(other).unwrap();
    c.fs(0).lookup(ROOT_INO, "other").unwrap();
    c.fs(0).write(other, 0, b"x").unwrap();
    let took = c.now().saturating_sub(before);
    assert!(
        took < Duration::from_millis(50).as_nanos() as u64,
        "other files waited {} ms for a commit",
        took / 1_000_000
    );
    assert_eq!(c.fs(0).getattr(big).unwrap().size, FILE as u64);

    // The commit fails and keeps the write; it goes through once the peer is back.
    assert_eq!(committing.join().unwrap(), Err(FsError::TimedOut));
    c.network().heal();
    c.fs(0).flush(big).unwrap();
    assert_eq!(c.fs(0).read(big, 0, 7), Ok(b"patched".to_vec()));
}
