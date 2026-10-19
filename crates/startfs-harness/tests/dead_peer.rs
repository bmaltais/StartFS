//! A peer that stops answering (frozen process, dead link) is asked less and less often, not
//! every retry delay for as long as it stays silent (#91).

use startfs_core::net::NodeId;
use startfs_core::protocol::Message;
use startfs_harness::{Cluster, noise};
use std::time::Duration;

const FROZEN: NodeId = NodeId(2);

fn pushes_to_frozen(c: &Cluster) -> usize {
    c.network()
        .traffic()
        .iter()
        .filter(|(from, to, _)| *from == NodeId(0) && *to == FROZEN)
        .filter(|(_, _, raw)| matches!(Message::decode(raw), Ok(Message::ChunkPush(..))))
        .count()
}

#[test]
fn a_silent_peer_is_asked_less_and_less_often() {
    let c = Cluster::with_offers(3, &[1 << 30, 1 << 30, 1 << 30]);
    // Nothing sent to the frozen node comes back within the hour.
    c.network()
        .set_delay(NodeId(0), FROZEN, Duration::from_secs(3600));
    let fs = c.fs(0);
    for i in 0..20 {
        use startfs_core::ops::*;
        let f = fs.create(ROOT_INO, &format!("f{i}"), 0o644).unwrap();
        fs.write(f.ino, 0, &noise(8 * 1024, i + 1)).unwrap();
    }

    let mut per_step = Vec::new();
    for _ in 0..12 {
        let before = pushes_to_frozen(&c);
        c.advance(Duration::from_secs(5));
        c.sync();
        per_step.push(pushes_to_frozen(&c) - before);
    }
    eprintln!("{per_step:?}");
    let early: usize = per_step[..4].iter().sum();
    let late: usize = per_step[8..].iter().sum();
    assert!(early > 0, "no asks at all: {per_step:?}");
    assert!(late * 2 <= early, "still asked as often: {per_step:?}");
}
