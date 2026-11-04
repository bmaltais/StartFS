//! Directories with a number of copies of their own: scratch held at one copy while photos
//! hold three, gossiped to every member, honoured by placement and repair, and shown by
//! `status`.

use startfs_core::ops::*;
use startfs_harness::{Cluster, noise};
use std::time::Duration;

const BIG: u64 = 10 << 30;

fn cluster() -> Cluster {
    Cluster::with_offers(3, &[BIG; 4])
}

fn dir(c: &Cluster, name: &str) -> Ino {
    c.fs(0).mkdir(ROOT_INO, name, 0o755).unwrap().ino
}

fn write(c: &Cluster, parent: Ino, name: &str, seed: u64) {
    let f = c.fs(0).create(parent, name, 0o644).unwrap();
    c.fs(0).write(f.ino, 0, &noise(60 * 1024, seed)).unwrap();
    c.fs(0).flush(f.ino).unwrap();
}

fn settle(c: &Cluster) {
    for _ in 0..20 {
        c.advance(Duration::from_secs(30));
        c.sync();
    }
}

fn stored(c: &Cluster) -> u64 {
    (0..c.node_count())
        .map(|i| c.control(i).status().unwrap().usage.stored_bytes)
        .sum()
}

#[test]
fn a_scratch_directory_is_held_at_one_copy_while_another_holds_three() {
    let c = cluster();
    let scratch = dir(&c, "scratch");
    let photos = dir(&c, "photos");
    c.control(0).set_dir_copies("/scratch", Some(1)).unwrap();
    c.control(0).set_dir_copies("/photos", Some(3)).unwrap();
    c.sync();
    write(&c, scratch, "tmp", 1);
    settle(&c);
    let scratch_total = stored(&c);
    write(&c, photos, "img", 2);
    settle(&c);
    let both = stored(&c);

    // About 60 KiB each: one copy of the first, three of the second.
    let photo_total = both - scratch_total;
    assert!(
        photo_total > scratch_total * 2,
        "scratch {scratch_total}, photos {photo_total}"
    );
    for i in 0..4 {
        let s = c.control(i).status().unwrap();
        assert!(s.unreplicated.is_empty(), "node {i}: {:?}", s.unreplicated);
        assert_eq!(s.filesystem.unwrap().dir_copies["/scratch"], 1);
    }
}

#[test]
fn a_directory_entry_trims_files_already_there_and_clearing_it_makes_the_copies_again() {
    let c = cluster();
    let scratch = dir(&c, "scratch");
    write(&c, scratch, "tmp", 3);
    settle(&c);
    let full = stored(&c);

    c.control(1).set_dir_copies("/scratch", Some(1)).unwrap();
    settle(&c);
    let trimmed = stored(&c);
    assert!(trimmed * 2 < full, "{full} -> {trimmed}");
    assert!(trimmed > 0);

    c.control(2).set_dir_copies("/scratch", None).unwrap();
    settle(&c);
    assert_eq!(stored(&c), full);
    for i in 0..4 {
        let s = c.control(i).status().unwrap();
        assert!(s.unreplicated.is_empty(), "node {i}: {:?}", s.unreplicated);
    }
}

/// Two copies wanted, but only node 0 offers space: nothing can reach more than one.
fn one_storage_node() -> Cluster {
    let c = Cluster::start(4);
    let token = c.control(0).create("home", 2).unwrap();
    c.control(0).set_offer(BIG).unwrap();
    for i in 1..4 {
        c.control(i).join(&token, Some(0)).unwrap();
    }
    c.sync();
    c
}

#[test]
fn status_counts_the_shortfall_against_the_directorys_own_target() {
    let c = one_storage_node();
    let scratch = dir(&c, "scratch");
    let keep = dir(&c, "keep");
    c.control(0).set_dir_copies("/scratch", Some(1)).unwrap();
    c.sync();
    write(&c, scratch, "tmp", 4);
    settle(&c);
    // One copy is all scratch wants: nothing is short.
    assert!(c.control(3).status().unwrap().unreplicated.is_empty());

    write(&c, keep, "img", 5);
    settle(&c);
    // The other file follows the filesystem's two copies and has one.
    let short: u32 = (c.control(3).status().unwrap().unreplicated.iter())
        .map(|f| f.chunks)
        .sum();
    assert_eq!(short, 1);
}

#[test]
fn a_chunk_shared_with_a_default_directory_keeps_the_default_copies() {
    let c = cluster();
    let scratch = dir(&c, "scratch");
    c.control(0).set_dir_copies("/scratch", Some(1)).unwrap();
    c.sync();
    let shared = noise(60 * 1024, 5);
    for (parent, name) in [(scratch, "a"), (ROOT_INO, "b")] {
        let f = c.fs(0).create(parent, name, 0o644).unwrap();
        c.fs(0).write(f.ino, 0, &shared).unwrap();
        c.fs(0).flush(f.ino).unwrap();
    }
    settle(&c);
    assert_eq!(stored(&c) % 3, 0, "three copies of each chunk");
    assert!(c.control(0).status().unwrap().unreplicated.is_empty());
}

#[test]
fn lowering_a_directory_waits_for_a_healthy_cluster_like_lowering_the_copies() {
    let c = cluster();
    c.control(0).set_dir_copies("/photos", Some(4)).unwrap();
    // Nothing is short, so lowering is allowed; raising never waits.
    c.control(0).set_dir_copies("/photos", Some(2)).unwrap();
    assert!(c.control(0).set_dir_copies("/", Some(2)).is_err());
    assert!(c.control(0).set_dir_copies("/photos", Some(0)).is_err());
}
