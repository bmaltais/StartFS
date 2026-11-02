//! A member that keeps receiving new entries makes them durable in groups, not once per tick: a
//! durable commit takes ~6 ms on the weak members' disks, and one per tick kept their tick busy
//! through a build (#166). Counts the process's write syscalls (a lazy commit makes none), so this
//! file holds one test.
#![cfg(target_os = "linux")]

use startfs_core::ops::*;
use startfs_harness::Cluster;
use std::time::Duration;

const RECEIVER: usize = 1;

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

/// Write syscalls of the receiver's ticks while a writer sends new entries `gap` apart.
fn receiver_writes(gap: Duration) -> u64 {
    let c = Cluster::start(3);
    let token = c.control(0).create("home", 3).unwrap();
    c.control(1).join(&token, None).unwrap();
    c.control(2).join(&token, None).unwrap();
    c.sync();
    let mut writes = 0;
    for round in 0..10 {
        c.fs(0)
            .create(ROOT_INO, &format!("f{round}"), 0o644)
            .unwrap();
        c.tick_one(0);
        c.advance(gap);
        let before = write_syscalls();
        c.tick_one(RECEIVER);
        writes += write_syscalls() - before;
    }
    c.sync();
    assert!(
        c.fs(RECEIVER).lookup(ROOT_INO, "f9").is_ok(),
        "the receiver has every file"
    );
    writes
}

#[test]
fn a_steady_stream_of_entries_is_made_durable_in_groups() {
    let spread = receiver_writes(Duration::from_millis(150));
    let stream = receiver_writes(Duration::from_millis(5));
    assert!(
        stream * 3 <= spread,
        "{stream} write syscalls for 10 batches 5 ms apart, {spread} for 10 batches 150 ms apart"
    );
}
