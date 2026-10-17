//! Op-log metadata sync, driven through FsOps and the control interface on a
//! cluster with a simulated network and a manual clock.

use startfs_core::net::NodeId;
use startfs_core::ops::*;
use startfs_core::sync::HELLO_INTERVAL;
use startfs_harness::Cluster;

/// Two nodes of one filesystem, peered and idle.
fn pair() -> Cluster {
    let c = Cluster::start(2);
    c.control(0).create("home", 2).unwrap();
    c.peer_all();
    c
}

fn read_all(fs: &impl FsOps, ino: Ino) -> Vec<u8> {
    fs.read(ino, 0, u32::MAX).unwrap()
}

#[test]
fn a_file_created_on_one_node_appears_with_its_content_on_the_other() {
    let c = pair();
    let (a, b) = (c.fs(0), c.fs(1));
    let d = a.mkdir(ROOT_INO, "docs", 0o755).unwrap();
    let f = a.create(d.ino, "note.txt", 0o644).unwrap();
    a.write(f.ino, 0, b"hello").unwrap();
    c.sync();

    let bd = b.lookup(ROOT_INO, "docs").unwrap();
    assert_eq!(bd.kind, FileKind::Dir);
    let bf = b.lookup(bd.ino, "note.txt").unwrap();
    assert_eq!(read_all(&b, bf.ino), b"hello");
    assert_eq!(bf.mode, 0o644);
}

fn names(fs: &impl FsOps, dir: Ino) -> Vec<String> {
    fs.readdir(dir)
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect()
}

#[test]
fn concurrent_edits_to_different_files_converge_to_the_same_tree() {
    let c = pair();
    let (a, b) = (c.fs(0), c.fs(1));
    let af = a.create(ROOT_INO, "from-a", 0o644).unwrap();
    a.write(af.ino, 0, b"A").unwrap();
    let bf = b.create(ROOT_INO, "from-b", 0o644).unwrap();
    b.write(bf.ino, 0, b"B").unwrap();
    c.sync();

    assert_eq!(names(&a, ROOT_INO), ["from-a", "from-b"]);
    assert_eq!(names(&b, ROOT_INO), ["from-a", "from-b"]);
    let on_a = a.lookup(ROOT_INO, "from-b").unwrap();
    assert_eq!(read_all(&a, on_a.ino), b"B");
    let on_b = b.lookup(ROOT_INO, "from-a").unwrap();
    assert_eq!(read_all(&b, on_b.ino), b"A");
}

#[test]
fn writes_made_while_partitioned_merge_on_reconnect() {
    let c = pair();
    let (a, b) = (c.fs(0), c.fs(1));
    c.network().partition(&[&[NodeId(0)], &[NodeId(1)]]);
    let af = a.create(ROOT_INO, "offline-a", 0o644).unwrap();
    a.write(af.ino, 0, b"made on a").unwrap();
    let d = b.mkdir(ROOT_INO, "dir", 0o755).unwrap();
    b.create(d.ino, "offline-b", 0o644).unwrap();
    c.sync();
    assert_eq!(names(&a, ROOT_INO), ["offline-a"]);
    assert_eq!(names(&b, ROOT_INO), ["dir"]);

    c.network().heal();
    c.advance(HELLO_INTERVAL);
    c.sync();
    assert_eq!(names(&a, ROOT_INO), ["dir", "offline-a"]);
    assert_eq!(names(&b, ROOT_INO), ["dir", "offline-a"]);
    let ad = a.lookup(ROOT_INO, "dir").unwrap();
    assert_eq!(names(&a, ad.ino), ["offline-b"]);
}
