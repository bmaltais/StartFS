//! A write commit cuts, hashes, compresses and stores its chunks without the node lock, so
//! the daemon's tick and other operations do not wait for it (a linker's writes held the lock
//! for 77 s of an 81 s link and the tick of the writing member fell behind its peers).

use startfs_core::clock::SystemClock;
use startfs_core::fs::{LocalFs, NodeConfig};
use startfs_core::members::MemberId;
use startfs_core::ops::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[test]
fn a_commit_does_not_hold_the_node_lock_while_it_cuts_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let node = NodeConfig {
        node: MemberId(1),
        uid: 1000,
        gid: 1000,
    };
    let fs = Arc::new(LocalFs::open(dir.path(), node, 1 << 20, Arc::new(SystemClock)).unwrap());
    let ino = fs.create(ROOT_INO, "big", 0o644).unwrap().ino;
    let done = Arc::new(AtomicBool::new(false));
    let prober = {
        let (fs, done) = (fs.clone(), done.clone());
        std::thread::spawn(move || {
            let mut worst = Duration::ZERO;
            while !done.load(Ordering::SeqCst) {
                let t = Instant::now();
                fs.frontier().unwrap();
                worst = worst.max(t.elapsed());
                std::thread::sleep(Duration::from_micros(200));
            }
            worst
        })
    };
    // Incompressible bytes, so every commit cuts, hashes and tries to compress all of them.
    let mut x = 0x2545_f491_4f6c_dd1du64;
    let data: Vec<u8> = (0..8 << 20)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect();
    let mut commits = Vec::new();
    for i in 0..12u64 {
        fs.write(ino, i * data.len() as u64, &data).unwrap();
        let t = Instant::now();
        fs.flush(ino).unwrap();
        commits.push(t.elapsed());
    }
    done.store(true, Ordering::SeqCst);
    let worst = prober.join().unwrap();
    commits.sort();
    let typical = commits[commits.len() / 2];
    assert!(
        worst < typical / 4,
        "the node lock was held up to {worst:?} while a commit took {typical:?}"
    );
}
