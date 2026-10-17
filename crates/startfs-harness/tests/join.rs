//! Joining a filesystem, membership and remote reads, driven through the
//! control interface and FsOps on a cluster with a simulated network.

use startfs_core::control::NodeState;
use startfs_core::net::NodeId;
use startfs_core::ops::*;
use startfs_core::sync::HELLO_INTERVAL;
use startfs_harness::Cluster;
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
        let f = c
            .fs(i)
            .create(ROOT_INO, &format!("from-{i}"), 0o644)
            .unwrap();
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
    c.network()
        .partition(&[&[NodeId(0), NodeId(1)], &[NodeId(2)]]);
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

fn offers(c: &Cluster, viewer: usize) -> Vec<u64> {
    let s = c.control(viewer).status().unwrap();
    s.members.iter().map(|m| m.offer_bytes).collect()
}

#[test]
fn a_member_changes_its_offer_with_config_and_everyone_learns_of_it() {
    let c = joined(2, 2);
    c.control(1).set_offer(500).unwrap();
    assert_eq!(c.control(1).status().unwrap().usage.offer_bytes, 500);
    c.advance(HELLO_INTERVAL);
    c.sync();
    assert_eq!(offers(&c, 0), [10 << 30, 500]);

    // The offer sticks across a restart, over the config file's default.
    c.crash(1);
    c.restart(1);
    assert_eq!(c.control(1).status().unwrap().usage.offer_bytes, 500);
    c.advance(HELLO_INTERVAL);
    c.sync();
    assert_eq!(offers(&c, 0), [10 << 30, 500]);
}

#[test]
fn an_offer_of_zero_makes_a_pure_client_that_adds_no_storage() {
    let c = Cluster::start(2);
    let token = c.control(0).create("home", 2).unwrap();
    c.control(1).join(&token, Some(0)).unwrap();
    c.sync();
    // Two members, but only one can hold replicas.
    let s = c.control(0).status().unwrap();
    assert_eq!((s.nodes, s.copies_possible), (2, 1));
    c.control(1).set_offer(1 << 30).unwrap();
    c.advance(HELLO_INTERVAL);
    c.sync();
    assert_eq!(c.control(0).status().unwrap().copies_possible, 2);
}

/// Incompressible bytes, so a file really needs chunk storage.
fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 32) as u8
        })
        .collect()
}

fn write_file(fs: &impl FsOps, name: &str, data: &[u8]) -> Ino {
    let f = fs.create(ROOT_INO, name, 0o644).unwrap();
    fs.write(f.ino, 0, data).unwrap();
    f.ino
}

#[test]
fn file_contents_are_fetched_from_a_peer_when_read() {
    let c = Cluster::start(2);
    let token = c.control(0).create("home", 2).unwrap();
    let data = noise(300_000, 7);
    write_file(&c.fs(0), "big", &data);
    c.control(1).join(&token, None).unwrap();
    c.sync();

    // The joiner pulled metadata only: it can see the file but not read it
    // unless a peer can supply the bytes.
    let b = c.fs(1);
    let on_b = b.lookup(ROOT_INO, "big").unwrap();
    assert_eq!(on_b.size, 300_000);
    c.network().partition(&[&[NodeId(0)], &[NodeId(1)]]);
    assert_eq!(b.read(on_b.ino, 0, u32::MAX), Err(FsError::Unavailable));

    c.network().heal();
    assert_eq!(b.read(on_b.ino, 0, u32::MAX).unwrap(), data);
}

#[test]
fn recently_read_files_stay_readable_offline_and_unread_ones_do_not() {
    let c = joined(2, 2);
    let (a, b) = (c.fs(0), c.fs(1));
    let seen = noise(100_000, 1);
    let unseen = noise(100_000, 2);
    write_file(&a, "seen", &seen);
    write_file(&a, "unseen", &unseen);
    c.sync();

    let on_b = |name: &str| b.lookup(ROOT_INO, name).unwrap().ino;
    assert_eq!(b.read(on_b("seen"), 0, u32::MAX).unwrap(), seen);

    // Offline for good: the daemon can even restart and the cache survives.
    c.network().partition(&[&[NodeId(0)], &[NodeId(1)]]);
    c.crash(1);
    c.restart(1);
    assert_eq!(b.read(on_b("seen"), 0, u32::MAX).unwrap(), seen);
    assert_eq!(b.read(on_b("unseen"), 0, 10), Err(FsError::Unavailable));
}

