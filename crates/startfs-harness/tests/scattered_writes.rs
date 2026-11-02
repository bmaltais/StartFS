//! A file written in pieces at scattered offsets, as a linker writes its output, is committed in
//! batches like a file written in order: one commit per buffer's worth, not one per piece (a
//! 256 MiB link took over a minute, every piece re-cutting the chunks around it).

use startfs_core::clock::SystemClock;
use startfs_core::fs::{CatchUp, LocalFs, NodeConfig};
use startfs_core::members::MemberId;
use startfs_core::oplog::Op;
use startfs_core::ops::*;
use startfs_harness::{Cluster, noise};
use std::path::Path;
use std::sync::Arc;

fn open(dir: &Path) -> LocalFs {
    let node = NodeConfig {
        node: MemberId(1),
        uid: 1000,
        gid: 1000,
    };
    LocalFs::open(dir, node, 1 << 20, Arc::new(SystemClock)).unwrap()
}

/// Offsets of `size / piece` pieces in a fixed shuffled order.
fn shuffled(size: u64, piece: u64) -> Vec<u64> {
    let mut offsets: Vec<u64> = (0..size / piece).map(|i| i * piece).collect();
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    for i in (1..offsets.len()).rev() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        offsets.swap(i, (x % (i as u64 + 1)) as usize);
    }
    offsets
}

/// Versions of the file a peer is sent: one per commit.
fn versions_sent(fs: &LocalFs, ino: Ino) -> usize {
    let CatchUp::Entries(entries) = fs.catch_up(&Default::default(), usize::MAX).unwrap() else {
        panic!("expected entries");
    };
    entries
        .iter()
        .filter(|e| matches!(&e.op, Op::Set { ino: i, .. } if *i == ino))
        .count()
}

#[test]
fn a_file_written_at_scattered_offsets_is_committed_in_batches() {
    let dir = tempfile::tempdir().unwrap();
    let fs = open(dir.path());
    let ino = fs.create(ROOT_INO, "bin", 0o644).unwrap().ino;
    let (size, piece) = (32u64 << 20, 64u64 << 10);
    let data = vec![7u8; piece as usize];
    for at in shuffled(size, piece) {
        fs.write(ino, at, &data).unwrap();
    }
    fs.flush(ino).unwrap();
    assert_eq!(fs.getattr(ino).unwrap().size, size);
    // 32 MiB through an 8 MiB buffer: a handful of commits, against 512 pieces.
    let commits = versions_sent(&fs, ino);
    assert!(
        commits <= 16,
        "{commits} commits for {} pieces",
        size / piece
    );
}

#[test]
fn scattered_writes_made_durable_survive_a_power_cut() {
    let c = Cluster::start_with_config(1, "sync_quiet_ms = 1000");
    let fs = c.fs(0);
    let f = fs.create(ROOT_INO, "bin", 0o600).unwrap();
    let mut model = noise(8 << 20, 1);
    fs.write(f.ino, 0, &model).unwrap();
    fs.fsync(f.ino).unwrap();

    // Pieces over the committed bytes, past them, and over each other.
    let (size, piece) = (12u64 << 20, 96u64 << 10);
    for (n, at) in shuffled(size, piece).into_iter().step_by(3).enumerate() {
        let at = at + (n as u64 % 7) * 4096;
        let bytes = noise(piece as usize, 100 + n as u64);
        fs.write(f.ino, at, &bytes).unwrap();
        let end = at as usize + bytes.len();
        if model.len() < end {
            model.resize(end, 0);
        }
        model[at as usize..end].copy_from_slice(&bytes);
    }
    fs.fsync(f.ino).unwrap();

    c.tick();
    c.power_cut(0);
    c.restart(0);
    assert!(
        fs.read(f.ino, 0, u32::MAX).unwrap() == model,
        "the durable bytes changed"
    );
}
