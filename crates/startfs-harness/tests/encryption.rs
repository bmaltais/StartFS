//! Encryption at rest: what a replica host stores, and how a node gets the key (#184).

use startfs_core::ops::*;
use startfs_harness::{Cluster, noise};
use std::path::Path;

fn put(fs: &impl FsOps, name: &str, data: &[u8]) -> Ino {
    let f = fs.create(ROOT_INO, name, 0o644).unwrap();
    assert_eq!(fs.write(f.ino, 0, data).unwrap(), data.len());
    f.ino
}

fn files_under(dir: &Path, out: &mut Vec<Vec<u8>>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            files_under(&p, out);
        } else if p.file_name().is_some_and(|n| n != "fs.key") {
            out.push(std::fs::read(&p).unwrap());
        }
    }
}

fn found_in(c: &Cluster, node: usize, needle: &[u8], stores: &[&str]) -> bool {
    let mut all = Vec::new();
    for store in stores {
        files_under(&c.data_dir(node).join(store), &mut all);
    }
    all.iter()
        .any(|f| f.windows(needle.len()).any(|w| w == needle))
}

const SECRET: &[u8] = b"the quarterly numbers are strictly confidential";

/// A file that does not compress, so its text would sit in the clear in a plain chunk.
fn readable() -> Vec<u8> {
    let mut body = SECRET.to_vec();
    body.extend(noise(30_000, 3));
    body
}

#[test]
fn the_check_can_see_plaintext_in_a_plain_filesystem() {
    let c = Cluster::with_filesystem(1, 1, "");
    put(&c.fs(0), "plans.txt", &readable());
    c.sync();
    assert!(found_in(&c, 0, SECRET, &["chunks", "staging"]));
}

#[test]
fn a_replica_host_holds_no_plaintext_chunks() {
    let (c, _key) = Cluster::with_encrypted_filesystem(2, 3);
    let body = readable();
    put(&c.fs(0), "plans.txt", &body);
    c.sync();
    let ino = c.fs(2).lookup(ROOT_INO, "plans.txt").unwrap().ino;
    assert_eq!(c.fs(2).read(ino, 0, body.len() as u32).unwrap(), body);

    for node in 0..3 {
        assert!(
            !found_in(&c, node, SECRET, &["chunks", "staging", "cache"]),
            "node {node} holds readable content"
        );
    }
    let held: u64 = (0..3)
        .map(|i| c.control(i).status().map_or(0, |s| s.usage.chunks))
        .sum();
    assert!(held >= 2, "the chunks are replicated");
}

#[test]
fn a_node_cannot_join_an_encrypted_filesystem_without_the_key() {
    let c = Cluster::start(2);
    let (token, _key) = c.control(0).create_filesystem("home", 1, true).unwrap();
    let no_key = c.control(1).join(&token, None).unwrap_err().to_string();
    assert!(no_key.contains("recovery key"), "{no_key}");

    let wrong = "sfrk1-00000000-00000000-00000000-00000000-00000000-00000000-00000000-00000000";
    let err = c
        .control(1)
        .join_with_key(&token, None, "", wrong)
        .unwrap_err()
        .to_string();
    assert!(err.contains("does not open"), "{err}");

    let junk = c.control(1).join_with_key(&token, None, "", "hunter2");
    assert!(junk.is_err());
}

#[test]
fn a_rebuilt_machine_comes_back_with_the_key_and_not_without_it() {
    let (c, key) = Cluster::with_encrypted_filesystem(2, 3);
    let body = noise(40_000, 7);
    put(&c.fs(0), "keep.bin", &body);
    c.sync();

    c.rebuild(2);
    let token = c.control(0).new_token().unwrap();
    assert!(c.control(2).join(&token, None).is_err());
    c.control(2).join_with_key(&token, None, "", &key).unwrap();
    c.sync();

    let ino = c.fs(2).lookup(ROOT_INO, "keep.bin").unwrap().ino;
    assert_eq!(c.fs(2).read(ino, 0, body.len() as u32).unwrap(), body);
}

#[test]
fn a_running_member_makes_a_new_key_that_adds_a_node() {
    let c = Cluster::start(4);
    let (token, original) = c.control(0).create_filesystem("home", 1, true).unwrap();
    let original = original.unwrap();
    c.control(1)
        .join_with_key(&token, None, "", &original)
        .unwrap();
    c.sync();

    // Nobody kept the original; a running member still holds the filesystem key.
    let fresh = c.control(1).recovery_key().unwrap();
    assert_ne!(fresh, original);
    c.sync();

    // Node 0 learned of it by gossip and admits with it.
    let token = c.control(0).new_token().unwrap();
    c.control(2)
        .join_with_key(&token, None, "", &fresh)
        .unwrap();
    // The old key still opens it too: nothing was re-encrypted.
    c.control(3)
        .join_with_key(&token, None, "", &original)
        .unwrap();
}

#[test]
fn a_plain_filesystem_has_no_key_to_make() {
    let c = Cluster::with_filesystem(1, 2, "");
    let err = c.control(0).recovery_key().unwrap_err().to_string();
    assert!(err.contains("not encrypted"), "{err}");
}
