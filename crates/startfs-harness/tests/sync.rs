//! Op-log metadata sync, driven through FsOps and the control interface on a
//! cluster with a simulated network and a manual clock.

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

const CONFLICT_FROM_A: &str = "report (conflict from node0 20231114-221330).txt";

/// Both nodes hold report.txt; then A (at 22:13:30) and B (at 22:13:40, newer)
/// write it while partitioned, and the partition heals.
fn conflicting_writes() -> Cluster {
    let c = pair();
    let f = c.fs(0).create(ROOT_INO, "report.txt", 0o644).unwrap();
    c.fs(0).write(f.ino, 0, b"v0").unwrap();
    c.sync();
    c.network().partition(&[&[NodeId(0)], &[NodeId(1)]]);
    c.advance(Duration::from_secs(10));
    c.fs(0).write(f.ino, 0, b"from a").unwrap();
    c.advance(Duration::from_secs(10));
    let on_b = c.fs(1).lookup(ROOT_INO, "report.txt").unwrap();
    c.fs(1).write(on_b.ino, 0, b"from b").unwrap();
    c.network().heal();
    c.advance(HELLO_INTERVAL);
    c.sync();
    c
}

#[test]
fn conflicting_writes_keep_the_newest_and_save_the_loser_as_a_named_copy() {
    let c = conflicting_writes();
    for i in 0..2 {
        let fs = c.fs(i);
        assert_eq!(
            names(&fs, ROOT_INO),
            [CONFLICT_FROM_A, "report.txt"],
            "node {i}"
        );
        let winner = fs.lookup(ROOT_INO, "report.txt").unwrap();
        assert_eq!(read_all(&fs, winner.ino), b"from b");
        let copy = fs.lookup(ROOT_INO, CONFLICT_FROM_A).unwrap();
        assert_eq!(read_all(&fs, copy.ino), b"from a");
    }
}

#[test]
fn status_lists_conflict_copies_on_every_node() {
    let c = conflicting_writes();
    for i in 0..2 {
        let s = c.control(i).status().unwrap();
        let paths: Vec<_> = s.conflicts.iter().map(|x| x.path.as_str()).collect();
        assert_eq!(paths, [format!("/{CONFLICT_FROM_A}")], "node {i}");
    }
}

#[test]
fn deleting_a_conflict_copy_resolves_it_everywhere() {
    let c = conflicting_writes();
    c.fs(0).unlink(ROOT_INO, CONFLICT_FROM_A).unwrap();
    assert!(c.control(0).status().unwrap().conflicts.is_empty());
    c.sync();
    for i in 0..2 {
        assert_eq!(names(&c.fs(i), ROOT_INO), ["report.txt"], "node {i}");
        assert!(
            c.control(i).status().unwrap().conflicts.is_empty(),
            "node {i}"
        );
    }
    // Later activity does not resurrect it.
    let f = c.fs(1).lookup(ROOT_INO, "report.txt").unwrap();
    c.fs(1).write(f.ino, 0, b"later").unwrap();
    c.sync();
    assert_eq!(names(&c.fs(0), ROOT_INO), ["report.txt"]);
}

#[test]
fn renames_and_deletes_propagate() {
    let c = pair();
    let (a, b) = (c.fs(0), c.fs(1));
    let d = a.mkdir(ROOT_INO, "dir", 0o755).unwrap();
    let f = a.create(ROOT_INO, "old", 0o644).unwrap();
    a.write(f.ino, 0, b"keep me").unwrap();
    a.create(ROOT_INO, "doomed", 0o644).unwrap();
    c.sync();

    c.advance(Duration::from_secs(1));
    a.rename(ROOT_INO, "old", d.ino, "new").unwrap();
    a.unlink(ROOT_INO, "doomed").unwrap();
    c.sync();

    assert_eq!(names(&b, ROOT_INO), ["dir"]);
    let moved = b.lookup(d.ino, "new").unwrap();
    assert_eq!(read_all(&b, moved.ino), b"keep me");
}

