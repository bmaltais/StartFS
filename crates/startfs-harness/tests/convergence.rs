//! Randomized check that nodes converge: two nodes make random changes while
//! partitioned, then merge, and must show the same tree with the right link counts.

use startfs_core::net::NodeId;
use startfs_core::ops::*;
use startfs_core::sync::HELLO_INTERVAL;
use startfs_harness::Cluster;
use std::collections::HashMap;
use std::time::Duration;

/// A small deterministic generator, so a failing seed replays exactly.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 33) as usize) % n
    }
}

type Listing = Vec<(Ino, String, Ino, FileKind)>;

/// Every entry as (directory, name, ino, kind), in a stable order.
fn listing(fs: &impl FsOps) -> Listing {
    let mut out = Vec::new();
    let mut stack = vec![ROOT_INO];
    while let Some(dir) = stack.pop() {
        for e in fs.readdir(dir).unwrap() {
            if e.kind == FileKind::Dir {
                stack.push(e.ino);
            }
            out.push((dir, e.name, e.ino, e.kind));
        }
    }
    out.sort_by(|a, b| (a.0, &a.1, a.2).cmp(&(b.0, &b.1, b.2)));
    out
}

/// Each file's link count must be the number of names it shows.
fn check_links(fs: &impl FsOps, list: &Listing) -> Result<(), String> {
    let mut names: HashMap<Ino, u32> = HashMap::new();
    for (_, _, ino, kind) in list {
        if *kind != FileKind::Dir {
            *names.entry(*ino).or_default() += 1;
        }
    }
    for (ino, n) in names {
        let nlink = fs.getattr(ino).map_err(|e| format!("{ino}: {e:?}"))?.nlink;
        if nlink != n {
            return Err(format!("file {ino} has nlink {nlink} but {n} names"));
        }
    }
    Ok(())
}

/// One random operation on `node`, described for the failure message.
fn step(c: &Cluster, node: usize, rng: &mut Rng, writes: &mut u32) -> String {
    let fs = c.fs(node);
    let entries = listing(&fs);
    let dirs: Vec<Ino> = std::iter::once(ROOT_INO)
        .chain(entries.iter().filter(|e| e.3 == FileKind::Dir).map(|e| e.2))
        .collect();
    let dir = dirs[rng.below(dirs.len())];
    let name = format!("n{}", rng.below(6));
    let existing = if entries.is_empty() {
        None
    } else {
        Some(entries[rng.below(entries.len())].clone())
    };
    match (rng.below(7), existing) {
        (0, _) => format!(
            "create {dir}/{name}: {:?}",
            fs.create(dir, &name, 0o644).err()
        ),
        (1, _) => format!(
            "mkdir {dir}/{name}: {:?}",
            fs.mkdir(dir, &name, 0o755).err()
        ),
        (2 | 3, Some((_, _, ino, kind))) if kind != FileKind::Dir => {
            format!(
                "link {ino} -> {dir}/{name}: {:?}",
                fs.link(ino, dir, &name).err()
            )
        }
        (4, Some((parent, n, _, kind))) => {
            let r = if kind == FileKind::Dir {
                fs.rmdir(parent, &n)
            } else {
                fs.unlink(parent, &n)
            };
            format!("remove {parent}/{n}: {:?}", r.err())
        }
        (5, Some((parent, n, _, _))) => {
            format!(
                "rename {parent}/{n} -> {dir}/{name}: {:?}",
                fs.rename(parent, &n, dir, &name).err()
            )
        }
        (_, Some((_, _, ino, FileKind::File))) => {
            *writes += 1;
            let wrote = fs.write(ino, 0, format!("w{writes}").as_bytes()).err();
            // Closing the file commits the write.
            format!("write {ino}: {wrote:?} {:?}", fs.flush(ino).err())
        }
        _ => "nothing".into(),
    }
}

/// Run one seed; `Err` says what went wrong and the operations that led to it.
fn run(seed: u64) -> Result<(), String> {
    let c = Cluster::start(2);
    let token = c.control(0).create("home", 2).unwrap();
    c.control(1).join(&token, None).unwrap();
    c.sync();
    let mut rng = Rng(seed + 1);
    let mut writes = 0;
    let mut log = Vec::new();
    let fail =
        |what: String, log: &[String]| Err(format!("seed {seed}: {what}\n{}", log.join("\n")));
    for round in 0..6 {
        c.network().partition(&[&[NodeId(0)], &[NodeId(1)]]);
        log.push(format!("-- round {round}"));
        for _ in 0..8 {
            let node = rng.below(2);
            c.advance(Duration::from_secs(1));
            let done = step(&c, node, &mut rng, &mut writes);
            log.push(format!("node {node}: {done}"));
            if let Err(e) = check_links(&c.fs(node), &listing(&c.fs(node))) {
                return fail(format!("round {round}, node {node}: {e}"), &log);
            }
        }
        c.network().heal();
        c.advance(HELLO_INTERVAL);
        c.sync();
        // Buffered writes commit once idle; let them reach the other node.
        for _ in 0..3 {
            c.advance(Duration::from_secs(5));
            c.sync();
        }
        let (a, b) = (listing(&c.fs(0)), listing(&c.fs(1)));
        if a != b {
            return fail(
                format!("round {round}: the nodes disagree\n{a:?}\n{b:?}"),
                &log,
            );
        }
        for node in 0..2 {
            if let Err(e) = check_links(&c.fs(node), &a) {
                return fail(format!("round {round}, node {node} after merge: {e}"), &log);
            }
        }
    }
    Ok(())
}

#[test]
fn random_changes_on_two_nodes_converge_with_correct_link_counts() {
    // `SEED=n` replays one seed.
    let seeds = match std::env::var("SEED").ok().and_then(|s| s.parse().ok()) {
        Some(one) => one..one + 1,
        None => {
            // `SEEDS=n` runs more.
            let n = std::env::var("SEEDS").ok().and_then(|s| s.parse().ok());
            let start = std::env::var("START")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            start..start + n.unwrap_or(SEEDS)
        }
    };
    let failures: Vec<String> = seeds.filter_map(|seed| run(seed).err()).collect();
    for f in &failures {
        eprintln!("FAILED {}", f.lines().next().unwrap_or_default());
    }
    assert!(
        failures.is_empty(),
        "{} of {SEEDS} seeds failed; first:\n{}",
        failures.len(),
        failures[0]
    );
}

/// Seeds run by default; `SEEDS=n` and `START=n` run others, `SEED=n` one.
const SEEDS: u64 = 60;
