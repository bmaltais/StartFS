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
        let at = c.data_dir(node).join(store);
        if at.is_dir() {
            files_under(&at, &mut all);
        } else if at.is_file() {
            all.push(std::fs::read(&at).unwrap());
        }
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
    assert!(found_in(&c, 0, b"plans.txt", &["meta.redb"]));
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
        let name_gone = !found_in(&c, node, b"plans.txt", &["meta.enc"]);
        assert!(name_gone, "node {node} holds a file name in the clear");
        assert!(!c.data_dir(node).join("meta.redb").exists());
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

#[test]
fn a_restarted_member_finds_its_key_and_reads_everything() {
    let (c, _key) = Cluster::with_encrypted_filesystem(2, 3);
    let body = noise(60_000, 11);
    put(&c.fs(0), "stay.bin", &body);
    c.sync();

    c.crash(1);
    c.restart(1);
    c.sync();

    let ino = c.fs(1).lookup(ROOT_INO, "stay.bin").unwrap().ino;
    assert_eq!(c.fs(1).read(ino, 0, body.len() as u32).unwrap(), body);
    assert!(c.data_dir(1).join("fs.key").exists());
}

/// The same files on a plain and an encrypted single-node filesystem.
fn workload(c: &Cluster) {
    let text = b"line of a log file that compresses very well. ".repeat(3000);
    put(&c.fs(0), "log.txt", &text);
    put(&c.fs(0), "log-copy.txt", &text);
    put(&c.fs(0), "photo.bin", &noise(300_000, 5));
    c.sync();
}

#[test]
fn encryption_leaves_dedup_and_compression_alone_and_costs_a_few_bytes_a_chunk() {
    let plain = Cluster::with_filesystem(1, 1, "");
    let (sealed, _key) = Cluster::with_encrypted_filesystem(1, 1);
    workload(&plain);
    workload(&sealed);
    let (p, s) = (
        plain.control(0).status().unwrap(),
        sealed.control(0).status().unwrap(),
    );

    // Dedup and compression decide the same chunks, so these match exactly.
    assert_eq!(p.usage.chunks, s.usage.chunks);
    assert_eq!(p.efficiency.referenced_bytes, s.efficiency.referenced_bytes);
    assert_eq!(p.efficiency.zstd_chunks, s.efficiency.zstd_chunks);
    assert_eq!(p.efficiency.raw_chunks, s.efficiency.raw_chunks);
    assert!(s.efficiency.zstd_chunks > 0, "{:?}", s.efficiency);

    // Each sealed chunk grows by its nonce and tag, nothing more.
    let extra = s.usage.stored_bytes - p.usage.stored_bytes;
    assert_eq!(
        extra,
        p.usage.chunks * startfs_core::crypto::SEAL_OVERHEAD as u64
    );
}