/// Both nodes hold file `x`; while partitioned, one renames it and the other
/// deletes it, in the given order 10 seconds apart; then the partition heals.
fn rename_delete_race(rename_first: bool) -> Cluster {
    let c = pair();
    let f = c.fs(0).create(ROOT_INO, "x", 0o644).unwrap();
    c.fs(0).write(f.ino, 0, b"data").unwrap();
    c.sync();
    c.network().partition(&[&[NodeId(0)], &[NodeId(1)]]);
    let rename = || c.fs(0).rename(ROOT_INO, "x", ROOT_INO, "y").unwrap();
    let delete = || c.fs(1).unlink(ROOT_INO, "x").unwrap();
    c.advance(Duration::from_secs(10));
    if rename_first {
        rename();
        c.advance(Duration::from_secs(10));
        delete();
    } else {
        delete();
        c.advance(Duration::from_secs(10));
        rename();
    }
    c.network().heal();
    c.advance(HELLO_INTERVAL);
    c.sync();
    c
}

#[test]
fn a_delete_newer_than_a_rename_wins() {
    let c = rename_delete_race(true);
    for i in 0..2 {
        assert!(names(&c.fs(i), ROOT_INO).is_empty(), "node {i}");
    }
}

#[test]
fn a_rename_newer_than_a_delete_wins() {
    let c = rename_delete_race(false);
    for i in 0..2 {
        assert_eq!(names(&c.fs(i), ROOT_INO), ["y"], "node {i}");
        let y = c.fs(i).lookup(ROOT_INO, "y").unwrap();
        assert_eq!(read_all(&c.fs(i), y.ino), b"data");
    }
}

/// Both nodes hold empty directory `d`; while partitioned, A removes it and B
/// adds `d/late`, in the given order 10 seconds apart; then the partition heals.
fn write_into_deleted_dir(delete_first: bool) -> Cluster {
    let c = pair();
    c.fs(0).mkdir(ROOT_INO, "d", 0o755).unwrap();
    c.sync();
    c.network().partition(&[&[NodeId(0)], &[NodeId(1)]]);
    let delete = || c.fs(0).rmdir(ROOT_INO, "d").unwrap();
    let write = || {
        let d = c.fs(1).lookup(ROOT_INO, "d").unwrap();
        let f = c.fs(1).create(d.ino, "late", 0o644).unwrap();
        c.fs(1).write(f.ino, 0, b"offline work").unwrap();
    };
    c.advance(Duration::from_secs(10));
    if delete_first {
        delete();
        c.advance(Duration::from_secs(10));
        write();
    } else {
        write();
        c.advance(Duration::from_secs(10));
        delete();
    }
    c.network().heal();
    c.advance(HELLO_INTERVAL);
    c.sync();
    c
}

#[test]
fn a_write_into_a_deleted_directory_recreates_the_path() {
    for delete_first in [true, false] {
        let c = write_into_deleted_dir(delete_first);
        for i in 0..2 {
            let fs = c.fs(i);
            let d = fs.lookup(ROOT_INO, "d").unwrap();
            let late = fs.lookup(d.ino, "late").unwrap();
            assert_eq!(read_all(&fs, late.ino), b"offline work", "node {i}");
        }
    }
}

#[test]
fn recreation_restores_every_deleted_ancestor() {
    let c = pair();
    let a = c.fs(0);
    let x = a.mkdir(ROOT_INO, "x", 0o755).unwrap();
    a.mkdir(x.ino, "y", 0o755).unwrap();
    c.sync();
    c.network().partition(&[&[NodeId(0)], &[NodeId(1)]]);
    c.advance(Duration::from_secs(10));
    let y = a.lookup(x.ino, "y").unwrap();
    a.rmdir(x.ino, "y").unwrap();
    a.rmdir(ROOT_INO, "x").unwrap();
    c.advance(Duration::from_secs(10));
    let b = c.fs(1);
    let bx = b.lookup(ROOT_INO, "x").unwrap();
    let by = b.lookup(bx.ino, "y").unwrap();
    assert_eq!(by.ino, y.ino);
    b.create(by.ino, "deep", 0o644).unwrap();
    c.network().heal();
    c.advance(HELLO_INTERVAL);
    c.sync();
    for i in 0..2 {
        let fs = c.fs(i);
        let x = fs.lookup(ROOT_INO, "x").unwrap();
        let y = fs.lookup(x.ino, "y").unwrap();
        assert_eq!(names(&fs, y.ino), ["deep"], "node {i}");
    }
}

