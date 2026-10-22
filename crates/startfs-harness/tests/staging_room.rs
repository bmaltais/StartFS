//! Writes run at local speed while chunks trickle out to a slow member: the staging area is
//! bounded by the free space of its partition, and only past that bound are writes refused (#148).

use startfs_core::net::NodeId;
use startfs_core::ops::*;
use startfs_core::resources::Resources;
use startfs_harness::{Cluster, noise};
use std::time::Duration;

const FILE: usize = 3 << 20;
const GIB: u64 = 1 << 30;
const SLOW: NodeId = NodeId(1);

fn free(bytes: u64) -> Resources {
    Resources {
        disk_free_bytes: Some(bytes),
        ..Resources::default()
    }
}

/// Node 0 writes `n` files of [`FILE`] bytes; returns the first error and the time it took.
fn write_files(c: &Cluster, from: usize, n: usize) -> (FsResult<()>, Duration) {
    let fs = c.fs(0);
    let started = c.now();
    let mut out = Ok(());
    for i in from..from + n {
        let step = fs
            .create(ROOT_INO, &format!("f{i}"), 0o644)
            .and_then(|f| fs.write(f.ino, 0, &noise(FILE, i as u64 + 1)).map(|_| f))
            .and_then(|f| fs.flush(f.ino));
        if let Err(e) = step {
            out = Err(e);
            break;
        }
    }
    (out, Duration::from_nanos(c.now() - started))
}

#[test]
fn a_slow_peer_never_slows_a_writer_that_has_room() {
    let c = Cluster::with_offers(2, &[GIB, GIB]);
    c.network()
        .set_delay(NodeId(0), SLOW, Duration::from_secs(3600));
    c.set_resources(0, free(100 * GIB));

    let (result, took) = write_files(&c, 0, 8);
    assert_eq!(result, Ok(()));
    assert!(took < Duration::from_millis(500), "writes took {took:?}");
    c.tick();
    assert!(c.control(0).status().unwrap().staging.chunks > 0);
}

#[test]
fn past_the_free_space_bound_writes_are_refused_until_room_returns() {
    // The node offers a gibibyte of replica space: the bound is the disk, not the offer.
    let c = Cluster::with_offers(2, &[GIB, GIB]);
    c.network()
        .set_delay(NodeId(0), SLOW, Duration::from_secs(3600));
    // Two gibibytes of headroom plus a little room: about 4 MiB may be staged.
    c.set_resources(0, free(2 * GIB + (5 << 20)));
    c.tick();

    let (result, _) = write_files(&c, 0, 8);
    assert_eq!(result, Err(FsError::NoSpace));

    c.set_resources(0, free(100 * GIB));
    c.tick();
    let (result, _) = write_files(&c, 100, 2);
    assert_eq!(result, Ok(()));
}

#[test]
fn status_names_the_member_the_backlog_waits_on() {
    let c = Cluster::with_offers(3, &[GIB, GIB, GIB]);
    c.network()
        .set_delay(NodeId(0), SLOW, Duration::from_secs(3600));
    c.set_resources(0, free(100 * GIB));
    write_files(&c, 0, 4).0.unwrap();
    c.advance(Duration::from_secs(5));
    c.sync();

    let staging = c.control(0).status().unwrap().staging;
    assert_eq!(staging.waiting_on.map(|(m, _)| m.0), Some(SLOW.0));
}
