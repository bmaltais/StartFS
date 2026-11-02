//! Entries a member already holds cost it no disk writes. Every member passes on what it receives,
//! so a member is sent each entry by several peers; a durable commit per such batch kept the weak
//! members' tick busy syncing nothing during a build (#166). Counts the process's write syscalls,
//! so this file holds one test.
#![cfg(target_os = "linux")]

use startfs_core::clock::SystemClock;
use startfs_core::fs::{CatchUp, LocalFs, NodeConfig};
use startfs_core::members::MemberId;
use startfs_core::ops::*;
use std::path::Path;
use std::sync::Arc;

fn open(id: u32, dir: &Path) -> LocalFs {
    let node = NodeConfig {
        node: MemberId(id),
        uid: 1000,
        gid: 1000,
    };
    LocalFs::open(dir, node, 1 << 20, Arc::new(SystemClock)).unwrap()
}

fn write_syscalls() -> u64 {
    std::fs::read_to_string("/proc/self/io")
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("syscw: "))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

#[test]
fn entries_already_held_cost_no_disk_writes() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, b) = (open(1, da.path()), open(2, db.path()));
    let dir = a.mkdir(ROOT_INO, "d", 0o755).unwrap().ino;
    for i in 0..20 {
        a.create(dir, &format!("f{i}"), 0o644).unwrap();
    }
    let CatchUp::Entries(entries) = a.catch_up(&Default::default(), 5000).unwrap() else {
        panic!("expected entries");
    };
    assert_eq!(b.apply_remote(&entries).unwrap(), entries.len());

    let before = write_syscalls();
    for _ in 0..10 {
        // The same batch again, as the next peer passes it on, and a part of it.
        assert_eq!(b.apply_remote(&entries).unwrap(), 0);
        assert_eq!(b.apply_remote(&entries[5..]).unwrap(), 0);
    }
    assert_eq!(
        write_syscalls() - before,
        0,
        "write syscalls for entries held"
    );
}
