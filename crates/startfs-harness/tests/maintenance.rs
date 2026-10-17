//! Background maintenance: garbage collection, scrub and log compaction,
//! driven through FsOps and the control interface on a simulated cluster
//! with a manual clock.

use startfs_core::control::ScrubState;
use startfs_core::net::NodeId;
use startfs_core::ops::*;
use startfs_harness::Cluster;
use std::time::Duration;

const BIG: u64 = 10 << 30;
const HOUR: Duration = Duration::from_secs(3600);
const DAY: Duration = Duration::from_secs(24 * 3600);

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

fn cluster_with(copies: u32, nodes: usize, config: &str) -> Cluster {
    let c = Cluster::start_with_config(nodes, config);
    let token = c.control(0).create("home", copies).unwrap();
    for i in 1..nodes {
        c.control(i).join(&token, Some(BIG)).unwrap();
    }
    c.sync();
    c
}

fn cluster(copies: u32, nodes: usize) -> Cluster {
    cluster_with(copies, nodes, "")
}

fn put(fs: &impl FsOps, name: &str, data: &[u8]) -> Ino {
    let f = fs.create(ROOT_INO, name, 0o644).unwrap();
    assert_eq!(fs.write(f.ino, 0, data).unwrap(), data.len());
    f.ino
}

/// Chunks held as replicas across the whole cluster, as `status` reports them.
fn total_replicas(c: &Cluster) -> u64 {
    (0..c.node_count())
        .map(|i| c.control(i).status().map_or(0, |s| s.usage.chunks))
        .sum()
}

/// Let `by` pass in steps, letting the cluster work after each.
fn pass(c: &Cluster, by: Duration, step: Duration) {
    let mut left = by;
    while !left.is_zero() {
        let s = step.min(left);
        c.advance(s);
        c.sync();
        left -= s;
    }
}

#[test]
fn deleted_data_is_kept_for_the_retention_period_and_then_reclaimed() {
    let c = cluster(2, 3);
    put(&c.fs(0), "doomed", &noise(8 * 1024, 1));
    c.sync();
    let held = total_replicas(&c);
    assert!(held >= 2, "{held}");

    c.fs(0).unlink(ROOT_INO, "doomed").unwrap();
    pass(&c, 6 * DAY, 12 * HOUR);
    assert_eq!(total_replicas(&c), held, "reclaimed before retention ended");

    pass(&c, 2 * DAY, 12 * HOUR);
    assert_eq!(total_replicas(&c), 0);
}

#[test]
fn overwritten_data_is_reclaimed_but_chunks_still_in_use_are_kept() {
    let c = cluster(2, 3);
    let shared = noise(8 * 1024, 2);
    put(&c.fs(0), "a", &shared);
    let b = put(&c.fs(0), "b", &shared);
    let d = put(&c.fs(0), "d", &noise(8 * 1024, 3));
    c.sync();
    assert_eq!(total_replicas(&c), 4); // shared + d's, two copies each

    // `b` still shares its old chunk with `a`; `d`'s old chunk is nobody's.
    c.fs(0).write(b, 0, &noise(8 * 1024, 4)).unwrap();
    c.fs(0).write(d, 0, &noise(8 * 1024, 5)).unwrap();
    c.sync();
    assert_eq!(total_replicas(&c), 8);
    pass(&c, 9 * DAY, 12 * HOUR);

    let a = c.fs(1).lookup(ROOT_INO, "a").unwrap();
    assert_eq!(c.fs(1).read(a.ino, 0, u32::MAX).unwrap(), shared);
    assert_eq!(total_replicas(&c), 6);
}

#[test]
fn the_retention_period_is_configurable() {
    let c = cluster_with(2, 3, "gc_retention_secs = 7200");
    put(&c.fs(1), "tmp", &noise(8 * 1024, 6));
    c.sync();
    c.fs(1).unlink(ROOT_INO, "tmp").unwrap();

    pass(&c, HOUR, Duration::from_secs(1200));
    assert!(total_replicas(&c) > 0);
    pass(&c, 3 * HOUR, Duration::from_secs(1200));
    assert_eq!(total_replicas(&c), 0);
}

