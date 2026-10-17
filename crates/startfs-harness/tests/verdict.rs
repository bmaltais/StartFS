//! The one-line answer to "is my data safe right now?", driven through the
//! control interface on a simulated cluster.

use startfs_core::control::Verdict;
use startfs_core::net::NodeId;
use startfs_core::ops::*;
use startfs_core::sync::HELLO_INTERVAL;
use startfs_harness::Cluster;

const BIG: u64 = 10 << 30;

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

fn write(c: &Cluster, node: usize) {
    let fs = c.fs(node);
    let f = fs.create(ROOT_INO, "f", 0o644).unwrap();
    fs.write(f.ino, 0, &noise(60_000, 7)).unwrap();
}

fn verdict(c: &Cluster, node: usize) -> Option<Verdict> {
    c.control(node).status().unwrap().verdict
}

#[test]
fn there_is_no_verdict_without_a_filesystem() {
    let c = Cluster::start(1);
    assert_eq!(verdict(&c, 0), None);
}

#[test]
fn fully_replicated_data_is_safe() {
    let c = cluster(2, &[BIG, BIG]);
    write(&c, 0);
    c.sync();
    assert_eq!(verdict(&c, 0), Some(Verdict::Safe));
    assert_eq!(verdict(&c, 1), Some(Verdict::Safe));
}

#[test]
fn a_file_with_a_single_copy_puts_the_data_at_risk_until_replicated() {
    let c = cluster(2, &[BIG, BIG, 0]);
    write(&c, 2);
    assert_eq!(verdict(&c, 2), Some(Verdict::AtRisk));
    c.sync();
    assert_eq!(verdict(&c, 2), Some(Verdict::Safe));
}

#[test]
fn a_silent_node_leaves_the_data_degraded_though_every_file_is_replicated() {
    let c = cluster(2, &[BIG, BIG]);
    write(&c, 0);
    c.sync();
    c.network().partition(&[&[NodeId(0)], &[NodeId(1)]]);
    for _ in 0..4 {
        c.advance(HELLO_INTERVAL);
        c.sync();
    }
    assert_eq!(verdict(&c, 0), Some(Verdict::Degraded));
    c.network().heal();
    c.advance(HELLO_INTERVAL);
    c.sync();
    assert_eq!(verdict(&c, 0), Some(Verdict::Safe));
}

#[test]
fn too_few_storage_nodes_for_n_copies_is_best_effort_not_a_failure() {
    let c = cluster(3, &[BIG, BIG]);
    write(&c, 0);
    c.sync();
    assert_eq!(verdict(&c, 0), Some(Verdict::BestEffort));
}
