//! Applying one new entry costs disk writes in proportion to the change, not to the size of the
//! tree (a disk with a few hundred IOPS stalls the tick otherwise, #85). Counts the process's
//! write syscalls, so this file holds one test.
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

fn ship(from: &LocalFs, to: &LocalFs) {
    loop {
        let CatchUp::Entries(entries) = from.catch_up(&to.frontier().unwrap(), 5000).unwrap()
        else {
            panic!("expected entries");
        };
        if entries.is_empty() {
            return;
        }
        to.apply_remote(&entries).unwrap();
    }
}

/// Write syscalls to apply one more file on a node that holds `files` files.
fn cost_of_one_more(files: usize) -> u64 {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, b) = (open(1, da.path()), open(2, db.path()));
    let dir = a.mkdir(ROOT_INO, "d", 0o755).unwrap().ino;
    for i in 0..files {
        a.create(dir, &format!("f{i}"), 0o644).unwrap();
    }
    ship(&a, &b);
    a.create(ROOT_INO, "one-more", 0o644).unwrap();
    let before = write_syscalls();
    ship(&a, &b);
    write_syscalls() - before
}

#[test]
fn one_entry_costs_the_same_on_a_small_tree_and_a_big_one() {
    let small = cost_of_one_more(200);
    let big = cost_of_one_more(5000);
    assert!(
        big <= small * 2 + 10,
        "{small} write syscalls on 200 files, {big} on 5000"
    );
}
