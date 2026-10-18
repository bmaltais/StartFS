//! Status as a cluster-wide answer to "is my data safe right now?".

use startfs_core::control::Shortfall;
use startfs_core::net::NodeId;
use startfs_core::ops::*;
use startfs_core::sync::HELLO_INTERVAL;
use startfs_harness::{Cluster, noise};
use std::time::Duration;

const BIG: u64 = 10 << 30;

/// Two copies wanted but only node 0 offers space: a file written anywhere
/// can never reach more than one.
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

fn write_on(c: &Cluster, node: usize, name: &str) {
    let f = c.fs(node).create(ROOT_INO, name, 0o644).unwrap();
    c.fs(node).write(f.ino, 0, &noise(60 * 1024, 1)).unwrap();
    c.sync();
}

fn gossip(c: &Cluster) {
    c.advance(HELLO_INTERVAL);
    c.sync();
}

#[test]
fn a_chunk_unreplicated_on_one_node_shows_in_every_nodes_status() {
    let c = one_storage_node();
    write_on(&c, 2, "a");
    gossip(&c);

    let expected = [Shortfall {
        copies: 1,
        chunks: 1,
    }];
    for node in [0, 1, 3] {
        let s = c.control(node).status().unwrap();
        assert_eq!(s.unreplicated, expected, "node {node}");
        assert!(s.unheard.is_empty(), "node {node}");
    }
}

#[test]
fn a_partitioned_member_makes_the_view_partial_and_is_named() {
    let c = one_storage_node();
    write_on(&c, 2, "a");
    gossip(&c);

    let ids = |ns: &[u32]| ns.iter().map(|n| NodeId(*n)).collect::<Vec<_>>();
    c.network().partition(&[&ids(&[0, 1, 3]), &ids(&[2])]);
    // Long enough for node 2 to count as down.
    for _ in 0..4 {
        gossip(&c);
    }
    let s = c.control(3).status().unwrap();
    assert_eq!(s.unheard.len(), 1);
    let named = s.members.iter().find(|m| m.id == s.unheard[0]).unwrap();
    assert_eq!(named.addr, c.control(2).status().unwrap().members[2].addr);
    // What node 2 reported is not shown as current while it is out of reach;
    // node 0, which holds the chunk, reports it once.
    assert_eq!(
        s.unreplicated,
        [Shortfall {
            copies: 1,
            chunks: 1
        }]
    );

    c.network().heal();
    gossip(&c);
    let s = c.control(3).status().unwrap();
    assert!(s.unheard.is_empty());
    assert_eq!(
        s.unreplicated,
        [Shortfall {
            copies: 1,
            chunks: 1
        }]
    );
}

#[test]
fn a_member_that_cannot_connect_is_shown_with_the_reason() {
    let c = one_storage_node();
    let why = "protocol version mismatch: the peer speaks version 5, this node speaks version 4; upgrade this node";
    c.network()
        .partition(&[&[NodeId(0), NodeId(1), NodeId(3)], &[NodeId(2)]]);
    c.network().set_error(NodeId(3), NodeId(2), Some(why));
    for _ in 0..4 {
        gossip(&c);
    }
    let members = c.control(3).status().unwrap().members;
    assert_eq!(members[2].problem.as_deref(), Some(why));
    assert_eq!(
        members[1].problem, None,
        "a reachable member has no problem"
    );
    assert_eq!(
        members[3].problem, None,
        "this node has no problem with itself"
    );
}

#[test]
fn repair_progress_is_the_same_cluster_wide_total_on_every_node() {
    let c = Cluster::start_with_config(4, "repair_bytes_per_sec = 20000");
    let token = c.control(0).create("home", 2).unwrap();
    for i in 1..4 {
        c.control(i).join(&token, Some(BIG)).unwrap();
    }
    c.sync();
    for i in 0..60 {
        let f = c.fs(0).create(ROOT_INO, &format!("f{i}"), 0o644).unwrap();
        c.fs(0).write(f.ino, 0, &noise(8 * 1024, i + 1)).unwrap();
    }
    c.sync();
    c.crash(3);
    c.advance(Duration::from_secs(31 * 60));
    c.sync();
    gossip(&c);

    let queued = |i: usize| {
        c.control(i)
            .status()
            .unwrap()
            .repair
            .map_or(0, |r| r.at_risk + r.degraded)
    };
    let total = queued(0);
    assert!(total > 0, "no repair shown while it is under way");
    assert_eq!(queued(1), total);
    assert_eq!(queued(2), total);
}
