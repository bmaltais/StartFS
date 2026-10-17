//! Hardlinks across nodes, driven through FsOps on a cluster with a simulated
//! network and a manual clock: names, link counts and content converge.

use startfs_core::net::NodeId;
use startfs_core::ops::*;
use startfs_core::sync::HELLO_INTERVAL;
use startfs_harness::Cluster;
use std::time::Duration;

/// Two nodes of one filesystem, peered and idle.
fn pair() -> Cluster {
    let c = Cluster::start(2);
    let token = c.control(0).create("home", 2).unwrap();
    c.control(1).join(&token, None).unwrap();
    c.sync();
    c
}

fn read_all(fs: &impl FsOps, ino: Ino) -> Vec<u8> {
    fs.read(ino, 0, u32::MAX).unwrap()
}

/// Every name under `dir` as (path, ino, nlink), depth first.
fn tree(fs: &impl FsOps, dir: Ino, prefix: &str, out: &mut Vec<(String, Ino, u32)>) {
    for e in fs.readdir(dir).unwrap() {
        let path = format!("{prefix}/{}", e.name);
        let a = fs.getattr(e.ino).unwrap();
        out.push((path.clone(), e.ino, a.nlink));
        if e.kind == FileKind::Dir {
            tree(fs, e.ino, &path, out);
        }
    }
}

fn listing(c: &Cluster, node: usize) -> Vec<(String, Ino, u32)> {
    let mut out = Vec::new();
    tree(&c.fs(node), ROOT_INO, "", &mut out);
    out
}

/// What both nodes show, after checking they agree.
fn converged(c: &Cluster) -> Vec<(String, Ino, u32)> {
    let (a, b) = (listing(c, 0), listing(c, 1));
    assert_eq!(a, b, "the nodes disagree");
    a
}

fn paths(l: &[(String, Ino, u32)]) -> Vec<&str> {
    l.iter().map(|(p, _, _)| p.as_str()).collect()
}

/// Both nodes hold the files in `setup`; while partitioned, `first` runs on one
/// node and, 10 seconds later, `second` on the other; then the partition heals.
fn race(
    setup: impl FnOnce(&Cluster),
    first: impl FnOnce(&Cluster),
    second: impl FnOnce(&Cluster),
) -> Cluster {
    let c = pair();
    setup(&c);
    c.sync();
    c.network().partition(&[&[NodeId(0)], &[NodeId(1)]]);
    c.advance(Duration::from_secs(10));
    first(&c);
    c.advance(Duration::from_secs(10));
    second(&c);
    c.network().heal();
    c.advance(HELLO_INTERVAL);
    c.sync();
    c
}

fn file_f(c: &Cluster) {
    let f = c.fs(0).create(ROOT_INO, "f", 0o644).unwrap();
    c.fs(0).write(f.ino, 0, b"data").unwrap();
}

#[test]
fn a_link_and_writes_through_either_name_reach_the_other_node() {
    let c = pair();
    let (a, b) = (c.fs(0), c.fs(1));
    let f = a.create(ROOT_INO, "f", 0o644).unwrap();
    a.write(f.ino, 0, b"one").unwrap();
    a.link(f.ino, ROOT_INO, "g").unwrap();
    c.sync();

    let bf = b.lookup(ROOT_INO, "f").unwrap();
    let bg = b.lookup(ROOT_INO, "g").unwrap();
    assert_eq!((bf.ino, bf.nlink), (f.ino, 2));
    assert_eq!((bg.ino, bg.nlink), (f.ino, 2));
    assert_eq!(read_all(&b, bg.ino), b"one");

    b.write(bg.ino, 0, b"two").unwrap();
    c.sync();
    assert_eq!(read_all(&a, f.ino), b"two");
    assert_eq!(converged(&c).len(), 2);
}