#[test]
fn a_stale_node_returning_after_the_reclaim_cannot_resurrect_deleted_data() {
    let c = cluster(3, 3);
    put(&c.fs(0), "doomed", &noise(8 * 1024, 7));
    let keeper_data = noise(8 * 1024, 8);
    put(&c.fs(0), "keeper", &keeper_data);
    c.sync();
    assert_eq!(total_replicas(&c), 6);

    c.crash(2);
    c.fs(0).unlink(ROOT_INO, "doomed").unwrap();
    pass(&c, 9 * DAY, 12 * HOUR);
    assert_eq!(total_replicas(&c), 2, "the live nodes reclaimed the orphan");

    c.restart(2);
    pass(&c, 2 * HOUR, Duration::from_secs(600));
    for i in 0..3 {
        assert_eq!(
            c.fs(i).lookup(ROOT_INO, "doomed").unwrap_err(),
            FsError::NotFound,
            "node {i}"
        );
        let k = c.fs(i).lookup(ROOT_INO, "keeper").unwrap();
        assert_eq!(c.fs(i).read(k.ino, 0, u32::MAX).unwrap(), keeper_data);
    }

    // Whatever the stale node still held is garbage too, and goes in time.
    pass(&c, 9 * DAY, 12 * HOUR);
    assert_eq!(total_replicas(&c), 3);
    for i in 0..3 {
        assert!(c.control(i).status().unwrap().unreplicated.is_empty());
    }
}

#[test]
fn scrub_finds_a_corrupt_replica_and_heals_it_from_another_copy() {
    let c = cluster(3, 3);
    let data = noise(20 * 1024, 9);
    let ino = put(&c.fs(0), "precious", &data);
    c.sync();

    c.corrupt_chunks(0);
    pass(&c, 31 * DAY, DAY);

    // Cut node 0 off: only a healed local replica can still serve the file.
    c.network()
        .partition(&[&[NodeId(0)], &[NodeId(1), NodeId(2)]]);
    assert_eq!(c.fs(0).read(ino, 0, u32::MAX).unwrap(), data);
}

#[test]
fn scrub_pauses_on_battery_power_and_carries_on_when_plugged_in() {
    let c = cluster(3, 3);
    let data = noise(20 * 1024, 10);
    let ino = put(&c.fs(0), "precious", &data);
    c.sync();
    c.corrupt_chunks(0);

    c.set_on_battery(0, true);
    pass(&c, 31 * DAY, DAY);
    assert_eq!(
        c.control(0).status().unwrap().scrub.state,
        ScrubState::PausedOnBattery
    );
    c.network()
        .partition(&[&[NodeId(0)], &[NodeId(1), NodeId(2)]]);
    assert!(c.fs(0).read(ino, 0, u32::MAX).is_err(), "scrubbed on battery");
    c.network().heal();

    c.set_on_battery(0, false);
    pass(&c, HOUR, Duration::from_secs(600));
    assert_eq!(c.control(0).status().unwrap().scrub.state, ScrubState::Idle);
    c.network()
        .partition(&[&[NodeId(0)], &[NodeId(1), NodeId(2)]]);
    assert_eq!(c.fs(0).read(ino, 0, u32::MAX).unwrap(), data);
}

/// Files enough that a scrub pass needs several rounds.
fn many_files(c: &Cluster, count: u64) {
    for i in 0..count {
        put(&c.fs(0), &format!("f{i}"), &noise(8 * 1024, 100 + i));
    }
    c.sync();
}

/// Run rounds until a scrub pass is under way on node 0.
fn until_scrubbing(c: &Cluster) {
    for _ in 0..50 {
        if c.control(0).status().unwrap().scrub.state == ScrubState::Running {
            return;
        }
        c.tick();
    }
    panic!("the scrub never started");
}

#[test]
fn a_scrub_pass_is_spread_over_many_rounds() {
    let c = cluster(2, 2);
    many_files(&c, 40);
    let before = c.control(0).status().unwrap().scrub.last_pass;

    c.advance(31 * DAY);
    until_scrubbing(&c);
    let first = c.control(0).status().unwrap().scrub;
    assert_eq!(first.last_pass, before);
    assert!(first.remaining > 0);

    c.sync();
    let done = c.control(0).status().unwrap().scrub;
    assert_eq!(done.state, ScrubState::Idle);
    assert!(done.last_pass > before, "{before:?} -> {:?}", done.last_pass);
}

#[test]
fn the_scrub_waits_while_replication_has_work_to_do() {
    let c = cluster(2, 2);
    many_files(&c, 40);
    c.advance(31 * DAY);
    until_scrubbing(&c);
    let remaining = |c: &Cluster| c.control(0).status().unwrap().scrub.remaining;
    let started = remaining(&c);

    // A fresh write is still being replicated: the round goes to that.
    let f = c.fs(0).lookup(ROOT_INO, "f0").unwrap();
    c.fs(0).write(f.ino, 0, &noise(8 * 1024, 999)).unwrap();
    c.tick();
    assert_eq!(remaining(&c), started);

    c.sync();
    assert_eq!(c.control(0).status().unwrap().scrub.state, ScrubState::Idle);
}
