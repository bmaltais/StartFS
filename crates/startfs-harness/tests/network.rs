//! The harness itself: simulated network, crash/restart and clock control.
//! (No cluster protocol exists yet, so these exercise the harness's own
//! transport and fault-injection surface directly.)

use startfs_core::net::{NodeId, Transport};
use startfs_harness::Cluster;
use std::time::Duration;

const A: NodeId = NodeId(0);
const B: NodeId = NodeId(1);
const C: NodeId = NodeId(2);

#[test]
fn starts_n_nodes_with_independent_filesystems() {
    let c = Cluster::start(3);
    assert_eq!(c.node_count(), 3);
    c.fs(0);
    c.fs(2);
}

#[test]
fn messages_flow_between_connected_nodes() {
    let c = Cluster::start(3);
    c.transport(0).send(B, b"hi".to_vec());
    assert_eq!(c.transport(1).recv(), Some((A, b"hi".to_vec())));
    assert_eq!(c.transport(1).recv(), None);
    assert_eq!(c.transport(2).recv(), None);
}

#[test]
fn partitions_block_traffic_until_healed() {
    let c = Cluster::start(3);
    c.network().partition(&[&[A, B], &[C]]);
    c.transport(0).send(C, b"lost".to_vec());
    c.transport(0).send(B, b"ok".to_vec());
    assert_eq!(c.transport(2).recv(), None);
    assert!(c.transport(1).recv().is_some());

    c.network().heal();
    c.transport(0).send(C, b"back".to_vec());
    assert_eq!(c.transport(2).recv(), Some((A, b"back".to_vec())));
}

#[test]
fn a_partition_cuts_off_messages_already_in_flight() {
    let c = Cluster::start(2);
    c.network().set_delay(A, B, Duration::from_secs(5));
    c.transport(0).send(B, b"x".to_vec());
    c.network().partition(&[&[A], &[B]]);
    c.advance(Duration::from_secs(10));
    assert_eq!(c.transport(1).recv(), None);
}

#[test]
fn delayed_messages_arrive_only_after_the_clock_advances() {
    let c = Cluster::start(2);
    c.network().set_delay(A, B, Duration::from_secs(5));
    c.transport(0).send(B, b"slow".to_vec());
    assert_eq!(c.transport(1).recv(), None);
    c.advance(Duration::from_secs(4));
    assert_eq!(c.transport(1).recv(), None);
    c.advance(Duration::from_secs(1));
    assert_eq!(c.transport(1).recv(), Some((A, b"slow".to_vec())));
}

#[test]
fn dropped_links_lose_messages_without_affecting_other_links() {
    let c = Cluster::start(3);
    c.network().set_drop(A, B, true);
    c.transport(0).send(B, b"gone".to_vec());
    c.transport(0).send(C, b"fine".to_vec());
    assert_eq!(c.transport(1).recv(), None);
    assert!(c.transport(2).recv().is_some());

    c.network().set_drop(A, B, false);
    c.transport(0).send(B, b"now".to_vec());
    assert!(c.transport(1).recv().is_some());
}

#[test]
fn crashed_nodes_receive_nothing_and_restart_clean() {
    let c = Cluster::start(2);
    c.transport(0).send(B, b"before".to_vec());
    c.crash(1);
    c.transport(0).send(B, b"during".to_vec());
    c.restart(1);
    assert_eq!(c.transport(1).recv(), None);
    c.transport(0).send(B, b"after".to_vec());
    assert_eq!(c.transport(1).recv(), Some((A, b"after".to_vec())));
}

#[test]
fn the_clock_only_moves_when_advanced() {
    let c = Cluster::start(1);
    let t = c.now();
    assert_eq!(c.now(), t);
    c.advance(Duration::from_secs(1800));
    assert_eq!(c.now(), t + 1800 * 1_000_000_000);
}