/// Both nodes create the same names while partitioned: A first (22:13:30), B 10s later.
fn same_name_created_on_both() -> Cluster {
    let c = pair();
    c.network().partition(&[&[NodeId(0)], &[NodeId(1)]]);
    c.advance(Duration::from_secs(10));
    let (a, b) = (c.fs(0), c.fs(1));
    let f = a.create(ROOT_INO, "notes.txt", 0o644).unwrap();
    a.write(f.ino, 0, b"a's notes").unwrap();
    let d = a.mkdir(ROOT_INO, "docs", 0o755).unwrap();
    a.create(d.ino, "from-a", 0o644).unwrap();
    c.advance(Duration::from_secs(10));
    let f = b.create(ROOT_INO, "notes.txt", 0o644).unwrap();
    b.write(f.ino, 0, b"b's notes").unwrap();
    let d = b.mkdir(ROOT_INO, "docs", 0o755).unwrap();
    b.create(d.ino, "from-b", 0o644).unwrap();
    c.network().heal();
    c.advance(HELLO_INTERVAL);
    c.sync();
    c
}

#[test]
fn files_created_with_the_same_name_keep_the_newest_and_a_conflict_copy() {
    let c = same_name_created_on_both();
    for i in 0..2 {
        let fs = c.fs(i);
        let copy = "notes (conflict from node0 20231114-221330).txt";
        assert_eq!(
            names(&fs, ROOT_INO),
            ["docs", copy, "notes.txt"],
            "node {i}"
        );
        let win = fs.lookup(ROOT_INO, "notes.txt").unwrap();
        assert_eq!(read_all(&fs, win.ino), b"b's notes");
        let lose = fs.lookup(ROOT_INO, copy).unwrap();
        assert_eq!(read_all(&fs, lose.ino), b"a's notes");
        let paths: Vec<_> = c.control(i).status().unwrap().conflicts;
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].path, format!("/{copy}"));
    }
}

#[test]
fn directories_created_with_the_same_name_are_merged() {
    let c = same_name_created_on_both();
    for i in 0..2 {
        let fs = c.fs(i);
        let d = fs.lookup(ROOT_INO, "docs").unwrap();
        assert_eq!(names(&fs, d.ino), ["from-a", "from-b"], "node {i}");
    }
}

#[test]
fn offline_edits_survive_a_crash_and_merge_after_restart() {
    let c = pair();
    c.network().partition(&[&[NodeId(0)], &[NodeId(1)]]);
    let f = c.fs(0).create(ROOT_INO, "offline", 0o644).unwrap();
    c.fs(0)
        .write(f.ino, 0, b"written before the crash")
        .unwrap();
    c.fs(1).create(ROOT_INO, "other", 0o644).unwrap();
    c.crash(0);
    c.restart(0);
    c.network().heal();
    c.advance(HELLO_INTERVAL);
    c.sync();

    for i in 0..2 {
        assert_eq!(names(&c.fs(i), ROOT_INO), ["offline", "other"], "node {i}");
    }
    let on_b = c.fs(1).lookup(ROOT_INO, "offline").unwrap();
    assert_eq!(read_all(&c.fs(1), on_b.ino), b"written before the crash");
}

#[test]
fn a_node_that_was_down_catches_up_when_it_restarts() {
    let c = pair();
    c.crash(1);
    let f = c.fs(0).create(ROOT_INO, "while-down", 0o644).unwrap();
    c.fs(0).write(f.ino, 0, b"missed").unwrap();
    c.sync();
    c.restart(1);
    c.advance(HELLO_INTERVAL);
    c.sync();
    let on_b = c.fs(1).lookup(ROOT_INO, "while-down").unwrap();
    assert_eq!(read_all(&c.fs(1), on_b.ino), b"missed");
}

