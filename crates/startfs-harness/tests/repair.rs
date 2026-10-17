//! Failure repair, leave and recovery, driven through FsOps and the control
//! interface on a simulated cluster with a manual clock.

use startfs_core::ops::*;
use startfs_harness::Cluster;
use std::time::Duration;

const BIG: u64 = 10 << 30;
const MINUTE: Duration = Duration::from_secs(60);

/// Incompressible bytes, so a file really needs chunk storage.
fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

/// A filesystem of `copies` copies on `nodes` nodes, all offering plenty.
fn cluster(copies: u32, nodes: usize) -> Cluster {
    let c = Cluster::start(nodes);
    let token = c.control(0).create("home", copies).unwrap();
    for i in 1..nodes {
        c.control(i).join(&token, Some(BIG)).unwrap();
    }
    c.sync();
    c
}

fn put(fs: &impl FsOps, name: &str, data: &[u8]) -> Ino {
    let f = fs.create(ROOT_INO, name, 0o644).unwrap();
    assert_eq!(fs.write(f.ino, 0, data).unwrap(), data.len());
    f.ino
}

/// Chunks held as replicas on each node, as `status` reports them.
fn replicas(c: &Cluster) -> Vec<u64> {
    (0..c.node_count())
        .map(|i| c.control(i).status().map_or(0, |s| s.usage.chunks))
        .collect()
}

fn write_files(c: &Cluster, count: u64) {
    for i in 0..count {
        put(&c.fs(0), &format!("f{i}"), &noise(8 * 1024, i + 1));
    }
    c.sync();
}

#[test]
fn after_the_grace_period_every_chunk_is_back_to_n_copies_without_user_action() {
    let c = cluster(2, 4);
    write_files(&c, 24);
    let before = replicas(&c);
    assert!(before[3] > 0, "{before:?}");

    c.crash(3);
    c.advance(31 * MINUTE);
    c.sync();

    let after = replicas(&c);
    assert_eq!(
        after[..3].iter().sum::<u64>(),
        before.iter().sum::<u64>(),
        "{before:?} -> {after:?}"
    );
}

#[test]
fn an_outage_shorter_than_the_grace_period_triggers_no_repair() {
    let c = cluster(2, 4);
    write_files(&c, 24);
    let before = replicas(&c);

    c.crash(3);
    c.advance(29 * MINUTE);
    c.sync();

    let after = replicas(&c);
    assert_eq!(&after[..3], &before[..3], "{before:?} -> {after:?}");
}

#[test]
fn a_returning_nodes_copies_count_again_and_the_surplus_is_trimmed() {
    let c = cluster(2, 4);
    write_files(&c, 24);
    let before = replicas(&c);
    c.crash(3);
    c.advance(31 * MINUTE);
    c.sync();

    c.restart(3);
    for _ in 0..3 {
        c.advance(Duration::from_secs(10));
        c.sync();
    }

    // Placement is as before the outage: the stand-in copies are gone again.
    assert_eq!(replicas(&c), before);
    for i in 0..4 {
        assert!(c.control(i).status().unwrap().unreplicated.is_empty());
    }
}
// NEXT
