//! Single-node file semantics, driven only through the filesystem-operations interface.

use startfs_core::ops::*;
use startfs_harness::Cluster;

fn names(fs: &impl FsOps, dir: Ino) -> Vec<String> {
    fs.readdir(dir)
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect()
}

#[test]
fn created_file_reads_back_what_was_written() {
    let c = Cluster::start(1);
    let fs = c.fs(0);
    let f = fs.create(ROOT_INO, "hello.txt", 0o644).unwrap();
    assert_eq!(fs.write(f.ino, 0, b"hello world").unwrap(), 11);
    assert_eq!(fs.read(f.ino, 0, 100).unwrap(), b"hello world");
    assert_eq!(fs.read(f.ino, 6, 100).unwrap(), b"world");
    assert_eq!(fs.getattr(f.ino).unwrap().size, 11);
    assert_eq!(fs.lookup(ROOT_INO, "hello.txt").unwrap().ino, f.ino);
}

#[test]
fn data_survives_a_node_crash_and_restart() {
    let c = Cluster::start(2);
    let fs = c.fs(0);
    let f = fs.create(ROOT_INO, "keep", 0o600).unwrap();
    fs.write(f.ino, 0, b"durable").unwrap();

    c.crash(0);
    assert_eq!(fs.read(f.ino, 0, 10), Err(FsError::NodeDown));
    // The other node is unaffected.
    assert!(c.fs(1).readdir(ROOT_INO).unwrap().is_empty());

    c.restart(0);
    let again = fs.lookup(ROOT_INO, "keep").unwrap();
    assert_eq!(fs.read(again.ino, 0, 10).unwrap(), b"durable");
}

#[test]
fn directories_nest_list_and_remove() {
    let c = Cluster::start(1);
    let fs = c.fs(0);
    let d = fs.mkdir(ROOT_INO, "docs", 0o755).unwrap();
    assert_eq!(d.kind, FileKind::Dir);
    fs.create(d.ino, "b.txt", 0o644).unwrap();
    fs.create(d.ino, "a.txt", 0o644).unwrap();
    fs.mkdir(d.ino, "sub", 0o755).unwrap();
    assert_eq!(names(&fs, d.ino), ["a.txt", "b.txt", "sub"]);

    assert_eq!(fs.rmdir(ROOT_INO, "docs"), Err(FsError::NotEmpty));
    assert_eq!(fs.mkdir(ROOT_INO, "docs", 0o755), Err(FsError::Exists));
    assert_eq!(fs.unlink(d.ino, "sub"), Err(FsError::IsDir));
    assert_eq!(fs.rmdir(d.ino, "a.txt"), Err(FsError::NotDir));
    assert_eq!(fs.lookup(d.ino, "nope"), Err(FsError::NotFound));

    fs.unlink(d.ino, "a.txt").unwrap();
    fs.unlink(d.ino, "b.txt").unwrap();
    fs.rmdir(d.ino, "sub").unwrap();
    fs.rmdir(ROOT_INO, "docs").unwrap();
    assert!(names(&fs, ROOT_INO).is_empty());
}