#[test]
fn unlinking_one_name_on_a_node_leaves_the_other_name_on_every_node() {
    let c = pair();
    let f = c.fs(0).create(ROOT_INO, "f", 0o644).unwrap();
    c.fs(0).link(f.ino, ROOT_INO, "g").unwrap();
    c.sync();
    c.fs(1).unlink(ROOT_INO, "f").unwrap();
    c.sync();

    let l = converged(&c);
    assert_eq!(l, [("/g".to_string(), f.ino, 1)]);
}

#[test]
fn a_link_made_while_the_other_name_is_unlinked_elsewhere_keeps_the_file() {
    for link_first in [true, false] {
        let link = |c: &Cluster| {
            let f = c.fs(0).lookup(ROOT_INO, "f").unwrap();
            c.fs(0).link(f.ino, ROOT_INO, "g").unwrap();
        };
        let unlink = |c: &Cluster| c.fs(1).unlink(ROOT_INO, "f").unwrap();
        let c = if link_first {
            race(file_f, link, unlink)
        } else {
            // The unlink is the older op, but still runs on the other node.
            race(file_f, |c| c.fs(1).unlink(ROOT_INO, "f").unwrap(), link)
        };
        let l = converged(&c);
        assert_eq!(paths(&l), ["/g"], "link_first={link_first}");
        assert_eq!(l[0].2, 1);
        let g = c.fs(1).lookup(ROOT_INO, "g").unwrap();
        assert_eq!(read_all(&c.fs(1), g.ino), b"data");
    }
}

#[test]
fn unlinking_the_last_two_names_on_different_nodes_removes_the_file() {
    let c = race(
        |c| {
            file_f(c);
            let f = c.fs(0).lookup(ROOT_INO, "f").unwrap();
            c.fs(0).link(f.ino, ROOT_INO, "g").unwrap();
        },
        |c| c.fs(0).unlink(ROOT_INO, "f").unwrap(),
        |c| c.fs(1).unlink(ROOT_INO, "g").unwrap(),
    );
    assert!(converged(&c).is_empty());
}

#[test]
fn a_link_and_a_rename_of_the_only_name_both_apply() {
    let c = race(
        file_f,
        |c| {
            let f = c.fs(0).lookup(ROOT_INO, "f").unwrap();
            c.fs(0).link(f.ino, ROOT_INO, "g").unwrap();
        },
        |c| c.fs(1).rename(ROOT_INO, "f", ROOT_INO, "h").unwrap(),
    );
    let l = converged(&c);
    assert_eq!(paths(&l), ["/g", "/h"]);
    assert_eq!((l[0].1, l[0].2), (l[1].1, 2));
}

#[test]
fn a_newer_unlink_beats_a_rename_of_the_same_name_and_an_older_one_loses() {
    let rename = |c: &Cluster| c.fs(0).rename(ROOT_INO, "f", ROOT_INO, "h").unwrap();
    let unlink = |c: &Cluster| c.fs(1).unlink(ROOT_INO, "f").unwrap();
    let setup = |c: &Cluster| {
        file_f(c);
        let f = c.fs(0).lookup(ROOT_INO, "f").unwrap();
        c.fs(0).link(f.ino, ROOT_INO, "keep").unwrap();
    };
    let c = race(setup, rename, unlink);
    assert_eq!(paths(&converged(&c)), ["/keep"]);
    let c = race(setup, unlink_on_b, rename);
    assert_eq!(paths(&converged(&c)), ["/h", "/keep"]);

    fn unlink_on_b(c: &Cluster) {
        c.fs(1).unlink(ROOT_INO, "f").unwrap();
    }
}

#[test]
fn a_link_into_a_directory_removed_elsewhere_recreates_it() {
    let c = race(
        |c| {
            file_f(c);
            c.fs(0).mkdir(ROOT_INO, "d", 0o755).unwrap();
        },
        |c| c.fs(0).rmdir(ROOT_INO, "d").unwrap(),
        |c| {
            let d = c.fs(1).lookup(ROOT_INO, "d").unwrap();
            let f = c.fs(1).lookup(ROOT_INO, "f").unwrap();
            c.fs(1).link(f.ino, d.ino, "l").unwrap();
        },
    );
    let l = converged(&c);
    assert_eq!(paths(&l), ["/d", "/d/l", "/f"]);
    assert_eq!((l[1].1, l[1].2), (l[2].1, 2));
}

