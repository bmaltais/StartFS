//! Placement, background replication, pure clients and capacity, driven
//! through FsOps and the control interface on a simulated cluster.

use startfs_core::control::{FileHealth, Tier};
use startfs_core::ops::*;
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
