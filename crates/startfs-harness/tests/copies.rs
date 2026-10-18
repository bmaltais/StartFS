//! Changing the number of copies of a live filesystem, through the control
//! interface on a simulated cluster.

use startfs_core::ops::*;
use startfs_harness::{Cluster, noise};
use std::time::Duration;

const BIG: u64 = 10 << 30;

fn cluster(nodes: usize, copies: u32) -> Cluster {
    Cluster::with_offers(copies, &vec![BIG; nodes])
}

fn write(c: &Cluster, node: usize, name: &str, data: &[u8]) {
    let f = c.fs(node).create(ROOT_INO, name, 0o644).unwrap();
    c.fs(node).write(f.ino, 0, data).unwrap();
    c.fs(node).flush(f.ino).unwrap();
}

/// Let background repair run for a while.
fn settle(c: &Cluster) {
    for _ in 0..20 {
        c.advance(Duration::from_secs(30));
        c.sync();
    }
}

fn copies_everywhere(c: &Cluster, nodes: usize) -> Vec<u32> {
    (0..nodes)
        .map(|i| c.control(i).status().unwrap().filesystem.unwrap().copies)
        .collect()
}

#[test]
fn raising_the_copies_reaches_every_node_and_repair_makes_the_extra_copy() {
    let c = cluster(3, 2);
    write(&c, 0, "a", &noise(60 * 1024, 1));
    settle(&c);
    let before: Vec<u64> = (0..3)
        .map(|i| c.control(i).status().unwrap().usage.stored_bytes)
        .collect();
    assert_eq!(before.iter().filter(|b| **b > 0).count(), 2, "{before:?}");

    c.control(1).set_copies(3).unwrap();
    settle(&c);

    assert_eq!(copies_everywhere(&c, 3), [3, 3, 3]);
    for i in 0..3 {
        let s = c.control(i).status().unwrap();
        assert!(s.usage.stored_bytes > 0, "node {i} holds no copy");
        assert!(s.unreplicated.is_empty(), "node {i}: {:?}", s.unreplicated);
    }
}

fn total_stored(c: &Cluster, nodes: usize) -> u64 {
    (0..nodes)
        .map(|i| c.control(i).status().unwrap().usage.stored_bytes)
        .sum()
}

#[test]
fn lowering_the_copies_trims_the_surplus_and_never_drops_below_the_new_count() {
    let c = cluster(3, 3);
    let data = noise(60 * 1024, 2);
    write(&c, 0, "a", &data);
    settle(&c);
    let full = total_stored(&c, 3);

    c.control(2).set_copies(2).unwrap();
    settle(&c);

    assert_eq!(copies_everywhere(&c, 3), [2, 2, 2]);
    let trimmed = total_stored(&c, 3);
    assert!(trimmed < full, "nothing trimmed: {full} -> {trimmed}");
    assert!(trimmed >= full * 2 / 3, "trimmed below 2 copies: {trimmed}");
    for i in 0..3 {
        let s = c.control(i).status().unwrap();
        assert!(s.unreplicated.is_empty(), "node {i}: {:?}", s.unreplicated);
        let f = c.fs(i).lookup(ROOT_INO, "a").unwrap();
        assert_eq!(c.fs(i).read(f.ino, 0, u32::MAX).unwrap(), data);
    }
}

#[test]
fn lowering_the_copies_is_refused_while_a_member_is_down() {
    let c = cluster(3, 3);
    write(&c, 0, "a", &noise(60 * 1024, 3));
    settle(&c);
    c.crash(2);
    settle(&c);

    let refused = c.control(0).set_copies(2);

    assert!(refused.is_err(), "lowering must wait for a healthy cluster");
    assert_eq!(copies_everywhere(&c, 2), [3, 3]);
}

#[test]
fn members_that_change_the_copies_at_the_same_time_settle_on_one_value() {
    use startfs_core::net::NodeId;
    let c = cluster(3, 2);
    c.network()
        .partition(&[&[NodeId(0)], &[NodeId(1), NodeId(2)]]);

    c.control(0).set_copies(3).unwrap();
    c.control(2).set_copies(4).unwrap();
    assert_eq!(copies_everywhere(&c, 3), [3, 2, 4]);

    c.network().heal();
    settle(&c);

    assert_eq!(copies_everywhere(&c, 3), [4, 4, 4]);
}

#[test]
fn a_filesystem_that_wants_more_copies_than_it_has_nodes_can_be_lowered_back() {
    let c = cluster(2, 3);
    write(&c, 0, "a", &noise(60 * 1024, 4));
    settle(&c);

    c.control(0).set_copies(2).unwrap();
    settle(&c);

    assert_eq!(copies_everywhere(&c, 2), [2, 2]);
}
