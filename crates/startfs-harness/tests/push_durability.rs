//! A chunk a member acknowledges is on its disk: the sender drops its own copy on the strength of
//! the ack, so a power cut at the receiver must not take back anything it acknowledged (#166).

use startfs_core::net::NodeId;
use startfs_core::ops::*;
use startfs_harness::{Cluster, noise};

const BIG: u64 = 10 << 30;

#[test]
fn a_burst_of_pushes_acknowledged_survives_a_power_cut_at_the_receiver() {
    let c = Cluster::with_offers(2, &[BIG, BIG]);
    let files: Vec<(Ino, Vec<u8>)> = (0..60u64)
        .map(|i| {
            let data = noise(100 * 1024, i + 1);
            let f = c.fs(0).create(ROOT_INO, &format!("f{i}"), 0o644).unwrap();
            c.fs(0).write(f.ino, 0, &data).unwrap();
            c.fs(0).flush(f.ino).unwrap();
            (f.ino, data)
        })
        .collect();
    c.sync();
    assert_eq!(
        c.control(0).status().unwrap().staging.chunks,
        0,
        "all acked"
    );
    let held = c.fs(1).stats().unwrap();
    assert!(held.chunks >= 60, "the receiver holds the replicas");

    c.power_cut(1);
    c.restart(1);

    assert_eq!(
        c.fs(1).stats().unwrap(),
        held,
        "nothing acknowledged was lost"
    );
    // Read from the receiver alone: its own disk has every chunk.
    c.network().partition(&[&[NodeId(0)], &[NodeId(1)]]);
    for (ino, data) in &files {
        assert_eq!(&c.fs(1).read(*ino, 0, u32::MAX).unwrap(), data);
    }
}
