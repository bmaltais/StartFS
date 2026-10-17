//! The daemon control interface, the same one the CLI uses over the local socket.

use startfs_core::control::NodeState;
use startfs_core::ops::{FsOps, ROOT_INO};
use startfs_harness::Cluster;

#[test]
fn create_prints_a_token_with_filesystem_id_and_bootstrap_address() {
    let c = Cluster::start(1);
    let token = c.control(0).create("home", 3).unwrap();
    assert!(token.starts_with("startfs1-"), "{token}");
    assert!(token.ends_with("@100.64.0.1:7600"), "{token}");
    // Filesystem id is 16 bytes in hex, followed by the token's own id.
    let ids = token
        .strip_prefix("startfs1-")
        .unwrap()
        .split('@')
        .next()
        .unwrap();
    let (id, token_id) = ids.split_once('.').unwrap();
    assert_eq!(id.len(), 32);
    assert!(id.chars().all(|ch| ch.is_ascii_hexdigit()));
    assert!(!token_id.is_empty());
}

#[test]
fn status_of_a_one_node_filesystem_shows_name_copies_state_and_usage() {
    let c = Cluster::start(1);
    c.control(0).create("home", 3).unwrap();
    let s = c.control(0).status().unwrap();
    let fs = s.filesystem.expect("filesystem exists");
    assert_eq!(fs.name, "home");
    assert_eq!(fs.copies, 3);
    assert_eq!(s.node_state, NodeState::Online);
    assert_eq!(s.usage.stored_bytes, 0);
    assert_eq!(s.usage.chunks, 0);
    // One node cannot hold three copies: redundancy is explicit best effort.
    assert_eq!(s.nodes, 1);
    assert_eq!(s.copies_possible, 1);
}

#[test]
fn status_before_create_reports_no_filesystem() {
    let c = Cluster::start(1);
    let s = c.control(0).status().unwrap();
    assert!(s.filesystem.is_none());
}

#[test]
fn status_usage_grows_with_stored_data() {
    let c = Cluster::start(1);
    let fs = c.fs(0);
    c.control(0).create("home", 2).unwrap();
    let f = fs.create(ROOT_INO, "big", 0o644).unwrap();
    // Poorly compressible data, large enough to need chunks.
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    let data: Vec<u8> = (0..200_000)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 32) as u8
        })
        .collect();
    fs.write(f.ino, 0, &data).unwrap();
    let s = c.control(0).status().unwrap();
    assert!(s.usage.chunks >= 1);
    assert!(s.usage.stored_bytes > 100_000, "{:?}", s.usage);
}

#[test]
fn defaults_need_no_config_and_a_config_file_overrides_them() {
    let c = Cluster::start(1);
    c.control(0).create("home", 3).unwrap();
    assert_eq!(c.control(0).status().unwrap().usage.offer_bytes, 10 << 30);

    let c = Cluster::start_with_config(1, "offer_bytes = 5000\nport = 9000\n");
    let token = c.control(0).create("home", 3).unwrap();
    assert!(token.ends_with(":9000"), "{token}");
    assert_eq!(c.control(0).status().unwrap().usage.offer_bytes, 5000);
}

#[test]
fn n_is_fixed_at_creation_and_a_second_create_is_refused() {
    let c = Cluster::start(1);
    c.control(0).create("home", 3).unwrap();
    assert!(c.control(0).create("other", 5).is_err());
    let fs = c.control(0).status().unwrap().filesystem.unwrap();
    assert_eq!((fs.name.as_str(), fs.copies), ("home", 3));
}

#[test]
fn create_needs_a_name_and_at_least_one_copy() {
    let c = Cluster::start(1);
    assert!(c.control(0).create("home", 0).is_err());
    assert!(c.control(0).create("", 3).is_err());
    assert!(c.control(0).status().unwrap().filesystem.is_none());
}

#[test]
fn the_filesystem_survives_a_daemon_crash_and_restart() {
    let c = Cluster::start(1);
    c.control(0).create("home", 3).unwrap();
    c.crash(0);
    assert!(c.control(0).status().is_err(), "daemon is down");
    c.restart(0);
    let fs = c.control(0).status().unwrap().filesystem.unwrap();
    assert_eq!((fs.name.as_str(), fs.copies), ("home", 3));
}
