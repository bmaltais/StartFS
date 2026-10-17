//! Joining a filesystem, membership and remote reads, driven through the
//! control interface and FsOps on a cluster with a simulated network.

use startfs_core::control::NodeState;
use startfs_core::ops::*;
use startfs_harness::Cluster;
use startfs_core::net::NodeId;
use startfs_core::sync::HELLO_INTERVAL;
use std::time::Duration;

fn names(fs: &impl FsOps, dir: Ino) -> Vec<String> {
    fs.readdir(dir)
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect()
}

/// A filesystem created on node 0 with the other nodes joined in order.
fn joined(nodes: usize, copies: u32) -> Cluster {
    let c = Cluster::start(nodes);
    let token = c.control(0).create("home", copies).unwrap();
    for i in 1..nodes {
        c.control(i).join(&token, None).unwrap();
    }
    c.sync();
    c
}

#[test]
fn every_node_gets_its_own_identity_so_inode_numbers_never_clash() {
    let c = joined(3, 3);
    let mut inos = Vec::new();
    for i in 0..3 {
        let f = c.fs(i).create(ROOT_INO, &format!("from-{i}"), 0o644).unwrap();
        inos.push(f.ino);
    }
    c.sync();
    inos.sort();
    inos.dedup();
    assert_eq!(inos.len(), 3, "inode numbers clash");
    for i in 0..3 {
        assert_eq!(names(&c.fs(i), ROOT_INO), ["from-0", "from-1", "from-2"]);
    }
}

#[test]
fn a_second_node_joins_with_a_token_and_sees_the_whole_tree() {
    let c = Cluster::start(2);
    let a = c.fs(0);
    let token = c.control(0).create("home", 2).unwrap();
    let d = a.mkdir(ROOT_INO, "docs", 0o755).unwrap();
    let f = a.create(d.ino, "note.txt", 0o644).unwrap();
    a.write(f.ino, 0, b"hello").unwrap();

    c.control(1).join(&token, None).unwrap();
    c.sync();

    let b = c.fs(1);
    let bd = b.lookup(ROOT_INO, "docs").unwrap();
    let bf = b.lookup(bd.ino, "note.txt").unwrap();
    assert_eq!(b.read(bf.ino, 0, 100).unwrap(), b"hello");
    let fs = c.control(1).status().unwrap().filesystem.unwrap();
    assert_eq!((fs.name.as_str(), fs.copies), ("home", 2));
}

#[test]
fn an_expired_token_is_rejected_and_a_fresh_one_works() {
    let c = Cluster::start_with_config(2, "token_ttl_secs = 600");
    let old = c.control(0).create("home", 2).unwrap();
    c.advance(Duration::from_secs(601));

    let err = c.control(1).join(&old, None).unwrap_err();
    assert!(err.contains("expired"), "{err}");
    assert!(c.control(1).status().unwrap().filesystem.is_none());

    let fresh = c.control(0).new_token().unwrap();
    c.control(1).join(&fresh, None).unwrap();
    assert!(c.control(1).status().unwrap().filesystem.is_some());
}

#[test]
fn a_revoked_token_is_rejected_even_before_it_expires() {
    let c = Cluster::start(3);
    let leaked = c.control(0).create("home", 2).unwrap();
    let other = c.control(0).new_token().unwrap();
    c.control(0).revoke(&leaked).unwrap();

    let err = c.control(1).join(&leaked, None).unwrap_err();
    assert!(err.contains("revoked"), "{err}");
    // Other tokens are unaffected.
    c.control(2).join(&other, None).unwrap();
}

#[test]
fn nonsense_and_foreign_tokens_are_rejected() {
    let c = Cluster::start(2);
    let token = c.control(0).create("home", 2).unwrap();
    assert!(c.control(1).join("hello", None).is_err());
    let forged = token.replace('.', ".0");
    assert!(c.control(1).join(&forged, None).is_err());
    assert!(c.control(1).status().unwrap().filesystem.is_none());
    // A node that is already a member cannot join another filesystem.
    c.control(1).join(&token, None).unwrap();
    assert!(c.control(1).join(&token, None).is_err());
}

#[test]
fn status_lists_every_member_with_its_address_and_offer() {
    let c = Cluster::start(3);
    let token = c.control(0).create("home", 2).unwrap();
    c.control(1).join(&token, Some(2_000)).unwrap();
    c.control(2).join(&token, Some(0)).unwrap();
    c.sync();

    // Every member sees the same list, including members admitted by others.
    for i in 0..3 {
        let s = c.control(i).status().unwrap();
        let view: Vec<_> = s
            .members
            .iter()
            .map(|m| (m.id, m.addr.as_str(), m.offer_bytes, m.state, m.is_self))
            .collect();
        let up = NodeState::Online;
        assert_eq!(
            view,
            [
                (0, "100.64.0.1:7600", 10 << 30, up, i == 0),
                (1, "100.64.0.2:7600", 2_000, up, i == 1),
                (2, "100.64.0.3:7600", 0, up, i == 2),
            ],
            "seen from node {i}"
        );
        assert_eq!(s.nodes, 3);
    }
}

fn states(c: &Cluster, viewer: usize) -> Vec<NodeState> {
    c.control(viewer)
        .status()
        .unwrap()
        .members
        .iter()
        .map(|m| m.state)
        .collect()
}

#[test]
fn a_member_that_goes_quiet_is_shown_down_and_comes_back_up() {
    let c = joined(3, 2);
    let (up, down) = (NodeState::Online, NodeState::Down);
    assert_eq!(states(&c, 0), [up, up, up]);

    // Node 2 is cut off; its silence is noticed once a few hellos are missed.
    c.network().partition(&[&[NodeId(0), NodeId(1)], &[NodeId(2)]]);
    for _ in 0..4 {
        c.advance(HELLO_INTERVAL);
        c.sync();
    }
    assert_eq!(states(&c, 0), [up, up, down]);
    assert_eq!(states(&c, 1), [up, up, down]);
    // A node always sees itself as up, and the others as down.
    assert_eq!(states(&c, 2), [down, down, up]);

    c.network().heal();
    c.advance(HELLO_INTERVAL);
    c.sync();
    assert_eq!(states(&c, 0), [up, up, up]);

    // A crashed node is down for the others too.
    c.crash(1);
    for _ in 0..4 {
        c.advance(HELLO_INTERVAL);
        c.sync();
    }
    assert_eq!(states(&c, 0), [up, down, up]);
}
