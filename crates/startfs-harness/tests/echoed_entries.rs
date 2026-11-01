//! A peer does not send a writer its own entries back (#210 follow-up). A peer learned what the
//! writer holds only from its hellos, which lag a busy writer, so it returned every new entry to
//! its author, and the writer paid a durable commit for each message of them.

use startfs_core::net::NodeId;
use startfs_core::ops::*;
use startfs_core::protocol::Message;
use startfs_harness::Cluster;
use std::time::Duration;

#[test]
fn a_writer_is_not_sent_its_own_entries() {
    let c = Cluster::start(3);
    let token = c.control(0).create("home", 3).unwrap();
    c.control(1).join(&token, None).unwrap();
    c.control(2).join(&token, None).unwrap();
    c.sync();

    let start = c.network().traffic().len();
    let fs = c.fs(0);
    for round in 0..50 {
        for i in 0..20 {
            fs.create(ROOT_INO, &format!("f{round}-{i}"), 0o644)
                .unwrap();
        }
        c.advance(Duration::from_millis(100));
        c.tick();
    }
    c.sync();

    let echoed: usize = c.network().traffic()[start..]
        .iter()
        .filter(|(from, to, _)| *from != NodeId(0) && *to == NodeId(0))
        .filter_map(|(_, _, raw)| match Message::decode(raw) {
            Ok(Message::Entries(entries)) => Some(entries.len()),
            _ => None,
        })
        .sum();
    assert_eq!(
        echoed, 0,
        "peers sent the writer {echoed} of its own entries"
    );
}
