//! Joining a filesystem, membership and remote reads, driven through the
//! control interface and FsOps on a cluster with a simulated network.

use startfs_core::ops::*;
use startfs_harness::Cluster;

#[test]
fn a_second_node_joins_with_a_token_and_sees_the_whole_tree() {
    let c = Cluster::start(2);
    let a = c.fs(0);
    let token = c.control(0).create("home", 2).unwrap();
    let d = a.mkdir(ROOT_INO, "docs", 0o755).unwrap();
    let f = a.create(d.ino, "note.txt", 0o644).unwrap();
    a.write(f.ino, 0, b"hello").unwrap();

    c.control(1).join(&token, None).unwrap();
    c.sync();

    let b = c.fs(1);
    let bd = b.lookup(ROOT_INO, "docs").unwrap();
    let bf = b.lookup(bd.ino, "note.txt").unwrap();
    assert_eq!(b.read(bf.ino, 0, 100).unwrap(), b"hello");
    let fs = c.control(1).status().unwrap().filesystem.unwrap();
    assert_eq!((fs.name.as_str(), fs.copies), ("home", 2));
}
