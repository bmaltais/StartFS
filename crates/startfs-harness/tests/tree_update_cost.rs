//! Applying a peer's entries reads the objects they change, not the whole tree (#210): a
//! member holding a big tree pays the same per entry as one holding a small one.

use startfs_core::clock::SystemClock;
use startfs_core::fs::{CatchUp, LocalFs, NodeConfig};
use startfs_core::members::MemberId;
use startfs_core::ops::*;
use std::path::Path;
use std::sync::Arc;

fn open(id: u32, dir: &Path) -> LocalFs {
    let node = NodeConfig {
        node: MemberId(id),
        uid: 1000,
        gid: 1000,
    };
    LocalFs::open(dir, node, 1 << 20, Arc::new(SystemClock)).unwrap()
}

fn ship(from: &LocalFs, to: &LocalFs) {
    ship_by(from, to, 5000);
}

fn ship_by(from: &LocalFs, to: &LocalFs, limit: usize) {
    // Peers are sent only what is durable.
    from.sync().unwrap();
    loop {
        let CatchUp::Entries(entries) = from.catch_up(&to.frontier().unwrap(), limit).unwrap()
        else {
            panic!("expected entries");
        };
        if entries.is_empty() {
            return;
        }
        to.begin_batch();
        to.apply_remote(&entries).unwrap();
        to.end_batch().unwrap();
    }
}

/// Objects `b` reads to take over one change of each kind, holding `files` files.
fn reads_for_a_change(files: usize) -> u64 {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, b) = (open(1, da.path()), open(2, db.path()));
    let dir = a.mkdir(ROOT_INO, "d", 0o755).unwrap().ino;
    for i in 0..files {
        a.create(dir, &format!("f{i}"), 0o644).unwrap();
    }
    ship(&a, &b);
    let before = b.tree_objects_read();
    a.create(dir, "new", 0o644).unwrap();
    a.mkdir(dir, "sub", 0o755).unwrap();
    a.rename(dir, "f1", dir, "f1-renamed").unwrap();
    a.unlink(dir, "f2").unwrap();
    ship(&a, &b);
    b.tree_objects_read() - before
}

#[test]
fn a_change_reads_the_same_objects_on_a_small_tree_and_a_big_one() {
    let small = reads_for_a_change(100);
    let big = reads_for_a_change(3000);
    assert!(
        big <= small + 10,
        "{small} objects read on 100 files, {big} on 3000"
    );
}

/// Every shown name with its attributes, and the conflict copies.
fn dump(fs: &LocalFs) -> Vec<String> {
    let mut out = Vec::new();
    let mut todo = vec![(ROOT_INO, String::new())];
    while let Some((dir, path)) = todo.pop() {
        for e in fs.readdir(dir).unwrap() {
            let a = fs.getattr(e.ino).unwrap();
            let at = format!("{path}/{}", e.name);
            out.push(format!(
                "{at} ino={} {:?} mode={:o} nlink={} size={} mtime={} ctime={}",
                e.ino, a.kind, a.mode, a.nlink, a.size, a.mtime, a.ctime
            ));
            if a.kind == FileKind::Dir {
                todo.push((e.ino, at));
            }
        }
    }
    out.extend(fs.conflicts().unwrap().into_iter().map(|c| c.path));
    out.sort();
    out
}

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
}

/// One random operation by `fs`; failures (a missing name, a full directory) are fine.
fn step(fs: &LocalFs, rng: &mut Rng) {
    let dirs: Vec<Ino> = std::iter::once(ROOT_INO)
        .chain(
            fs.readdir(ROOT_INO)
                .unwrap()
                .into_iter()
                .filter(|e| e.kind == FileKind::Dir)
                .map(|e| e.ino),
        )
        .collect();
    let dir = dirs[rng.below(dirs.len())];
    let other = dirs[rng.below(dirs.len())];
    let name = format!("n{}", rng.below(6));
    let name2 = format!("n{}", rng.below(6));
    match rng.below(9) {
        0 | 1 => drop(fs.create(dir, &name, 0o644)),
        2 => drop(fs.mkdir(dir, &name, 0o755)),
        3 => drop(fs.rename(dir, &name, other, &name2)),
        4 => drop(fs.unlink(dir, &name)),
        5 => drop(fs.rmdir(dir, &name)),
        6 => {
            if let Ok(a) = fs.lookup(dir, &name)
                && a.kind == FileKind::File
            {
                drop(fs.write(a.ino, 0, b"hello"));
                drop(fs.flush(a.ino));
            }
        }
        7 => {
            if let Ok(a) = fs.lookup(dir, &name)
                && a.kind == FileKind::File
            {
                drop(fs.link(a.ino, other, &name2));
            }
        }
        _ => {
            if let Ok(a) = fs.lookup(dir, &name) {
                drop(fs.setattr(
                    a.ino,
                    SetAttr {
                        mode: Some(0o600),
                        ..Default::default()
                    },
                ));
            }
        }
    }
}

#[test]
fn the_tree_built_entry_by_entry_equals_the_rebuilt_one() {
    for seed in 1..=40u64 {
        let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
        let a = open(1, dirs[0].path());
        let b = open(2, dirs[1].path());
        let c = open(3, dirs[2].path());
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        for round in 0..12 {
            for _ in 0..rng.below(8) + 1 {
                step(&a, &mut rng);
            }
            for _ in 0..rng.below(4) {
                step(&b, &mut rng);
            }
            if rng.below(2) == 0 {
                ship(&a, &b);
            }
            ship(&a, &c);
            ship(&b, &c);
            let got = dump(&c);
            c.rebuild_tree().unwrap();
            assert_eq!(got, dump(&c), "seed {seed} round {round}");
        }
    }
}

#[test]
fn a_directory_arriving_after_what_is_in_it_shows_it() {
    // Node 1's file reaches node 3 before node 2's directory, which it sits in.
    let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let (a, b, c) = (
        open(2, dirs[0].path()),
        open(1, dirs[1].path()),
        open(3, dirs[2].path()),
    );
    let d = a.mkdir(ROOT_INO, "d", 0o755).unwrap().ino;
    ship(&a, &b);
    b.create(d, "f", 0o644).unwrap();
    ship_by(&b, &c, 1);
    let got = dump(&c);
    assert_eq!(got.len(), 2, "{got:?}");
    c.rebuild_tree().unwrap();
    assert_eq!(got, dump(&c));
}