#[test]
fn three_nodes_converge_without_quorum_even_with_one_cut_off() {
    let (a, b, z) = (NodeId(0), NodeId(1), NodeId(2));
    let c = Cluster::start(3);
    let token = c.control(0).create("home", 3).unwrap();
    c.control(1).join(&token, None).unwrap();
    c.control(2).join(&token, None).unwrap();
    c.sync();
    // Node 2 is isolated: the other two still accept writes and agree.
    c.network().partition(&[&[a, b], &[z]]);
    c.fs(0).create(ROOT_INO, "one", 0o644).unwrap();
    c.fs(1).create(ROOT_INO, "two", 0o644).unwrap();
    c.fs(2).create(ROOT_INO, "three", 0o644).unwrap();
    c.sync();
    assert_eq!(names(&c.fs(0), ROOT_INO), ["one", "two"]);
    assert_eq!(names(&c.fs(1), ROOT_INO), ["one", "two"]);
    assert_eq!(names(&c.fs(2), ROOT_INO), ["three"]);

    c.network().heal();
    c.advance(HELLO_INTERVAL);
    c.sync();
    for i in 0..3 {
        assert_eq!(
            names(&c.fs(i), ROOT_INO),
            ["one", "three", "two"],
            "node {i}"
        );
    }
}

#[test]
fn entries_are_relayed_between_nodes_that_cannot_reach_each_other() {
    let c = Cluster::start(3);
    let token = c.control(0).create("home", 3).unwrap();
    c.control(1).join(&token, None).unwrap();
    c.control(2).join(&token, None).unwrap();
    c.sync();
    c.network().set_drop(NodeId(0), NodeId(2), true);
    c.fs(0).create(ROOT_INO, "relayed", 0o644).unwrap();
    c.sync();
    assert_eq!(names(&c.fs(2), ROOT_INO), ["relayed"]);
}

#[test]
fn metadata_changes_propagate() {
    let c = pair();
    let (a, b) = (c.fs(0), c.fs(1));
    let f = a.create(ROOT_INO, "f", 0o644).unwrap();
    a.write(f.ino, 0, b"0123456789").unwrap();
    c.sync();
    c.advance(Duration::from_secs(1));
    a.setattr(
        f.ino,
        SetAttr {
            mode: Some(0o600),
            size: Some(4),
            ..Default::default()
        },
    )
    .unwrap();
    a.setxattr(f.ino, "user.tag", b"blue").unwrap();
    c.sync();
    let on_b = b.lookup(ROOT_INO, "f").unwrap();
    assert_eq!(on_b.mode, 0o600);
    assert_eq!(read_all(&b, on_b.ino), b"0123");
    assert_eq!(b.getxattr(on_b.ino, "user.tag").unwrap(), b"blue");
    c.advance(Duration::from_secs(1));
    a.removexattr(f.ino, "user.tag").unwrap();
    c.sync();
    assert_eq!(b.getxattr(on_b.ino, "user.tag"), Err(FsError::NoData));
}

#[test]
fn a_node_makes_commits_durable_before_a_peer_can_see_them() {
    let c = Cluster::start_with_config(2, "sync_quiet_ms = 1000000");
    let token = c.control(0).create("home", 2).unwrap();
    c.control(1).join(&token, None).unwrap();
    c.sync();
    let (a, b) = (c.fs(0), c.fs(1));
    a.create(ROOT_INO, "n", 0o644).unwrap();
    assert!(a.has_unsynced());

    c.sync();
    assert!(!a.has_unsynced(), "durable before the entries went out");
    assert!(b.lookup(ROOT_INO, "n").is_ok());
}

/// Nodes hold empty root-level directories `names`; while partitioned, node i
/// renames directory i into directory i+1 (a cycle), one second apart in node
/// order; then the partition heals.
fn rename_cycle(names: &[&str]) -> Cluster {
    rename_cycle_with(names, false)
}

