//! A node that applies peers' entries in a batch shows the same tree as one
//! that applies them one by one, and a crash before the batch ends loses nothing.

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

fn names(fs: &LocalFs) -> Vec<String> {
    let mut v: Vec<String> = fs
        .readdir(ROOT_INO)
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    v.sort();
    v
}

/// Ship everything `from` holds to `to`, a few entries per message.
fn ship(from: &LocalFs, to: &LocalFs) {
    loop {
        let CatchUp::Entries(entries) = from.catch_up(&to.frontier().unwrap(), 5).unwrap() else {
            panic!("expected entries");
        };
        if entries.is_empty() {
            return;
        }
        to.apply_remote(&entries).unwrap();
    }
}

fn source(dir: &Path) -> LocalFs {
    let a = open(1, dir);
    for i in 0..40 {
        a.create(ROOT_INO, &format!("file-{i:02}"), 0o644).unwrap();
    }
    a
}

#[test]
fn a_batch_ends_with_the_same_tree_as_applying_one_by_one() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = source(da.path());
    let b = open(2, db.path());
    b.begin_batch();
    ship(&a, &b);
    b.end_batch().unwrap();
    assert_eq!(names(&b), names(&a));
}

#[test]
fn a_crash_before_the_batch_ends_is_repaired_at_the_next_start() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = source(da.path());
    let b = open(2, db.path());
    b.begin_batch();
    ship(&a, &b);
    b.sync().unwrap();
    drop(b);
    let b = open(2, db.path());
    assert_eq!(names(&b), names(&a));
}
