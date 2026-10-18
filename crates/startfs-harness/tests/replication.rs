//! Placement, background replication, pure clients and capacity, driven
//! through FsOps and the control interface on a simulated cluster.

use startfs_core::control::Shortfall;
use startfs_core::net::NodeId;
use startfs_core::ops::*;
use startfs_core::replication::RETRY;
use startfs_core::sync::HELLO_INTERVAL;
use startfs_harness::{Cluster, noise};
use std::time::Duration;

const BIG: u64 = 10 << 30;

fn put(fs: &impl FsOps, name: &str, data: &[u8]) -> Ino {
    let f = fs.create(ROOT_INO, name, 0o644).unwrap();
    assert_eq!(fs.write(f.ino, 0, data).unwrap(), data.len());
    f.ino
}

#[test]
fn a_pure_client_write_is_durable_locally_and_counted_as_not_yet_replicated() {
    let c = Cluster::with_offers(2, &[BIG, BIG, 0]);
    let data = noise(60 * 1024, 1);
    let ino = put(&c.fs(2), "a", &data);

    assert_eq!(c.fs(2).read(ino, 0, u32::MAX).unwrap(), data);
    assert_eq!(c.control(2).status().unwrap().staging.chunks, 1);
    assert_eq!(
        c.control(2).status().unwrap().unreplicated,
        [Shortfall {
            copies: 1,
            chunks: 1
        }]
    );
}

#[test]
fn replicas_are_spread_in_proportion_to_each_nodes_offer() {
    let g = 1 << 30;
    let c = Cluster::with_offers(1, &[g, 3 * g, 0]);
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
    let c = Cluster::with_offers(3, &[BIG, BIG, 0]);
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
        // Held on two nodes, reported by one of them, not by both.
        [Shortfall {
            copies: 2,
            chunks: 1
        }]
    );
    assert_eq!(replicas(&c), [1, 1, 0]);
    assert_eq!(s.staging.chunks, 0);
    assert_eq!(c.fs(2).read(ino, 0, u32::MAX).unwrap(), data);
}

#[test]
fn a_chunks_shortfall_follows_how_many_nodes_hold_it() {
    let c = Cluster::with_offers(3, &[BIG, BIG, BIG, 0]);
    let ids = |ns: &[u32]| ns.iter().map(|n| NodeId(*n)).collect::<Vec<_>>();
    let copies = |c: &Cluster| {
        c.control(3)
            .status()
            .unwrap()
            .unreplicated
            .first()
            .map(|f| f.copies)
    };

    // The client reaches nobody: its staged copy is all there is.
    c.network().partition(&[&ids(&[3]), &ids(&[0, 1, 2])]);
    put(&c.fs(3), "a", &noise(60 * 1024, 1));
    c.sync();
    assert_eq!(copies(&c), Some(1));

    // One storage node reachable: still a single copy.
    c.network().partition(&[&ids(&[3, 0]), &ids(&[1, 2])]);
    c.advance(RETRY);
    c.sync();
    assert_eq!(copies(&c), Some(1));

    c.network().partition(&[&ids(&[3, 0, 1]), &ids(&[2])]);
    c.advance(RETRY);
    c.sync();
    assert_eq!(copies(&c), Some(2));
    assert_eq!(c.control(3).status().unwrap().staging.chunks, 1);

    c.network().heal();
    c.advance(RETRY);
    c.sync();
    assert_eq!(copies(&c), None);
    assert_eq!(c.control(3).status().unwrap().staging.chunks, 0);
    assert_eq!(replicas(&c), [1, 1, 1, 0]);
}

