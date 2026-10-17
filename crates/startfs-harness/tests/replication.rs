//! Placement, background replication, pure clients and capacity, driven
//! through FsOps and the control interface on a simulated cluster.

use startfs_core::control::{FileHealth, Tier};
use startfs_core::net::NodeId;
use startfs_core::ops::*;
use startfs_core::replication::RETRY;
use startfs_harness::Cluster;

const BIG: u64 = 10 << 30;

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

/// A filesystem of `copies` copies; node `i` joins offering `offers[i]` bytes
/// (node 0 creates it, then sets its offer).
fn cluster(copies: u32, offers: &[u64]) -> Cluster {
    let c = Cluster::start(offers.len());
    let token = c.control(0).create("home", copies).unwrap();
    c.control(0).set_offer(offers[0]).unwrap();
    for (i, offer) in offers.iter().enumerate().skip(1) {
        c.control(i).join(&token, Some(*offer)).unwrap();
    }
    c.sync();
    c
}

fn put(fs: &impl FsOps, name: &str, data: &[u8]) -> Ino {
    let f = fs.create(ROOT_INO, name, 0o644).unwrap();
    assert_eq!(fs.write(f.ino, 0, data).unwrap(), data.len());
    f.ino
}

#[test]
fn a_pure_client_write_is_durable_locally_and_listed_as_not_yet_replicated() {
    let c = cluster(2, &[BIG, BIG, 0]);
    let data = noise(60 * 1024, 1);
    let ino = put(&c.fs(2), "a", &data);

    assert_eq!(c.fs(2).read(ino, 0, u32::MAX).unwrap(), data);
    assert_eq!(c.control(2).status().unwrap().staging.chunks, 1);
    assert_eq!(
        c.control(2).status().unwrap().unreplicated,
        [FileHealth {
            path: "/a".into(),
            copies: 1,
            wanted: 2,
            tier: Tier::AtRisk
        }]
    );
}

#[test]
fn replicas_are_spread_in_proportion_to_each_nodes_offer() {
    let g = 1 << 30;
    let c = cluster(1, &[g, 3 * g, 0]);
    for i in 0..200 {
        put(&c.fs(2), &format!("f{i}"), &noise(8 * 1024, i + 1));
    }
    c.sync();

    let held = replicas(&c);
    assert_eq!(held.iter().sum::<u64>(), 200, "{held:?}");
    // A quarter of the space, so about a quarter of the chunks (50 +- 20).
    assert!((30..=70).contains(&held[0]), "{held:?}");
}

#[test]
fn with_too_few_storage_nodes_writes_succeed_best_effort_and_status_says_so() {
    let c = cluster(3, &[BIG, BIG, 0]);
    let data = noise(60 * 1024, 1);
    // The write is not refused although three copies cannot be placed.
    let ino = put(&c.fs(2), "a", &data);
    c.sync();

    let s = c.control(2).status().unwrap();
    assert_eq!(s.copies_possible, 2);
    assert!(
        s.warnings
            .iter()
            .any(|w| w.contains("3 wanted, 2 possible")),
        "{:?}",
        s.warnings
    );
    assert_eq!(
        s.unreplicated,
        [FileHealth {
            path: "/a".into(),
            copies: 2,
            wanted: 3,
            tier: Tier::Degraded
        }]
    );
    assert_eq!(replicas(&c), [1, 1, 0]);
    assert_eq!(s.staging.chunks, 0);
    assert_eq!(c.fs(2).read(ino, 0, u32::MAX).unwrap(), data);
}

#[test]
fn a_files_tier_follows_how_many_nodes_hold_its_chunks() {
    let c = cluster(3, &[BIG, BIG, BIG, 0]);
    let ids = |ns: &[u32]| ns.iter().map(|n| NodeId(*n)).collect::<Vec<_>>();
    let tier = |c: &Cluster| {
        c.control(3)
            .status()
            .unwrap()
            .unreplicated
            .first()
            .map(|f| (f.copies, f.tier))
    };

    // The client reaches nobody: its staged copy is all there is.
    c.network().partition(&[&ids(&[3]), &ids(&[0, 1, 2])]);
    put(&c.fs(3), "a", &noise(60 * 1024, 1));
    c.sync();
    assert_eq!(tier(&c), Some((1, Tier::AtRisk)));

    // One storage node reachable: still a single copy.
    c.network().partition(&[&ids(&[3, 0]), &ids(&[1, 2])]);
    c.advance(RETRY);
    c.sync();
    assert_eq!(tier(&c), Some((1, Tier::AtRisk)));

    c.network().partition(&[&ids(&[3, 0, 1]), &ids(&[2])]);
    c.advance(RETRY);
    c.sync();
    assert_eq!(tier(&c), Some((2, Tier::Degraded)));
    assert_eq!(c.control(3).status().unwrap().staging.chunks, 1);

    c.network().heal();
    c.advance(RETRY);
    c.sync();
    assert_eq!(tier(&c), None);
    assert_eq!(c.control(3).status().unwrap().staging.chunks, 0);
    assert_eq!(replicas(&c), [1, 1, 1, 0]);
}

#[test]
fn full_nodes_are_skipped_in_placement_and_status_warns() {
    // Each node has room for 8 of the 100 KB chunks, 24 in all; 14 files want 28.
    let offer = 8 * 100_100;
    let c = cluster(2, &[offer, offer, offer, 0]);
    let files: Vec<_> = (0..14)
        .map(|i| {
            // Writes are never refused for want of room elsewhere.
            let data = noise(100_000, i + 1);
            (put(&c.fs(3), &format!("f{i}"), &data), data)
        })
        .collect();
    c.sync();
    // Nothing is lost when the cluster has no room: data waits in staging.
    for (ino, data) in &files {
        assert_eq!(&c.fs(3).read(*ino, 0, u32::MAX).unwrap(), data);
    }
    assert!(c.control(3).status().unwrap().staging.chunks > 0);

    // Chunks turned away by a full node went to nodes with room, so every
    // slot is used and none is overfilled.
    assert_eq!(replicas(&c), [8, 8, 8, 0]);
    let s = c.control(3).status().unwrap();
    assert!(
        s.warnings
            .iter()
            .any(|w| w.contains("100.64.0.1") && w.contains("full")),
        "{:?}",
        s.warnings
    );
    // Full nodes cannot hold more copies, so none are counted as possible.
    assert_eq!(s.copies_possible, 0);
    assert!(!s.unreplicated.is_empty());
}

fn replicas(c: &Cluster) -> Vec<u64> {
    (0..c.node_count())
        .map(|i| c.control(i).status().unwrap().usage.chunks)
        .collect()
}

#[test]
fn a_pure_clients_data_reaches_n_storage_nodes_and_its_staging_is_released() {
    let c = cluster(2, &[BIG, BIG, BIG, 0]);
    let data = noise(60 * 1024, 1);
    let ino = put(&c.fs(3), "a", &data);

    c.sync();

    let held = replicas(&c);
    assert_eq!(held[3], 0, "a pure client stores no replicas");
    assert_eq!(held.iter().sum::<u64>(), 2, "{held:?}");
    let s = c.control(3).status().unwrap();
    assert_eq!(s.staging.chunks, 0);
    assert!(s.unreplicated.is_empty(), "{:?}", s.unreplicated);
    // The data is still readable, now from the storage nodes.
    assert_eq!(c.fs(3).read(ino, 0, u32::MAX).unwrap(), data);
}
