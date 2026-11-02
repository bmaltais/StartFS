//! A member applies the new entries that reached it from several peers in one tick with one
//! durable commit, not one per peer: a build on one member reaches every other member from each
//! of them, and a commit per peer kept the weak members' tick busy syncing (#166). Counts the
//! process's write syscalls, so this file holds one test.
#![cfg(target_os = "linux")]

use startfs_core::ops::*;
use startfs_harness::Cluster;

fn write_syscalls() -> u64 {
    std::fs::read_to_string("/proc/self/io")
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("syscw: "))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

const RECEIVER: usize = 3;

/// Three writers each create a file; the receiver's ticks are counted. `together`: the three
/// writers' entries reach it before one tick; otherwise it ticks after each writer's.
fn receiver_writes(together: bool) -> u64 {
    let c = Cluster::start(4);
    let token = c.control(0).create("home", 2).unwrap();
    for i in 1..4 {
        c.control(i).join(&token, None).unwrap();
    }
    c.sync();
    let mut writes = 0;
    for (round, writer) in (0..3).enumerate() {
        c.fs(writer)
            .create(ROOT_INO, &format!("w{writer}-{round}"), 0o644)
            .unwrap();
        // The writer sends; nobody else ticks, so nothing is passed on.
        c.tick_one(writer);
        if !together {
            let before = write_syscalls();
            c.tick_one(RECEIVER);
            writes += write_syscalls() - before;
        }
    }
    if together {
        let before = write_syscalls();
        c.tick_one(RECEIVER);
        writes += write_syscalls() - before;
    }
    c.sync();
    for writer in 0..3 {
        assert!(
            c.fs(RECEIVER)
                .lookup(ROOT_INO, &format!("w{writer}-{writer}"))
                .is_ok(),
            "the receiver has writer {writer}'s file"
        );
    }
    writes
}

#[test]
fn entries_from_three_peers_in_one_tick_cost_one_commit() {
    let apart = receiver_writes(false);
    let together = receiver_writes(true);
    assert!(
        together * 2 <= apart,
        "{together} write syscalls for three peers' entries in one tick, {apart} in three ticks"
    );
}