/// As [`rename_cycle`], optionally with a user directory `lost+found` (holding
/// `mine.txt`) already at the root.
fn rename_cycle_with(names: &[&str], user_lost_found: bool) -> Cluster {
    let n = names.len();
    let c = Cluster::start(n);
    let token = c.control(0).create("home", n as u32).unwrap();
    for i in 1..n {
        c.control(i).join(&token, None).unwrap();
    }
    c.sync();
    if user_lost_found {
        let lf = c.fs(0).mkdir(ROOT_INO, "lost+found", 0o755).unwrap();
        c.fs(0).create(lf.ino, "mine.txt", 0o644).unwrap();
    }
    let dirs: Vec<Ino> = names
        .iter()
        .map(|d| c.fs(0).mkdir(ROOT_INO, d, 0o755).unwrap().ino)
        .collect();
    let f = c.fs(0).create(dirs[0], "inner.txt", 0o644).unwrap();
    c.fs(0).write(f.ino, 0, b"still here").unwrap();
    c.sync();
    let sides: Vec<[NodeId; 1]> = (0..n).map(|i| [NodeId(i as u32)]).collect();
    let groups: Vec<&[NodeId]> = sides.iter().map(|s| &s[..]).collect();
    c.network().partition(&groups);
    for (i, name) in names.iter().enumerate() {
        c.advance(Duration::from_secs(1));
        let next = (i + 1) % n;
        c.fs(i).rename(ROOT_INO, name, dirs[next], name).unwrap();
    }
    c.network().heal();
    c.advance(HELLO_INTERVAL);
    c.sync();
    c
}

#[test]
fn a_two_directory_rename_cycle_puts_the_later_rename_in_lost_and_found() {
    let c = rename_cycle(&["a", "b"]);
    for i in 0..2 {
        let fs = c.fs(i);
        // a moved into b first; b moved into a later and gives way.
        assert_eq!(names(&fs, ROOT_INO), ["lost+found"], "node {i}");
        let lf = fs.lookup(ROOT_INO, "lost+found").unwrap();
        assert_eq!(names(&fs, lf.ino), ["b"], "node {i}");
        let b = fs.lookup(lf.ino, "b").unwrap();
        assert_eq!(names(&fs, b.ino), ["a"], "node {i}");
        let a = fs.lookup(b.ino, "a").unwrap();
        let f = fs.lookup(a.ino, "inner.txt").unwrap();
        assert_eq!(read_all(&fs, f.ino), b"still here", "node {i}");
    }
}

#[test]
fn a_three_directory_rename_cycle_breaks_at_exactly_one_directory() {
    let c = rename_cycle(&["a", "b", "c"]);
    for i in 0..3 {
        let fs = c.fs(i);
        assert_eq!(names(&fs, ROOT_INO), ["lost+found"], "node {i}");
        let lf = fs.lookup(ROOT_INO, "lost+found").unwrap();
        assert_eq!(names(&fs, lf.ino), ["c"], "node {i}");
        let mut at = fs.lookup(lf.ino, "c").unwrap();
        for next in ["b", "a"] {
            assert_eq!(names(&fs, at.ino), [next], "node {i}");
            at = fs.lookup(at.ino, next).unwrap();
        }
        let f = fs.lookup(at.ino, "inner.txt").unwrap();
        assert_eq!(read_all(&fs, f.ino), b"still here", "node {i}");
    }
}

#[test]
fn a_users_own_lost_and_found_directory_absorbs_the_displaced_one() {
    let c = rename_cycle_with(&["a", "b"], true);
    for i in 0..2 {
        let fs = c.fs(i);
        assert_eq!(names(&fs, ROOT_INO), ["lost+found"], "node {i}");
        let lf = fs.lookup(ROOT_INO, "lost+found").unwrap();
        assert_eq!(names(&fs, lf.ino), ["b", "mine.txt"], "node {i}");
    }
}

#[test]
fn moving_a_directory_out_of_lost_and_found_sticks_and_the_folder_goes_away() {
    let c = rename_cycle(&["a", "b"]);
    let fs = c.fs(0);
    let lf = fs.lookup(ROOT_INO, "lost+found").unwrap();
    c.advance(Duration::from_secs(1));
    fs.rename(lf.ino, "b", ROOT_INO, "b").unwrap();
    c.sync();
    for i in 0..2 {
        assert_eq!(names(&c.fs(i), ROOT_INO), ["b"], "node {i}");
    }
}

#[test]
fn no_lost_and_found_without_a_cycle() {
    let c = pair();
    let a = c.fs(0).mkdir(ROOT_INO, "a", 0o755).unwrap();
    c.fs(0).mkdir(ROOT_INO, "b", 0o755).unwrap();
    c.sync();
    c.fs(0).rename(ROOT_INO, "b", a.ino, "b").unwrap();
    c.sync();
    assert_eq!(names(&c.fs(1), ROOT_INO), ["a"]);
}
