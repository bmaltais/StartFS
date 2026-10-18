//! Any node can ask the others for their storage figures on demand.

use startfs_core::ops::*;
use startfs_harness::Cluster;
use std::time::Duration;

const BIG: u64 = 10 << 30;

fn cluster(nodes: usize, copies: u32) -> Cluster {
    let c = Cluster::start(nodes);
    let token = c.control(0).create("home", copies).unwrap();
    c.control(0).set_offer(BIG).unwrap();
    for i in 1..nodes {
        c.control(i).join(&token, Some(BIG)).unwrap();
    }
    c.sync();
    c
}

fn settle(c: &Cluster) {
    for _ in 0..20 {
        c.advance(Duration::from_secs(30));
        c.sync();
    }
}

/// Compressible, so the figures show a saving.
fn write(c: &Cluster, node: usize, name: &str, data: &[u8]) {
    let f = c.fs(node).create(ROOT_INO, name, 0o644).unwrap();
    c.fs(node).write(f.ino, 0, data).unwrap();
    c.fs(node).flush(f.ino).unwrap();
}

#[test]
fn every_node_reports_its_own_storage_to_whoever_asks() {
    let c = cluster(3, 2);
    write(&c, 0, "a", &vec![b'x'; 2 << 20]);
    settle(&c);

    let rows = c.control(2).node_stats().unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows.iter().filter(|r| r.is_self).count(), 1);
    for row in &rows {
        let stats = row.stats.expect("every node is up");
        assert_eq!(stats.usage.offer_bytes, BIG);
        assert_eq!(stats.efficiency.logical_bytes, 2 << 20);
    }
    // Each row is that node's own replica store, as its `status` says.
    for i in 0..3 {
        let own = c.control(i).status().unwrap();
        let row = rows
            .iter()
            .find(|r| r.addr == own.members.iter().find(|m| m.is_self).unwrap().addr)
            .unwrap();
        assert_eq!(row.stats.unwrap().usage, own.usage);
    }
}

#[test]
fn a_node_that_is_down_is_listed_without_figures() {
    let c = cluster(3, 2);
    write(&c, 0, "a", &vec![b'x'; 1 << 20]);
    settle(&c);
    let down = c
        .control(1)
        .status()
        .unwrap()
        .members
        .into_iter()
        .find(|m| m.is_self)
        .unwrap()
        .addr;
    c.crash(1);
    c.advance(Duration::from_secs(600));
    c.sync();

    let rows = c.control(0).node_stats().unwrap();
    assert_eq!(rows.len(), 3);
    for row in rows {
        assert_eq!(row.stats.is_none(), row.addr == down, "{}", row.addr);
    }
}
