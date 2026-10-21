//! A build writes in pieces, patches and deletes files in a loop: what is staged afterwards,
//! and what is pushed to peers, stays within a small multiple of what stays live (#155).

use startfs_core::net::NodeId;
use startfs_core::ops::*;
use startfs_core::protocol::Message;
use startfs_harness::{Cluster, noise};
use std::time::Duration;

const FILE: usize = 2 << 20;
const PIECE: usize = 256 << 10;

#[test]
fn short_lived_data_costs_little() {
    let c = Cluster::with_offers(2, &[1 << 30, 1 << 30]);
    let fs = c.fs(0);
    let mut live = 0u64;
    for i in 0..20u64 {
        let name = format!("lib{i}.rlib");
        let f = fs.create(ROOT_INO, &name, 0o644).unwrap();
        for (k, part) in noise(FILE, i + 1).chunks(PIECE).enumerate() {
            fs.write(f.ino, (k * PIECE) as u64, part).unwrap();
            fs.flush(f.ino).unwrap();
        }
        // The header is patched when the file is finished.
        fs.write(f.ino, 0, b"patched-header").unwrap();
        fs.flush(f.ino).unwrap();
        c.elapse(Duration::from_secs(1));
        if i % 2 == 0 {
            fs.unlink(ROOT_INO, &name).unwrap();
        } else {
            live += FILE as u64;
        }
    }
    c.sync();

    let pushed: u64 = c
        .network()
        .traffic()
        .iter()
        .filter(|(from, _, _)| *from == NodeId(0))
        .filter_map(|(_, _, raw)| match Message::decode(raw) {
            Ok(Message::ChunkPush(_, Some(data))) => Some(data.len() as u64),
            _ => None,
        })
        .sum();
    let staged = c.control(0).status().unwrap().staging.bytes;
    assert_eq!(staged, 0);
    // Two copies, so one push per live byte; files deleted within a second may still be pushed.
    assert!(pushed <= 3 * live, "pushed {pushed} bytes for {live} live");
}
