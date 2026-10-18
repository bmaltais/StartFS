//! A busy node is not a dead one: peers keep seeing it while its background
//! work is stuck behind slow disk or a long queue.

use startfs_core::control::NodeState;
use startfs_harness::Cluster;
use std::time::Duration;

fn state_of(c: &Cluster, viewer: usize, member: usize) -> NodeState {
    c.control(viewer).status().unwrap().members[member].state
}

#[test]
fn peers_keep_seeing_a_node_whose_background_work_is_stuck() {
    let c = Cluster::start(3);
    let token = c.control(0).create("home", 2).unwrap();
    c.control(1).join(&token, Some(0)).unwrap();
    c.control(2).join(&token, Some(0)).unwrap();
    c.sync();

    // Node 1 is stuck in a long tick; only its liveness traffic still runs.
    for _ in 0..12 {
        c.advance(Duration::from_secs(5));
        c.tick_except(1);
        c.keepalive(1);
    }

    assert_eq!(state_of(&c, 0, 1), NodeState::Online);
    assert_eq!(state_of(&c, 2, 1), NodeState::Online);
}
