//! Storage-only members: they hold a replica and take part in repair, but never serve the
//! filesystem. Driven through the cluster harness.

use startfs_core::control::{NodeState, Verdict};
use startfs_core::ops::*;
use startfs_core::status_view::render;
use startfs_core::sync::HELLO_INTERVAL;
use startfs_harness::{Cluster, noise};

const BIG: u64 = 10 << 30;

fn put(fs: &impl FsOps, name: &str, data: &[u8]) {
    let f = fs.create(ROOT_INO, name, 0o644).unwrap();
    assert_eq!(fs.write(f.ino, 0, data).unwrap(), data.len());
}

/// Node 0 mounts; nodes 1 and 2 are storage-only. Two copies of everything.
fn one_mounter_two_storage_members() -> Cluster {
    let c = Cluster::start(3);
    let token = c.control(0).create("home", 2).unwrap();
    c.control(0).set_offer(BIG).unwrap();
    for i in 1..3 {
        c.control(i)
            .join_storage_only(&token, Some(BIG), "")
            .unwrap();
    }
    c.sync();
    c
}

#[test]
fn a_storage_only_member_holds_its_share_and_status_names_the_roles() {
    let c = one_mounter_two_storage_members();
    for i in 0..20 {
        put(&c.fs(0), &format!("f{i}"), &noise(8 * 1024, i + 1));
    }
    c.sync();

    let status = c.control(0).status().unwrap();
    let roles: Vec<(u64, bool)> = status
        .members
        .iter()
        .map(|m| (m.id.0.into(), m.storage_only))
        .collect();
    assert_eq!(roles, [(0, false), (1, true), (2, true)]);
    assert!(status.members.iter().all(|m| m.state == NodeState::Online));
    assert_eq!(
        status.verdict,
        Some(Verdict::Safe),
        "the role is not a fault"
    );

    // Two copies of every chunk over three members: the storage-only ones hold some.
    let held: Vec<u64> = (0..3)
        .map(|i| c.control(i).status().map_or(0, |s| s.usage.chunks))
        .collect();
    assert!(held[1] > 0 && held[2] > 0, "{held:?}");

    let text = render(&status, false);
    assert!(text.contains("storage-only"), "{text}");
}

#[test]
fn every_filesystem_operation_is_refused_on_a_storage_only_member() {
    let c = one_mounter_two_storage_members();
    put(&c.fs(0), "a", b"hello");
    c.sync();

    let fs = c.fs(1);
    let refused = |r: FsResult<()>| assert_eq!(r, Err(FsError::StorageOnly));
    refused(fs.lookup(ROOT_INO, "a").map(drop));
    refused(fs.getattr(ROOT_INO).map(drop));
    refused(fs.readdir(ROOT_INO).map(drop));
    refused(fs.create(ROOT_INO, "b", 0o644).map(drop));
    refused(fs.mkdir(ROOT_INO, "d", 0o755).map(drop));
    refused(fs.write(2, 0, b"x").map(drop));
    refused(fs.read(2, 0, 10).map(drop));
    refused(fs.unlink(ROOT_INO, "a"));
    refused(fs.space().map(drop));

    // The mounting member is untouched, and nothing was written through the other.
    assert_eq!(c.fs(0).readdir(ROOT_INO).unwrap().len(), 1);
}

#[test]
fn the_role_is_set_and_cleared_with_config_and_gossiped() {
    let c = Cluster::start(3);
    let token = c.control(0).create("home", 2).unwrap();
    c.control(0).set_offer(BIG).unwrap();
    for i in 1..3 {
        c.control(i).join(&token, Some(BIG)).unwrap();
    }
    c.sync();
    put(&c.fs(2), "mine", b"before");
    c.sync();

    c.control(2).set_storage_only(true).unwrap();
    c.advance(HELLO_INTERVAL);
    c.sync();
    for viewer in 0..3 {
        let status = c.control(viewer).status().unwrap();
        let flags: Vec<bool> = status.members.iter().map(|m| m.storage_only).collect();
        assert_eq!(flags, [false, false, true], "seen from {viewer}");
    }
    assert_eq!(
        c.fs(2).readdir(ROOT_INO).map(drop),
        Err(FsError::StorageOnly)
    );

    c.control(2).set_storage_only(false).unwrap();
    c.advance(HELLO_INTERVAL);
    c.sync();
    let status = c.control(0).status().unwrap();
    assert!(status.members.iter().all(|m| !m.storage_only));
    assert_eq!(c.fs(2).readdir(ROOT_INO).unwrap().len(), 1);
}

