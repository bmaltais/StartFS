//! `startfs activity`: a node's recent traffic, queues and resource figures.

use startfs_core::activity::ActivityReport;
use startfs_core::control::NodeState;
use startfs_core::net::{NodeId, Transport};
use startfs_core::ops::*;
use startfs_core::protocol::Message;
use startfs_core::resources::{Cgroup, Pressure, Resources};
use startfs_harness::{Cluster, noise};
use std::time::Duration;

fn write(c: &Cluster, node: usize, name: &str, data: &[u8]) {
    let f = c.fs(node).create(ROOT_INO, name, 0o644).unwrap();
    c.fs(node).write(f.ino, 0, data).unwrap();
    c.fs(node).flush(f.ino).unwrap();
}

fn activity(c: &Cluster, node: usize) -> ActivityReport {
    c.control(node).activity().unwrap()
}

/// The row for the node at endpoint `peer`, as seen by node `viewer`.
fn row(report: &ActivityReport, c: &Cluster, viewer: usize, peer: usize) -> usize {
    let status = c.control(viewer).status().unwrap();
    let addr = &status.members[peer].addr;
    report
        .peers
        .iter()
        .position(|p| &p.addr == addr)
        .expect("every member has a row")
}

#[test]
fn traffic_is_counted_per_peer_path_and_matches_what_crossed_the_network() {
    let c = Cluster::with_filesystem(2, 2, "");
    write(&c, 0, "big", &noise(3 << 20, 1));
    c.sync();

    let report = activity(&c, 0);
    assert!(report.enabled);
    let i = row(&report, &c, 0, 1);
    let traffic = report.peers[i].traffic.expect("counters are on");
    let (sent, received): (u64, u64) =
        c.network()
            .traffic()
            .iter()
            .fold((0, 0), |(s, r), t| match (t.0, t.1) {
                (NodeId(0), NodeId(1)) => (s + t.2.len() as u64, r),
                (NodeId(1), NodeId(0)) => (s, r + t.2.len() as u64),
                _ => (s, r),
            });
    let total = |cats: &startfs_core::activity::Categories| {
        cats.chunk.bytes + cats.metadata.bytes + cats.gossip.bytes + cats.other.bytes
    };
    assert_eq!(total(&traffic.sent), sent);
    assert_eq!(total(&traffic.received), received);
    // The file went over as chunk data, and the two sides keep the log in step.
    assert!(traffic.sent.chunk.bytes >= 3 << 20);
    assert!(traffic.sent.metadata.messages > 0);
    assert!(traffic.sent.gossip.messages > 0);
    // The peer acknowledged what it was pushed.
    assert!(traffic.acknowledged > 0);
    assert_eq!(traffic.timed_out, 0);
}

#[test]
fn the_window_reports_how_long_it_has_collected_and_forgets_old_traffic() {
    let c = Cluster::with_filesystem(2, 2, "");
    write(&c, 0, "f", &noise(300 << 10, 2));
    c.sync();
    c.advance(Duration::from_secs(30));
    let report = activity(&c, 0);
    assert!(
        (report.window_secs - 30.0).abs() < 1.0,
        "{}",
        report.window_secs
    );
    let i = row(&report, &c, 0, 1);
    assert!(report.peers[i].traffic.unwrap().sent.chunk.bytes > 0);

    // Past a minute without anything new, the old traffic has left the window.
    c.advance(Duration::from_secs(40));
    let report = activity(&c, 0);
    assert_eq!(report.window_secs, 60.0);
    let i = row(&report, &c, 0, 1);
    assert_eq!(report.peers[i].traffic.unwrap().sent.chunk.bytes, 0);
}

#[test]
fn a_member_that_is_down_has_a_row_with_its_timeouts_and_the_queue_behind_it() {
    let c = Cluster::with_filesystem(3, 3, "");
    c.crash(2);
    write(&c, 0, "f", &noise(200 << 10, 3));
    for _ in 0..8 {
        c.advance(Duration::from_secs(6));
        c.sync();
    }
    let report = activity(&c, 0);
    let down = row(&report, &c, 0, 2);
    let peer = &report.peers[down];
    assert_eq!(peer.state, NodeState::Down);
    let traffic = peer.traffic.unwrap();
    assert!(traffic.timed_out > 0, "{traffic:?}");
    assert!(traffic.retried > 0);
    assert_eq!(traffic.received.chunk.messages, 0);
    assert!(peer.in_flight > 0);
    assert!(report.queue.chunks > 0);
    assert!(report.queue.oldest_secs.unwrap() >= 5);
    // The healthy peer answered everything it was asked.
    let up = row(&report, &c, 0, 1);
    assert_eq!(report.peers[up].traffic.unwrap().timed_out, 0);
    assert_eq!(report.peers[up].state, NodeState::Online);
}

