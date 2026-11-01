//! A chunk no file refers to any more is not replicated: a build that writes and deletes
//! many files must not push the deleted ones to every peer (#156).

use startfs_core::net::NodeId;
use startfs_core::ops::*;
use startfs_core::protocol::Message;
use startfs_harness::{Cluster, noise};
use std::time::Duration;

const FILE: usize = 1 << 20;

fn chunk_pushes(c: &Cluster) -> usize {
    c.network()
        .traffic()
        .iter()
        .filter(|(from, _, raw)| {
            *from == NodeId(0) && matches!(Message::decode(raw), Ok(Message::ChunkPush(_, Some(_))))
        })
        .count()
}

#[test]
fn a_file_deleted_before_replication_is_never_pushed() {
    let c = Cluster::with_offers(2, &[1 << 30, 1 << 30]);
    let fs = c.fs(0);
    let gone = fs.create(ROOT_INO, "gone", 0o644).unwrap();
    fs.write(gone.ino, 0, &noise(FILE, 1)).unwrap();
    fs.flush(gone.ino).unwrap();
    fs.unlink(ROOT_INO, "gone").unwrap();

    c.sync();
    c.elapse(Duration::from_secs(30));

    assert_eq!(chunk_pushes(&c), 0);
    assert_eq!(c.control(0).status().unwrap().staging.chunks, 0);
}

#[test]
fn a_file_that_is_kept_is_still_replicated() {
    let c = Cluster::with_offers(2, &[1 << 30, 1 << 30]);
    let fs = c.fs(0);
    let gone = fs.create(ROOT_INO, "gone", 0o644).unwrap();
    fs.write(gone.ino, 0, &noise(FILE, 1)).unwrap();
    fs.flush(gone.ino).unwrap();
    let kept = fs.create(ROOT_INO, "kept", 0o644).unwrap();
    fs.write(kept.ino, 0, &noise(FILE, 2)).unwrap();
    fs.flush(kept.ino).unwrap();
    fs.unlink(ROOT_INO, "gone").unwrap();

    c.sync();
    c.elapse(Duration::from_secs(30));

    assert!(chunk_pushes(&c) > 0);
    assert_eq!(c.control(0).status().unwrap().staging.chunks, 0);
    assert_eq!(
        c.fs(1).read(kept.ino, 0, 16),
        Ok(noise(FILE, 2)[..16].to_vec())
    );
}

/// Two nodes keeping two copies, holding new chunks for `hold` seconds.
fn holding(hold: u64, extra: &str) -> Cluster {
    Cluster::with_filesystem(2, 2, &format!("replication_hold_secs = {hold}\n{extra}"))
}

/// Let `secs` pass a second at a time.
fn run_for(c: &Cluster, secs: u64) {
    for _ in 0..secs {
        c.advance(Duration::from_secs(1));
        c.sync();
    }
}

#[test]
fn a_file_deleted_within_the_hold_is_never_pushed() {
    let c = holding(30, "");
    let fs = c.fs(0);
    let tmp = fs.create(ROOT_INO, "tmp", 0o644).unwrap();
    fs.write(tmp.ino, 0, &noise(FILE, 1)).unwrap();
    fs.flush(tmp.ino).unwrap();
    run_for(&c, 10);
    fs.unlink(ROOT_INO, "tmp").unwrap();
    run_for(&c, 60);

    assert_eq!(chunk_pushes(&c), 0);
    assert_eq!(c.control(0).status().unwrap().staging.chunks, 0);
}

#[test]
fn a_file_kept_past_the_hold_is_replicated_once_it_ends() {
    let c = holding(30, "");
    let fs = c.fs(0);
    let kept = fs.create(ROOT_INO, "kept", 0o644).unwrap();
    fs.write(kept.ino, 0, &noise(FILE, 2)).unwrap();
    fs.flush(kept.ino).unwrap();
    run_for(&c, 20);
    assert_eq!(chunk_pushes(&c), 0, "held");

    run_for(&c, 20);
    assert!(chunk_pushes(&c) > 0);
    assert!(c.control(0).status().unwrap().unreplicated.is_empty());
}

#[test]
fn nothing_is_held_while_staging_is_more_than_half_full() {
    // Room for four files in staging: two fill half of it.
    let c = holding(30, &format!("staging_bytes = {}", 4 * FILE));
    let fs = c.fs(0);
    for i in 0..3 {
        let f = fs.create(ROOT_INO, &format!("f{i}"), 0o644).unwrap();
        fs.write(f.ino, 0, &noise(FILE, 10 + i)).unwrap();
        fs.flush(f.ino).unwrap();
    }
    run_for(&c, 5);
    assert!(chunk_pushes(&c) > 0);
}