#[test]
fn two_links_onto_one_name_keep_one_and_save_the_other_as_a_conflict_copy() {
    let c = race(
        |c| {
            file_f(c);
            let y = c.fs(0).create(ROOT_INO, "y", 0o644).unwrap();
            c.fs(0).write(y.ino, 0, b"why").unwrap();
        },
        |c| {
            let f = c.fs(0).lookup(ROOT_INO, "f").unwrap();
            c.fs(0).link(f.ino, ROOT_INO, "n").unwrap();
        },
        |c| {
            let y = c.fs(1).lookup(ROOT_INO, "y").unwrap();
            c.fs(1).link(y.ino, ROOT_INO, "n").unwrap();
        },
    );
    let l = converged(&c);
    assert_eq!(l.len(), 4, "{l:?}");
    let n = l.iter().filter(|(p, _, _)| p.starts_with("/n")).count();
    assert_eq!(n, 2);
    // Each file keeps its own name and gains one: two links apiece.
    assert!(l.iter().all(|(_, _, nlink)| *nlink == 2), "{l:?}");
}

#[test]
fn a_conflict_copy_cannot_be_linked() {
    let c = pair();
    let f = c.fs(0).create(ROOT_INO, "f", 0o644).unwrap();
    c.fs(0).write(f.ino, 0, b"base").unwrap();
    c.sync();
    c.network().partition(&[&[NodeId(0)], &[NodeId(1)]]);
    c.fs(0).write(f.ino, 0, b"from a").unwrap();
    c.advance(Duration::from_secs(10));
    c.fs(1).write(f.ino, 0, b"from b").unwrap();
    c.network().heal();
    c.advance(HELLO_INTERVAL);
    c.sync();
    let copy = c
        .fs(0)
        .readdir(ROOT_INO)
        .unwrap()
        .into_iter()
        .find(|e| e.name.contains("conflict"))
        .expect("a conflict copy");
    assert_eq!(
        c.fs(0).link(copy.ino, ROOT_INO, "alias"),
        Err(FsError::NotPermitted)
    );
}

/// Node 0 moves one name of `f` onto `n` while node 1 links `f` as `n`: after
/// the merge `n` shows once, standing for two names of the same file.
fn two_names_under_one_string() -> Cluster {
    race(
        |c| {
            file_f(c);
            let f = c.fs(0).lookup(ROOT_INO, "f").unwrap();
            c.fs(0).link(f.ino, ROOT_INO, "g").unwrap();
        },
        |c| c.fs(0).rename(ROOT_INO, "g", ROOT_INO, "n").unwrap(),
        |c| {
            let f = c.fs(1).lookup(ROOT_INO, "f").unwrap();
            c.fs(1).link(f.ino, ROOT_INO, "n").unwrap();
        },
    )
}

#[test]
fn two_names_of_one_file_under_one_string_show_once() {
    let c = two_names_under_one_string();
    let l = converged(&c);
    assert_eq!(paths(&l), ["/f", "/n"]);
    assert_eq!(l[0].2, 2);
}

#[test]
fn unlinking_a_doubled_name_removes_it_on_every_node() {
    let c = two_names_under_one_string();
    c.fs(0).unlink(ROOT_INO, "n").unwrap();
    c.sync();
    let l = converged(&c);
    assert_eq!(paths(&l), ["/f"]);
    assert_eq!(l[0].2, 1);
}

#[test]
fn renaming_a_doubled_name_moves_it_on_every_node() {
    let c = two_names_under_one_string();
    c.fs(1).rename(ROOT_INO, "n", ROOT_INO, "m").unwrap();
    c.sync();
    let l = converged(&c);
    assert_eq!(paths(&l), ["/f", "/m"]);
    assert_eq!(l[0].2, 2);
}
