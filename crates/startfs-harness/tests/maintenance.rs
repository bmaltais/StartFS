//! Background maintenance: garbage collection, scrub and log compaction,
//! driven through FsOps and the control interface on a simulated cluster
//! with a manual clock.

use startfs_core::control::ScrubState;
use startfs_core::net::NodeId;
use startfs_core::ops::*;
use startfs_harness::{Cluster, noise};
use std::time::Duration;

const HOUR: Duration = Duration::from_secs(3600);
const DAY: Duration = Duration::from_secs(24 * 3600);

fn cluster(copies: u32, nodes: usize) -> Cluster {
    Cluster::with_filesystem(copies, nodes, "")
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
fn a_linked_files_data_is_kept_until_its_last_name_is_gone_and_then_reclaimed() {
    let c = cluster(2, 3);
    let ino = put(&c.fs(0), "a", &noise(8 * 1024, 1));
    c.fs(0).link(ino, ROOT_INO, "b").unwrap();
    c.sync();
    let held = total_replicas(&c);
    assert!(held >= 2, "{held}");

    c.fs(0).unlink(ROOT_INO, "a").unwrap();
    pass(&c, 10 * DAY, 12 * HOUR);
    assert_eq!(total_replicas(&c), held, "reclaimed while a name remained");
    let b = c.fs(1).lookup(ROOT_INO, "b").unwrap();
    assert_eq!(
        c.fs(1).read(b.ino, 0, u32::MAX).unwrap(),
        noise(8 * 1024, 1)
    );

    c.fs(0).unlink(ROOT_INO, "b").unwrap();
    pass(&c, 10 * DAY, 12 * HOUR);
    assert_eq!(total_replicas(&c), 0);
}

#[test]
fn a_linked_file_that_is_fully_unlinked_leaves_no_metadata_behind() {
    let c = cluster(2, 3);
    let ino = put(&c.fs(0), "a", b"x");
    c.fs(0).link(ino, ROOT_INO, "b").unwrap();
    c.sync();
    c.fs(0).unlink(ROOT_INO, "a").unwrap();
    c.fs(0).unlink(ROOT_INO, "b").unwrap();
    pass(&c, 6 * HOUR, Duration::from_secs(1800));
    for i in 0..3 {
        assert!(names(&c.fs(i)).is_empty(), "node {i}");
        assert!(c.control(i).status().unwrap().log.objects < 3, "node {i}");
    }
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
    let c = Cluster::with_filesystem(2, 3, "gc_retention_secs = 7200");
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
    // The reclaim gate holds the orphan until node 2 is back and caught up.
    assert_eq!(total_replicas(&c), 4, "reclaimed while node 2 was away");

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
    assert!(
        c.fs(0).read(ino, 0, u32::MAX).is_err(),
        "scrubbed on battery"
    );
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
    assert!(
        done.last_pass > before,
        "{before:?} -> {:?}",
        done.last_pass
    );
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

#[test]
fn the_log_stays_bounded_however_much_is_written() {
    let c = cluster(2, 3);
    let f = put(&c.fs(0), "busy", b"v0");
    for i in 0..200 {
        c.fs(i % 3).write(f, 0, format!("v{i}").as_bytes()).unwrap();
        c.sync();
    }
    let log = |c: &Cluster, i| c.control(i).status().unwrap().log.entries;
    assert!(log(&c, 0) > 200);

    pass(&c, 2 * HOUR, Duration::from_secs(1800));
    for i in 0..3 {
        assert!(log(&c, i) < 10, "node {i}: {}", log(&c, i));
    }
    // Compaction loses nothing.
    let last = c.fs(1).read(f, 0, u32::MAX).unwrap();
    assert_eq!(last, b"v199");
    assert_eq!(c.fs(2).read(f, 0, u32::MAX).unwrap(), b"v199");
}

fn names(fs: &impl FsOps) -> Vec<String> {
    let mut v: Vec<String> = fs
        .readdir(ROOT_INO)
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    v.sort();
    v
}

#[test]
fn a_node_offline_across_a_compaction_still_converges() {
    let c = cluster(2, 3);
    put(&c.fs(0), "old", b"before");
    put(&c.fs(0), "doomed", b"soon gone");
    c.sync();

    c.network()
        .partition(&[&[NodeId(0), NodeId(1)], &[NodeId(2)]]);
    // Node 2 carries on alone while the others change plenty and compact.
    put(&c.fs(2), "from-the-island", b"offline work");
    c.fs(0).unlink(ROOT_INO, "doomed").unwrap();
    put(&c.fs(1), "new", b"while you were away");
    let old = c.fs(0).lookup(ROOT_INO, "old").unwrap().ino;
    c.fs(0).write(old, 0, b"after!").unwrap();
    pass(&c, 3 * HOUR, Duration::from_secs(1800));
    assert!(c.control(0).status().unwrap().log.entries < 3);

    c.network().heal();
    pass(&c, HOUR, Duration::from_secs(600));

    let want = vec!["from-the-island", "new", "old"];
    for i in 0..3 {
        assert_eq!(names(&c.fs(i)), want, "node {i}");
        let o = c.fs(i).lookup(ROOT_INO, "old").unwrap();
        assert_eq!(c.fs(i).read(o.ino, 0, u32::MAX).unwrap(), b"after!");
        let n = c.fs(i).lookup(ROOT_INO, "from-the-island").unwrap();
        assert_eq!(c.fs(i).read(n.ino, 0, u32::MAX).unwrap(), b"offline work");
    }
}

#[test]
fn deleted_files_leave_no_metadata_behind_once_every_member_has_caught_up() {
    let c = cluster(2, 3);
    for i in 0..20 {
        put(&c.fs(0), &format!("f{i}"), b"x");
    }
    c.sync();
    let objects = |c: &Cluster, i| c.control(i).status().unwrap().log.objects;
    assert!(objects(&c, 0) >= 20);

    // Node 2 has not heard about the deletes, so tombstones must stay.
    c.network()
        .partition(&[&[NodeId(0), NodeId(1)], &[NodeId(2)]]);
    for i in 0..20 {
        c.fs(0).unlink(ROOT_INO, &format!("f{i}")).unwrap();
    }
    pass(&c, 3 * HOUR, Duration::from_secs(1800));
    assert!(objects(&c, 0) >= 20, "tombstones dropped too early");

    c.network().heal();
    pass(&c, 3 * HOUR, Duration::from_secs(1800));
    for i in 0..3 {
        assert!(names(&c.fs(i)).is_empty(), "node {i} resurrected files");
        assert!(objects(&c, i) < 5, "node {i}: {}", objects(&c, i));
    }
}

/// The members (by id) that node `i` says GC is waiting on.
fn gc_waiting_on(c: &Cluster, i: usize) -> Vec<startfs_core::members::MemberId> {
    c.control(i).status().unwrap().gc.waiting_on
}

fn member_id(c: &Cluster, i: usize) -> startfs_core::members::MemberId {
    let s = c.control(i).status().unwrap();
    s.members.iter().find(|m| m.is_self).unwrap().id
}

#[test]
fn gc_waits_for_a_partitioned_member_before_reclaiming() {
    let c = Cluster::with_filesystem(2, 3, "gc_retention_secs = 7200");
    put(&c.fs(0), "doomed", &noise(8 * 1024, 11));
    pass(&c, HOUR, Duration::from_secs(1200));
    let on = |c: &Cluster, i| c.control(i).status().unwrap().usage.chunks;
    // The chunk is held by two nodes (A and B); the third (C) is cut off.
    let cut = (0..3)
        .find(|&i| on(&c, i) == 0)
        .expect("a node without a copy");
    let rest: Vec<NodeId> = (0..3)
        .filter(|&i| i != cut)
        .map(|i| NodeId(i as u32))
        .collect();
    let [a, b] = [rest[0].0 as usize, rest[1].0 as usize];
    assert_eq!(on(&c, a) + on(&c, b), 2);

    c.network().partition(&[&rest, &[NodeId(cut as u32)]]);
    c.fs(a).unlink(ROOT_INO, "doomed").unwrap();
    pass(&c, 3 * HOUR, Duration::from_secs(1200));

    let c_id = member_id(&c, cut);
    for i in [a, b] {
        let gc = c.control(i).status().unwrap().gc;
        assert_eq!(
            on(&c, i),
            1,
            "node {i} reclaimed while node {cut} was cut off"
        );
        assert_eq!(gc.pending_chunks, 1, "node {i}");
        assert_eq!(gc.reclaimed_chunks, 0, "node {i}");
        assert_eq!(gc_waiting_on(&c, i), vec![c_id], "node {i}");
    }

    c.network().heal();
    pass(&c, 3 * HOUR, Duration::from_secs(1200));
    assert_eq!(total_replicas(&c), 0);
    for i in [a, b] {
        let gc = c.control(i).status().unwrap().gc;
        assert_eq!((gc.reclaimed_chunks, gc.pending_chunks), (1, 0), "node {i}");
        assert!(gc.waiting_on.is_empty(), "node {i}");
    }
}

#[test]
fn content_rewritten_on_the_far_side_of_a_partition_is_not_lost() {
    let c = Cluster::with_filesystem(2, 3, "gc_retention_secs = 7200");
    let data = noise(8 * 1024, 12);
    put(&c.fs(0), "old", &data);
    c.sync();

    c.network()
        .partition(&[&[NodeId(0), NodeId(1)], &[NodeId(2)]]);
    c.fs(0).unlink(ROOT_INO, "old").unwrap();
    put(&c.fs(2), "again", &data);
    pass(&c, 3 * HOUR, Duration::from_secs(1200));

    c.network().heal();
    pass(&c, 12 * HOUR, Duration::from_secs(1200));
    for i in 0..3 {
        let f = c.fs(i).lookup(ROOT_INO, "again").unwrap();
        assert_eq!(c.fs(i).read(f.ino, 0, u32::MAX).unwrap(), data, "node {i}");
        assert!(c.control(i).status().unwrap().unreplicated.is_empty());
    }
    assert!(total_replicas(&c) >= 2);
}