#[test]
fn full_nodes_are_skipped_in_placement_and_status_warns() {
    // Each node has room for 8 of the 100 KB chunks, 24 in all; 14 files want 28.
    let offer = 8 * 100_100;
    let c = Cluster::with_offers(2, &[offer, offer, offer, 0]);
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

#[test]
fn a_write_is_refused_only_when_the_local_node_has_no_space() {
    // The only node offers nothing, so no copy can be placed anywhere, and
    // its staging area holds just over one 100 KB file.
    let c = Cluster::start_with_config(1, "staging_bytes = 150000");
    c.control(0).create("home", 3).unwrap();
    c.control(0).set_offer(0).unwrap();
    let fs = c.fs(0);

    let kept = noise(100_000, 1);
    let ino = put(&fs, "kept", &kept);
    c.sync();
    let f = fs.create(ROOT_INO, "refused", 0o644).unwrap();
    assert_eq!(
        fs.write(f.ino, 0, &noise(100_000, 2)),
        Err(FsError::NoSpace)
    );
    assert_eq!(FsError::NoSpace.errno(), 28);
    // The refusal changed nothing: the first file is intact.
    assert_eq!(fs.read(ino, 0, u32::MAX).unwrap(), kept);
}

#[test]
fn a_storage_node_is_not_refused_just_because_the_staging_cap_is_reached() {
    // The cap holds just over one 100 KB file, but this node offers space and
    // replication has not yet moved the first file out of staging.
    let c = Cluster::start_with_config(1, "staging_bytes = 150000");
    c.control(0).create("home", 1).unwrap();
    let fs = c.fs(0);

    let first = noise(100_000, 1);
    let second = noise(100_000, 2);
    let a = put(&fs, "a", &first);
    let b = put(&fs, "b", &second);
    assert_eq!(fs.read(a, 0, u32::MAX).unwrap(), first);
    assert_eq!(fs.read(b, 0, u32::MAX).unwrap(), second);
}

#[test]
fn a_write_survives_a_crash_before_replication_and_replicates_after_restart() {
    let c = Cluster::with_offers(2, &[BIG, BIG, 0]);
    let data = noise(60 * 1024, 1);
    let ino = put(&c.fs(2), "a", &data);

    c.crash(2);
    c.restart(2);
    assert_eq!(c.fs(2).read(ino, 0, u32::MAX).unwrap(), data);
    c.advance(RETRY);
    c.sync();

    assert_eq!(replicas(&c), [1, 1, 0]);
    assert_eq!(c.control(2).status().unwrap().staging.chunks, 0);
}

#[test]
fn a_new_member_takes_over_only_its_share_and_nothing_moves_between_old_members() {
    // Nodes 0-2 store; node 3 joins later; node 4 is the pure client that writes.
    let c = Cluster::start(5);
    let token = c.control(0).create("home", 1).unwrap();
    for i in [1, 2] {
        c.control(i).join(&token, None).unwrap();
    }
    c.control(4).join(&token, Some(0)).unwrap();
    for i in 0..120 {
        put(&c.fs(4), &format!("f{i}"), &noise(8 * 1024, i + 1));
    }
    c.sync();
    let before = replicas(&c);
    assert_eq!(before[..3].iter().sum::<u64>(), 120, "{before:?}");

    c.control(3).join(&token, None).unwrap();
    for _ in 0..3 {
        c.advance(RETRY);
        c.sync();
    }

    let after = replicas(&c);
    assert_eq!(after.iter().sum::<u64>(), 120, "{after:?}");
    // A quarter of the space is new, so about 30 chunks move, all to node 3.
    assert!((15..=50).contains(&after[3]), "{after:?}");
    // Old members only gave chunks away: none took any from another.
    for i in 0..3 {
        assert!(after[i] <= before[i], "{before:?} -> {after:?}");
    }
}

#[test]
fn a_node_that_drops_its_offer_to_zero_hands_its_replicas_to_the_others() {
    let c = Cluster::with_offers(1, &[BIG, BIG, 0]);
    for i in 0..20 {
        put(&c.fs(2), &format!("f{i}"), &noise(8 * 1024, i + 1));
    }
    c.sync();
    assert!(replicas(&c)[0] > 0);

    c.control(0).set_offer(0).unwrap();
    for _ in 0..3 {
        c.advance(RETRY);
        c.sync();
    }

    assert_eq!(replicas(&c), [0, 20, 0]);
    // Everything is still readable.
    for i in 0..20 {
        let ino = c.fs(0).lookup(ROOT_INO, &format!("f{i}")).unwrap().ino;
        assert_eq!(
            c.fs(0).read(ino, 0, u32::MAX).unwrap(),
            noise(8 * 1024, i + 1)
        );
    }
}

#[test]
fn usable_size_is_the_offered_space_divided_by_the_copies_wanted() {
    let c = Cluster::with_offers(2, &[4000, 2000, 0]);
    // Pure clients add nothing; 6000 bytes offered, two copies of everything.
    assert_eq!(c.control(2).status().unwrap().usable_bytes, 3000);
}

fn replicas(c: &Cluster) -> Vec<u64> {
    (0..c.node_count())
        .map(|i| c.control(i).status().unwrap().usage.chunks)
        .collect()
}

#[test]
fn a_pure_clients_data_reaches_n_storage_nodes_and_its_staging_is_released() {
    let c = Cluster::with_offers(2, &[BIG, BIG, BIG, 0]);
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

#[test]
fn a_chunk_on_several_members_is_counted_once_cluster_wide() {
    let c = Cluster::with_offers(3, &[BIG, BIG, BIG, 0]);
    put(&c.fs(3), "a", &noise(60 * 1024, 1));
    c.sync();
    assert!(c.control(3).status().unwrap().unreplicated.is_empty());

    // Node 2 is lost: two nodes hold the chunk, three copies are wanted.
    c.crash(2);
    c.advance(31 * Duration::from_secs(60));
    c.sync();
    c.advance(HELLO_INTERVAL);
    c.sync();
    for node in [0, 1, 3] {
        assert_eq!(
            c.control(node).status().unwrap().unreplicated,
            [Shortfall {
                copies: 2,
                chunks: 1
            }],
            "node {node}"
        );
    }
}