#[test]
fn the_role_is_refused_while_the_filesystem_is_mounted_or_the_offer_is_zero() {
    let c = Cluster::start(3);
    let token = c.control(0).create("home", 2).unwrap();
    c.control(0).set_offer(BIG).unwrap();
    c.control(1).join(&token, Some(BIG)).unwrap();
    c.control(2).join(&token, Some(0)).unwrap(); // a pure client
    c.sync();

    // Mounted: unmount first.
    c.mount(1);
    let why = c.control(1).set_storage_only(true).unwrap_err().to_string();
    assert!(why.contains("unmount"), "{why}");

    // Joining as storage-only with the filesystem already mounted is refused as well.
    let fresh = Cluster::start(2);
    let t = fresh.control(0).create("home", 1).unwrap();
    fresh.mount(1);
    let why = fresh
        .control(1)
        .join_storage_only(&t, Some(BIG), "")
        .unwrap_err()
        .to_string();
    assert!(why.contains("unmount"), "{why}");

    // Offering nothing: there would be nothing for it to do.
    assert!(c.control(2).set_storage_only(true).is_err());
    let again = Cluster::start(2);
    let t = again.control(0).create("home", 1).unwrap();
    let joined = again.control(1).join_storage_only(&t, Some(0), "");
    assert!(
        joined.is_err(),
        "joining storage-only with nothing to offer"
    );

    // And a storage-only member cannot later drop its offer to nothing.
    let c = one_mounter_two_storage_members();
    assert!(c.control(1).set_offer(0).is_err());
    assert!(
        c.control(1).set_storage_only(true).is_ok(),
        "already is: no change"
    );
    let status = c.control(0).status().unwrap();
    assert!(status.members[1].storage_only);
}

fn write_files(c: &Cluster, count: u64) {
    for i in 0..count {
        put(&c.fs(0), &format!("f{i}"), &noise(8 * 1024, i + 1));
    }
    c.sync();
}

fn chunks_held(c: &Cluster) -> Vec<u64> {
    (0..c.node_count())
        .map(|i| c.control(i).status().map_or(0, |s| s.usage.chunks))
        .collect()
}

#[test]
fn repair_restores_the_copies_when_a_storage_only_member_is_lost() {
    let c = one_mounter_two_storage_members();
    write_files(&c, 20);
    let before = chunks_held(&c);
    let unique = before.iter().sum::<u64>() / 2;

    c.crash(1);
    c.elapse(std::time::Duration::from_secs(3 * 3600));

    // Two copies of every chunk, now on the two members left.
    let after = chunks_held(&c);
    assert_eq!(
        (after[0], after[2]),
        (unique, unique),
        "{before:?} -> {after:?}"
    );
    assert!(c.control(0).status().unwrap().unreplicated.is_empty());
}

#[test]
fn offers_steer_placement_and_a_storage_only_member_can_leave() {
    let c = Cluster::start(3);
    let token = c.control(0).create("home", 2).unwrap();
    c.control(0).set_offer(BIG).unwrap();
    c.control(1)
        .join_storage_only(&token, Some(BIG), "")
        .unwrap();
    c.control(2)
        .join_storage_only(&token, Some(BIG / 8), "")
        .unwrap();
    c.sync();
    write_files(&c, 40);
    let held = chunks_held(&c);
    assert!(
        held[1] > held[2],
        "the larger offer takes the larger share: {held:?}"
    );

    c.control(2).leave().unwrap();
    c.elapse(std::time::Duration::from_secs(3600));
    let after = chunks_held(&c);
    assert_eq!(
        after[0], after[1],
        "two copies on the two members left: {after:?}"
    );
    assert!(c.control(0).status().unwrap().unreplicated.is_empty());
}
