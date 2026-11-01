//! Log exchange over a link slower than the hello interval. A hello says what the peer held
//! when it was sent, so it lags what is in flight; resending from it on a slow link queued the
//! same entries over and over, until the daemon ran out of memory.

use startfs_core::net::NodeId;
use startfs_core::ops::*;
use startfs_core::protocol::Message;
use startfs_harness::Cluster;
use std::collections::HashSet;
use std::time::Duration;

#[test]
fn entries_in_flight_on_a_slow_link_are_not_sent_again() {
    let c = Cluster::start(2);
    let token = c.control(0).create("home", 2).unwrap();
    c.control(1).join(&token, None).unwrap();
    c.sync();

    // Messages take longer to arrive than the hellos take to repeat.
    c.network()
        .set_delay(NodeId(0), NodeId(1), Duration::from_secs(12));
    let start = c.network().traffic().len();
    let fs = c.fs(0);
    for i in 0..3000 {
        fs.create(ROOT_INO, &format!("tmp{i}"), 0o644).unwrap();
    }
    for _ in 0..120 {
        c.advance(Duration::from_secs(1));
        c.tick();
    }

    let (mut sent, mut distinct) = (0usize, HashSet::new());
    for (from, _, raw) in &c.network().traffic()[start..] {
        if *from == NodeId(0)
            && let Ok(Message::Entries(entries)) = Message::decode(raw)
        {
            sent += entries.len();
            distinct.extend(entries.iter().map(|e| (e.origin, e.seq)));
        }
    }
    assert_eq!(distinct.len(), 3000);
    assert_eq!(sent, distinct.len(), "entries were sent more than once");
}