#[test]
fn a_chunk_is_fetched_from_whichever_peer_can_supply_it() {
    let c = joined(3, 3);
    let data = noise(100_000, 3);
    write_file(&c.fs(0), "f", &data);
    c.sync();
    // Node 1 reads (and so caches) the file, then the author goes away.
    let on_1 = c.fs(1).lookup(ROOT_INO, "f").unwrap().ino;
    assert_eq!(c.fs(1).read(on_1, 0, u32::MAX).unwrap(), data);
    c.crash(0);

    let on_2 = c.fs(2).lookup(ROOT_INO, "f").unwrap().ino;
    assert_eq!(c.fs(2).read(on_2, 0, u32::MAX).unwrap(), data);
}

#[test]
fn the_cache_is_bounded_and_drops_the_least_recently_used_file() {
    // Room for two of the three 100 KB files.
    let c = Cluster::start_with_config(2, "cache_bytes = 250000");
    let token = c.control(0).create("home", 2).unwrap();
    c.control(1).join(&token, None).unwrap();
    let a = c.fs(0);
    let files: Vec<_> = (1..=3u64).map(|i| noise(100_000, 2 * i + 1)).collect();
    for (i, data) in files.iter().enumerate() {
        write_file(&a, &format!("f{i}"), data);
    }
    c.sync();

    let b = c.fs(1);
    let ino = |i: usize| b.lookup(ROOT_INO, &format!("f{i}")).unwrap().ino;
    for i in [0, 1, 0, 2] {
        assert_eq!(b.read(ino(i), 0, u32::MAX).unwrap(), files[i]);
    }
    let cache = c.control(1).status().unwrap().cache;
    assert_eq!(cache.capacity_bytes, 250_000);
    assert!(cache.used_bytes <= 250_000, "{cache:?}");

    // f1 was the least recently used when f2 arrived.
    c.network().partition(&[&[NodeId(0)], &[NodeId(1)]]);
    assert_eq!(b.read(ino(0), 0, u32::MAX).unwrap(), files[0]);
    assert_eq!(b.read(ino(2), 0, u32::MAX).unwrap(), files[2]);
    assert_eq!(b.read(ino(1), 0, 10), Err(FsError::Unavailable));
}

#[test]
fn cache_eviction_never_touches_replica_storage() {
    let c = Cluster::start_with_config(2, "cache_bytes = 120000");
    let token = c.control(0).create("home", 2).unwrap();
    c.control(1).join(&token, None).unwrap();
    let files: Vec<_> = (1..=3u64).map(|i| noise(100_000, 2 * i + 1)).collect();
    for (i, data) in files.iter().enumerate() {
        write_file(&c.fs(0), &format!("f{i}"), data);
    }
    c.sync();
    let replicas_on_a = c.control(0).status().unwrap().usage;
    assert_eq!(replicas_on_a.chunks, 3);

    // Reading everything through a cache that holds one file churns it.
    for round in 0..2 {
        for i in 0..3 {
            let ino = c.fs(1).lookup(ROOT_INO, &format!("f{i}")).unwrap().ino;
            let got = c.fs(1).read(ino, 0, u32::MAX).unwrap();
            assert_eq!(got, files[i], "round {round}");
        }
    }
    // The reader stores no replicas; the author's replicas are untouched.
    let b = c.control(1).status().unwrap();
    assert_eq!((b.usage.chunks, b.usage.stored_bytes), (0, 0));
    assert_eq!(b.cache.chunks, 1);
    assert_eq!(c.control(0).status().unwrap().usage, replicas_on_a);

    // The author's own files never depend on its cache.
    c.network().partition(&[&[NodeId(0)], &[NodeId(1)]]);
    let own = c.fs(0).lookup(ROOT_INO, "f0").unwrap().ino;
    assert_eq!(c.fs(0).read(own, 0, u32::MAX).unwrap(), files[0]);
}