#[test]
fn chunk_sizes_count_what_was_replicated() {
    let c = Cluster::with_filesystem(2, 2, "");
    write(&c, 0, "small", &noise(10 << 10, 4));
    write(&c, 0, "mid", &noise(50 << 10, 5));
    c.sync();
    let report = activity(&c, 0);
    let in_class = |up_to: u64| {
        report
            .chunk_sizes
            .iter()
            .find(|s| s.up_to_bytes == Some(up_to))
            .unwrap()
            .chunks
    };
    assert_eq!(in_class(16 << 10), 1);
    assert_eq!(in_class(64 << 10), 1);
    assert_eq!(report.chunk_sizes.last().unwrap().up_to_bytes, None);
}

#[test]
fn ticks_are_timed_on_the_injected_clock() {
    let c = Cluster::with_filesystem(2, 2, "");
    c.sync();
    c.advance(Duration::from_secs(1));
    c.sync();
    let tick = activity(&c, 0).tick.expect("ticks ran");
    assert!(tick.ticks > 0);
    // Simulated time stands still inside a tick, so none of it was busy.
    assert_eq!(tick.busy_fraction, 0.0);
    assert_eq!(tick.max_ms, 0.0);
}

#[test]
fn switched_off_nothing_is_counted_and_the_view_says_so() {
    let c = Cluster::with_filesystem(2, 2, "activity = false");
    write(&c, 0, "f", &noise(300 << 10, 6));
    c.sync();
    let report = activity(&c, 0);
    assert!(!report.enabled);
    assert!(report.tick.is_none());
    assert!(report.chunk_sizes.is_empty());
    assert!(report.peers.iter().all(|p| p.traffic.is_none()));

    // Switched on at run time, it starts a fresh window and counts from then.
    c.control(0).set_activity(true).unwrap();
    assert!(activity(&c, 0).enabled);
    write(&c, 0, "g", &noise(300 << 10, 7));
    c.sync();
    let report = activity(&c, 0);
    let i = row(&report, &c, 0, 1);
    assert!(report.peers[i].traffic.unwrap().sent.chunk.bytes > 0);

    c.control(0).set_activity(false).unwrap();
    assert!(!activity(&c, 0).enabled);
}

#[test]
fn resource_figures_the_os_does_not_give_are_unavailable_not_zero() {
    let c = Cluster::with_filesystem(2, 2, "");
    let none = activity(&c, 0).resources;
    assert_eq!(none, Resources::default());
    assert!(none.rss_bytes.is_none() && none.memory_pressure.is_none() && none.cgroup.is_none());
    assert!(none.io_pressure.is_none() && none.written_bytes.is_none());

    c.set_resources(
        0,
        Resources {
            rss_bytes: Some(2 << 30),
            threads: Some(12),
            mem_available_bytes: Some(1 << 30),
            swap_used_bytes: Some(0),
            memory_pressure: Some(Pressure {
                avg10: 1.5,
                avg60: 0.5,
            }),
            io_pressure: Some(Pressure {
                avg10: 70.0,
                avg60: 40.0,
            }),
            written_bytes: Some(0),
            cgroup: Some(Cgroup {
                memory_current_bytes: 3 << 30,
                memory_limit_bytes: None,
            }),
            ..Resources::default()
        },
    );
    let seen = activity(&c, 0).resources;
    assert_eq!(seen.rss_bytes, Some(2 << 30));
    // Present, and zero is a real reading; the ones never given stay absent.
    assert_eq!(seen.swap_used_bytes, Some(0));
    assert!(seen.cpu_secs.is_none() && seen.cpu_pressure.is_none());
    assert_eq!(seen.io_pressure.unwrap().avg10, 70.0);
    assert_eq!(seen.written_bytes, Some(0));
    assert_eq!(seen.cgroup.unwrap().memory_limit_bytes, None);
}

#[test]
fn the_inbox_figures_rise_with_a_burst_and_fall_as_it_is_handled() {
    let c = Cluster::with_filesystem(2, 2, "");
    let before = activity(&c, 1).structures;
    assert_eq!(before.inbox.unwrap().entries, 0);

    let sender = c.transport(0);
    for i in 0..50u8 {
        sender.send(
            NodeId(1),
            Message::ChunkReply([i; 32], Some(vec![i; 10_000])).encode(),
        );
    }
    let burst = activity(&c, 1).structures;
    let inbox = burst.inbox.unwrap();
    assert_eq!(inbox.entries, 50);
    assert!(inbox.bytes.unwrap() >= 500_000);
    // No bound is configured on it, and the figure says so.
    assert_eq!(inbox.limit_bytes, None);

    c.sync();
    let after = activity(&c, 1).structures;
    assert_eq!(after.inbox.unwrap().entries, 0);
    // Nobody asked for those chunks: they sit among the unclaimed replies.
    let replies = after.chunk_replies.unwrap();
    assert_eq!(replies.entries, 50);
    assert_eq!(replies.bytes, Some(500_000));
    // The decoded-chunk cache is bounded; the figure shows its limit.
    assert!(after.decoded_cache.unwrap().limit_bytes.is_some());
}