#[test]
fn truncate_shrinks_and_zero_extends() {
    let c = Cluster::start(1);
    let fs = c.fs(0);
    let f = fs.create(ROOT_INO, "t", 0o644).unwrap();
    fs.write(f.ino, 0, b"0123456789").unwrap();

    fs.setattr(
        f.ino,
        SetAttr {
            size: Some(4),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(fs.read(f.ino, 0, 100).unwrap(), b"0123");

    fs.setattr(
        f.ino,
        SetAttr {
            size: Some(8),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(fs.read(f.ino, 0, 100).unwrap(), b"0123\0\0\0\0");
}

#[test]
fn writing_past_the_end_leaves_a_zero_filled_hole() {
    let c = Cluster::start(1);
    let fs = c.fs(0);
    let f = fs.create(ROOT_INO, "sparse", 0o644).unwrap();
    fs.write(f.ino, 4, b"xy").unwrap();
    assert_eq!(fs.read(f.ino, 0, 100).unwrap(), b"\0\0\0\0xy");
    fs.write(f.ino, 1, b"AB").unwrap();
    assert_eq!(fs.read(f.ino, 0, 100).unwrap(), b"\0AB\0xy");
}

#[test]
fn rename_moves_replaces_and_rejects_cycles() {
    let c = Cluster::start(1);
    let fs = c.fs(0);
    let a = fs.create(ROOT_INO, "a", 0o644).unwrap();
    fs.write(a.ino, 0, b"A").unwrap();
    let b = fs.create(ROOT_INO, "b", 0o644).unwrap();
    fs.write(b.ino, 0, b"B").unwrap();
    let d = fs.mkdir(ROOT_INO, "d", 0o755).unwrap();

    // Replace an existing file.
    fs.rename(ROOT_INO, "a", ROOT_INO, "b").unwrap();
    assert_eq!(names(&fs, ROOT_INO), ["b", "d"]);
    let moved = fs.lookup(ROOT_INO, "b").unwrap();
    assert_eq!(fs.read(moved.ino, 0, 10).unwrap(), b"A");

    // Move into a directory under a new name.
    fs.rename(ROOT_INO, "b", d.ino, "c").unwrap();
    assert_eq!(names(&fs, d.ino), ["c"]);

    // A directory cannot move into itself, and files cannot replace dirs.
    let sub = fs.mkdir(d.ino, "sub", 0o755).unwrap();
    assert_eq!(
        fs.rename(ROOT_INO, "d", sub.ino, "d"),
        Err(FsError::Invalid)
    );
    assert_eq!(fs.rename(d.ino, "c", ROOT_INO, "d"), Err(FsError::IsDir));
    assert_eq!(
        fs.rename(ROOT_INO, "missing", ROOT_INO, "x"),
        Err(FsError::NotFound)
    );
}

#[test]
fn symlinks_store_and_return_their_target() {
    let c = Cluster::start(1);
    let fs = c.fs(0);
    let l = fs.symlink(ROOT_INO, "link", "../some/where").unwrap();
    assert_eq!(l.kind, FileKind::Symlink);
    assert_eq!(fs.readlink(l.ino).unwrap(), "../some/where");
    let f = fs.create(ROOT_INO, "file", 0o644).unwrap();
    assert_eq!(fs.readlink(f.ino), Err(FsError::Invalid));
}

#[test]
fn permission_bits_are_stored_and_masked() {
    let c = Cluster::start(1);
    let fs = c.fs(0);
    let f = fs.create(ROOT_INO, "script.sh", 0o755).unwrap();
    assert_eq!(f.mode, 0o755);
    let a = fs
        .setattr(
            f.ino,
            SetAttr {
                mode: Some(0o4600),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(a.mode, 0o4600);
    assert_eq!(fs.getattr(f.ino).unwrap().mode, 0o4600);
    assert_eq!(fs.mkdir(ROOT_INO, "d", 0o700).unwrap().mode, 0o700);
}

#[test]
fn mtime_follows_the_clock_and_can_be_set() {
    let c = Cluster::start(1);
    let fs = c.fs(0);
    let t0 = c.now();
    let f = fs.create(ROOT_INO, "m", 0o644).unwrap();
    assert_eq!(f.mtime, t0);

    c.advance(std::time::Duration::from_secs(60));
    fs.write(f.ino, 0, b"x").unwrap();
    assert_eq!(fs.getattr(f.ino).unwrap().mtime, t0 + 60_000_000_000);

    let a = fs
        .setattr(
            f.ino,
            SetAttr {
                mtime: Some(5_000),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(a.mtime, 5_000);

    // Creating an entry updates the parent directory's mtime.
    c.advance(std::time::Duration::from_secs(10));
    fs.create(ROOT_INO, "other", 0o644).unwrap();
    assert_eq!(fs.getattr(ROOT_INO).unwrap().mtime, c.now());
}

#[test]
fn xattrs_set_get_list_remove() {
    let c = Cluster::start(1);
    let fs = c.fs(0);
    let f = fs.create(ROOT_INO, "x", 0o644).unwrap();
    assert_eq!(fs.getxattr(f.ino, "user.tag"), Err(FsError::NoData));
    fs.setxattr(f.ino, "user.tag", b"blue").unwrap();
    fs.setxattr(f.ino, "user.other", b"1").unwrap();
    assert_eq!(fs.getxattr(f.ino, "user.tag").unwrap(), b"blue");
    assert_eq!(fs.listxattr(f.ino).unwrap(), ["user.other", "user.tag"]);
    fs.removexattr(f.ino, "user.tag").unwrap();
    assert_eq!(fs.getxattr(f.ino, "user.tag"), Err(FsError::NoData));
    assert_eq!(fs.removexattr(f.ino, "user.tag"), Err(FsError::NoData));
}

#[test]
fn ownership_is_mapped_to_the_local_user_of_each_node() {
    let c = Cluster::start(2);
    for (i, uid) in [(0usize, 1000u32), (1, 1001)] {
        let fs = c.fs(i);
        let f = fs.create(ROOT_INO, "mine", 0o644).unwrap();
        assert_eq!((f.uid, f.gid), (uid, uid));
        // chown is accepted but ownership stays mapped to the local user.
        let a = fs
            .setattr(
                f.ino,
                SetAttr {
                    uid: Some(0),
                    gid: Some(0),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!((a.uid, a.gid), (uid, uid));
    }
}

#[test]
fn hardlinks_fail_with_a_clear_posix_error() {
    let c = Cluster::start(1);
    let fs = c.fs(0);
    let f = fs.create(ROOT_INO, "f", 0o644).unwrap();
    let err = fs.link(f.ino, ROOT_INO, "g").unwrap_err();
    assert_eq!(err.errno(), libc_eperm());
    assert!(fs.lookup(ROOT_INO, "g").is_err());
}

#[test]
fn acl_calls_fail_with_a_clear_posix_error() {
    let c = Cluster::start(1);
    let fs = c.fs(0);
    let f = fs.create(ROOT_INO, "f", 0o644).unwrap();
    for name in ["system.posix_acl_access", "system.posix_acl_default"] {
        let err = fs.setxattr(f.ino, name, b"\x02\0\0\0").unwrap_err();
        assert_eq!(err, FsError::NotSupported);
        assert_eq!(err.errno(), 95); // EOPNOTSUPP on Linux
        assert_eq!(fs.getxattr(f.ino, name), Err(FsError::NotSupported));
    }
}

#[test]
fn bad_names_are_rejected() {
    let c = Cluster::start(1);
    let fs = c.fs(0);
    assert_eq!(fs.create(ROOT_INO, "", 0o644), Err(FsError::Invalid));
    assert_eq!(fs.create(ROOT_INO, "a/b", 0o644), Err(FsError::Invalid));
    assert_eq!(
        fs.create(ROOT_INO, &"n".repeat(256), 0o644),
        Err(FsError::NameTooLong)
    );
    let f = fs.create(ROOT_INO, "f", 0o644).unwrap();
    assert_eq!(fs.create(f.ino, "child", 0o644), Err(FsError::NotDir));
}

fn libc_eperm() -> i32 {
    1
}

#[test]
fn creates_become_durable_on_fsync_or_after_the_quiet_period_but_not_on_close() {
    let c = Cluster::start_with_config(1, "sync_quiet_ms = 1000");
    let fs = c.fs(0);
    let f = fs.create(ROOT_INO, "a", 0o600).unwrap();
    fs.write(f.ino, 0, b"x").unwrap();
    fs.flush(f.ino).unwrap();
    assert!(fs.has_unsynced(), "close alone does not wait for disk");

    c.advance(std::time::Duration::from_millis(1500));
    c.tick();
    assert!(!fs.has_unsynced(), "quiet long enough: made durable");

    let g = fs.create(ROOT_INO, "b", 0o600).unwrap();
    fs.write(g.ino, 0, b"y").unwrap();
    fs.fsync(g.ino).unwrap();
    assert!(!fs.has_unsynced(), "fsync is durable at once");
    assert_eq!(fs.read(g.ino, 0, 10).unwrap(), b"y");
}

#[test]
fn a_file_larger_than_inline_is_durable_when_its_close_returns() {
    let c = Cluster::start_with_config(1, "sync_quiet_ms = 1000");
    let fs = c.fs(0);
    let f = fs.create(ROOT_INO, "big", 0o600).unwrap();
    fs.write(f.ino, 0, &vec![7u8; 200_000]).unwrap();
    fs.flush(f.ino).unwrap();
    assert!(
        !fs.has_unsynced(),
        "chunk-backed writes keep their durability"
    );
}
