//! Merging operation logs into the visible tree.
//!
//! Every log entry is folded into per-object registers that do not depend on
//! arrival order; the visible tree is then rebuilt from those registers, so
//! all nodes holding the same entries show the same tree.

use super::*;
use crate::oplog::StateTransfer;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

/// One recorded state of an object.
#[derive(Serialize, Deserialize, Clone)]
struct Version {
    hlc: Hlc,
    base: Option<Hlc>,
    size: u64,
    mode: u32,
    mtime: Nanos,
    xattrs: BTreeMap<String, Vec<u8>>,
    content: Content,
}

/// Merge registers for one object.
#[derive(Serialize, Deserialize, Clone, Default)]
struct Object {
    kind: Option<FileKind>,
    /// Latest create or rename: where the object lives.
    loc: Option<(Hlc, Ino, String)>,
    /// Latest op that is not a delete.
    max_live: Hlc,
    max_del: Option<Hlc>,
    /// Versions nothing else is derived from.
    heads: Vec<Version>,
    /// Stamps of versions that something else is derived from.
    superseded: Vec<Hlc>,
}

impl Object {
    /// Fold another node's register for the same object into this one.
    fn join(&mut self, other: Object) {
        self.kind = self.kind.or(other.kind);
        if other.loc.as_ref().map(|l| l.0) > self.loc.as_ref().map(|l| l.0) {
            self.loc = other.loc;
        }
        self.max_live = self.max_live.max(other.max_live);
        self.max_del = self.max_del.max(other.max_del);
        for s in other.superseded {
            if !self.superseded.contains(&s) {
                self.superseded.push(s);
            }
        }
        for h in other.heads {
            if !self.heads.iter().any(|x| x.hlc == h.hlc) {
                self.heads.push(h);
            }
        }
        let superseded = &self.superseded;
        self.heads.retain(|h| !superseded.contains(&h.hlc));
    }

    fn winner(&self) -> Option<&Version> {
        self.heads.iter().max_by_key(|v| v.hlc)
    }
    fn alive(&self) -> bool {
        self.max_del.is_none_or(|d| self.max_live > d)
    }
    fn add_version(&mut self, v: Version) {
        if let Some(b) = v.base {
            self.heads.retain(|h| h.hlc != b);
            if !self.superseded.contains(&b) {
                self.superseded.push(b);
            }
        }
        if !self.superseded.contains(&v.hlc) && !self.heads.iter().any(|h| h.hlc == v.hlc) {
            self.heads.push(v);
        }
    }
}

/// What a peer needs to catch up with our log.
pub enum CatchUp {
    Entries(Vec<Entry>),
    State(StateTransfer),
}

/// An unresolved conflict copy, as shown by `status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictCopy {
    /// Where the copy lives in the filesystem.
    pub path: String,
}

/// Bookkeeping for a conflict copy in the tree.
#[derive(Serialize, Deserialize)]
struct ConflictRec {
    path: String,
    /// For a copy of an older version: the version it was saved from.
    seed: Option<Hlc>,
}

/// `report.txt` saved from node 3 at 2023-11-14T22:13:30Z becomes
/// `report (conflict from node3 20231114-221330).txt`.
fn conflict_name(name: &str, from: Hlc) -> String {
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => name.split_at(i),
        _ => (name, ""),
    };
    format!(
        "{stem} (conflict from node{} {}){ext}",
        from.node,
        stamp(from.wall)
    )
}

/// `YYYYMMDD-HHMMSS` in UTC.
fn stamp(wall: Nanos) -> String {
    let secs = wall / 1_000_000_000;
    let (days, rest) = (secs / 86_400, secs % 86_400);
    // Civil-from-days (proleptic Gregorian), after Howard Hinnant.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// Stable inode number of the conflict copy of `of`'s version stamped `from`.
fn conflict_ino(of: Ino, from: Hlc) -> Ino {
    let mut h = blake3::Hasher::new();
    h.update(&of.to_le_bytes());
    h.update(&from.wall.to_le_bytes());
    h.update(&from.counter.to_le_bytes());
    h.update(&from.node.to_le_bytes());
    let raw: [u8; 8] = h.finalize().as_bytes()[..8].try_into().expect("8 bytes");
    u64::from_le_bytes(raw) | (1 << 63)
}

fn enc<T: Serialize>(v: &T) -> Vec<u8> {
    postcard::to_stdvec(v).expect("serialises")
}

fn dec<'a, T: Deserialize<'a>>(raw: &'a [u8]) -> FsResult<T> {
    postcard::from_bytes(raw).map_err(|e| FsError::Io(format!("bad record: {e}")))
}

impl Tables<'_> {
    /// Forget what only matters while some member might not have seen it:
    /// superseded versions, and objects that are deleted. Safe only when every
    /// member holds the whole log. Conflict copies keep their records: a
    /// deleted one is how the tree remembers it was resolved.
    pub(super) fn purge_settled(&mut self) -> FsResult<()> {
        let mut all = Vec::new();
        for row in self.objects.iter().map_err(db)? {
            let (k, v) = row.map_err(db)?;
            all.push((k.value(), dec::<Object>(v.value())?));
        }
        for (ino, mut o) in all {
            if ino & (1 << 63) != 0 {
                continue;
            }
            if o.kind.is_none() || !o.alive() {
                self.objects.remove(ino).map_err(db)?;
            } else if !o.superseded.is_empty() {
                o.superseded.clear();
                self.put_object(ino, &o)?;
            }
        }
        Ok(())
    }

    fn object(&self, ino: Ino) -> FsResult<Object> {
        match self.objects.get(ino).map_err(db)? {
            Some(g) => dec(g.value()),
            None => Ok(Object::default()),
        }
    }
    fn put_object(&mut self, ino: Ino, o: &Object) -> FsResult<()> {
        self.objects.insert(ino, enc(o).as_slice()).map_err(db)?;
        Ok(())
    }
    fn conflict(&self, ino: Ino) -> FsResult<Option<ConflictRec>> {
        match self.conflicts.get(ino).map_err(db)? {
            Some(g) => Ok(Some(dec(g.value())?)),
            None => Ok(None),
        }
    }
    fn next_seq(&self, origin: u32) -> FsResult<u64> {
        Ok(self
            .frontier
            .get(origin)
            .map_err(db)?
            .map_or(0, |g| g.value()))
    }
}

impl LocalFs {
    /// The content and attributes of an inode as a log snapshot.
    pub(super) fn snapshot(&self, i: &Inode) -> Snapshot {
        Snapshot {
            mode: i.mode,
            mtime: i.mtime,
            xattrs: i.xattrs.clone(),
            content: i.content.clone(),
        }
    }

    /// Record a new state of `ino` (already applied to the visible tree).
    pub(super) fn emit_set(&self, t: &mut Tables<'_>, ino: Ino, i: &Inode) -> FsResult<()> {
        let base = match t.object(ino)?.winner() {
            Some(v) => Some(v.hlc),
            None => t.conflict(ino)?.and_then(|c| c.seed),
        };
        let snap = self.snapshot(i);
        self.emit(t, Op::Set { ino, base, snap })
    }

    /// Append a local operation to this node's log.
    pub(super) fn emit(&self, t: &mut Tables<'_>, op: Op) -> FsResult<()> {
        let origin = self.node().0;
        let hlc = {
            let mut h = self.hlc.lock().unwrap();
            *h = Hlc::tick(*h, self.clock.now(), origin);
            *h
        };
        let seq = t.next_seq(origin)?;
        let e = Entry {
            origin,
            seq,
            hlc,
            op,
        };
        self.record(t, &e)
    }

    /// Store an entry in the log and fold it into the registers.
    fn record(&self, t: &mut Tables<'_>, e: &Entry) -> FsResult<()> {
        t.log
            .insert((e.origin, e.seq), enc(e).as_slice())
            .map_err(db)?;
        t.frontier.insert(e.origin, e.seq + 1).map_err(db)?;
        let hlc = *self.hlc.lock().unwrap();
        t.counters.insert("hlc_wall", hlc.wall).map_err(db)?;
        t.counters
            .insert("hlc_counter", u64::from(hlc.counter))
            .map_err(db)?;
        self.register(t, e)
    }

    fn version(&self, hlc: Hlc, base: Option<Hlc>, snap: &Snapshot) -> Version {
        let size = match &snap.content {
            Content::Inline(b) => b.len() as u64,
            Content::Chunks(refs) => refs.iter().map(|r| u64::from(r.len)).sum(),
        };
        Version {
            hlc,
            base,
            size,
            mode: snap.mode,
            mtime: snap.mtime,
            xattrs: snap.xattrs.clone(),
            content: snap.content.clone(),
        }
    }

    fn register(&self, t: &mut Tables<'_>, e: &Entry) -> FsResult<()> {
        match &e.op {
            Op::Create {
                ino,
                kind,
                parent,
                name,
                snap,
            } => {
                let mut o = t.object(*ino)?;
                o.kind = Some(*kind);
                if o.loc.as_ref().is_none_or(|l| l.0 < e.hlc) {
                    o.loc = Some((e.hlc, *parent, name.clone()));
                }
                o.max_live = o.max_live.max(e.hlc);
                o.add_version(self.version(e.hlc, None, snap));
                t.put_object(*ino, &o)
            }
            Op::Set { ino, base, snap } => {
                let mut o = t.object(*ino)?;
                o.max_live = o.max_live.max(e.hlc);
                o.add_version(self.version(e.hlc, *base, snap));
                t.put_object(*ino, &o)
            }
            Op::Rename { ino, parent, name } => {
                let mut o = t.object(*ino)?;
                if o.loc.as_ref().is_none_or(|l| l.0 < e.hlc) {
                    o.loc = Some((e.hlc, *parent, name.clone()));
                }
                o.max_live = o.max_live.max(e.hlc);
                t.put_object(*ino, &o)
            }
            Op::Delete { ino } => {
                let mut o = t.object(*ino)?;
                o.max_del = o.max_del.max(Some(e.hlc));
                t.put_object(*ino, &o)
            }
        }
    }

    /// What a peer holding `have` needs to catch up: the entries it lacks, or,
    /// when some of those were compacted away, a copy of our merge state.
    pub fn catch_up(&self, have: &Frontier, limit: usize) -> FsResult<CatchUp> {
        self.run(false, |t| {
            for row in t.logbase.iter().map_err(db)? {
                let (origin, base) = row.map_err(db)?;
                if have.get(&origin.value()).copied().unwrap_or(0) < base.value() {
                    return Ok(CatchUp::State(self.state_in(t)?));
                }
            }
            Ok(CatchUp::Entries(Self::entries_in(t, have, limit)?))
        })
    }

    /// A copy of this node's merge state, as of its whole log.
    fn state_in(&self, t: &Tables<'_>) -> FsResult<StateTransfer> {
        let mut frontier = Frontier::new();
        for row in t.frontier.iter().map_err(db)? {
            let (k, v) = row.map_err(db)?;
            frontier.insert(k.value(), v.value());
        }
        let mut objects = Vec::new();
        for row in t.objects.iter().map_err(db)? {
            let (k, v) = row.map_err(db)?;
            objects.push((k.value(), v.value().to_vec()));
        }
        Ok(StateTransfer {
            frontier,
            hlc: *self.hlc.lock().unwrap(),
            objects,
        })
    }

    /// Join a peer's merge state into ours and carry on from where its log
    /// stood. Returns whether this moved us forward.
    pub fn apply_state(&self, st: &StateTransfer) -> FsResult<bool> {
        self.run(true, |t| {
            let mut ahead = false;
            for (origin, next) in &st.frontier {
                if t.next_seq(*origin)? < *next {
                    ahead = true;
                    // Entries below it are folded into the state we are given.
                    t.log
                        .retain_in((*origin, 0)..(*origin, *next), |_, _| false)
                        .map_err(db)?;
                    t.frontier.insert(*origin, *next).map_err(db)?;
                    t.logbase.insert(*origin, *next).map_err(db)?;
                }
            }
            if !ahead {
                return Ok(false);
            }
            for (ino, raw) in &st.objects {
                let mut o = t.object(*ino)?;
                o.join(dec(raw)?);
                t.put_object(*ino, &o)?;
            }
            {
                let mut h = self.hlc.lock().unwrap();
                *h = Hlc::observe(*h, st.hlc, self.clock.now());
                t.counters.insert("hlc_wall", h.wall).map_err(db)?;
                t.counters
                    .insert("hlc_counter", u64::from(h.counter))
                    .map_err(db)?;
            }
            self.reconcile(t)?;
            Ok(true)
        })
    }

    /// What this node holds of every node's log.
    pub fn frontier(&self) -> FsResult<Frontier> {
        self.run(false, |t| {
            let mut out = Frontier::new();
            for row in t.frontier.iter().map_err(db)? {
                let (k, v) = row.map_err(db)?;
                out.insert(k.value(), v.value());
            }
            Ok(out)
        })
    }

    /// Entries a peer holding `have` lacks, in per-origin order, at most `limit`.
    fn entries_in(t: &Tables<'_>, have: &Frontier, limit: usize) -> FsResult<Vec<Entry>> {
        let mut out = Vec::new();
        for row in t.frontier.iter().map_err(db)? {
            let origin = row.map_err(db)?.0.value();
            let from = have.get(&origin).copied().unwrap_or(0);
            for row in t
                .log
                .range((origin, from)..=(origin, u64::MAX))
                .map_err(db)?
            {
                if out.len() >= limit {
                    return Ok(out);
                }
                out.push(dec(row.map_err(db)?.1.value())?);
            }
        }
        Ok(out)
    }

    /// Apply entries received from a peer; returns how many were new.
    pub fn apply_remote(&self, entries: &[Entry]) -> FsResult<usize> {
        self.run(true, |t| {
            let mut applied = 0;
            for e in entries {
                if e.seq != t.next_seq(e.origin)? {
                    continue; // duplicate, or a gap the peer will fill in order
                }
                {
                    let mut h = self.hlc.lock().unwrap();
                    *h = Hlc::observe(*h, e.hlc, self.clock.now());
                }
                self.record(t, e)?;
                applied += 1;
            }
            if applied > 0 {
                self.reconcile(t)?;
            }
            Ok(applied)
        })
    }

    /// Conflict copies currently in the tree.
    pub fn conflicts(&self) -> FsResult<Vec<ConflictCopy>> {
        self.run(false, |t| {
            let mut out = Vec::new();
            for row in t.conflicts.iter().map_err(db)? {
                let rec: ConflictRec = dec(row.map_err(db)?.1.value())?;
                out.push(ConflictCopy { path: rec.path });
            }
            out.sort_by(|a, b| a.path.cmp(&b.path));
            Ok(out)
        })
    }

    /// Rebuild the visible tree from the registers.
    fn reconcile(&self, t: &mut Tables<'_>) -> FsResult<()> {
        let mut objects: BTreeMap<Ino, Object> = BTreeMap::new();
        for row in t.objects.iter().map_err(db)? {
            let (k, v) = row.map_err(db)?;
            objects.insert(k.value(), dec(v.value())?);
        }

        struct Placed {
            ino: Ino,
            kind: FileKind,
            parent: Ino,
            name: String,
            version: Version,
            /// Set for conflict copies of an older version.
            seed: Option<Hlc>,
            conflict: bool,
        }
        // Live objects, plus any directory a live object needs to sit in: a
        // write into a deleted directory recreates the path.
        let mut alive: HashSet<Ino> = objects
            .iter()
            .filter(|(_, o)| o.kind.is_some() && o.loc.is_some() && o.alive())
            .map(|(ino, _)| *ino)
            .collect();
        for start in alive.clone() {
            let mut cur = start;
            for _ in 0..4096 {
                let Some((_, parent, _)) = objects.get(&cur).and_then(|o| o.loc.as_ref()) else {
                    break;
                };
                let is_dir = objects
                    .get(parent)
                    .is_some_and(|p| p.kind == Some(FileKind::Dir) && p.loc.is_some());
                if *parent == ROOT_INO || !is_dir {
                    break;
                }
                alive.insert(*parent);
                cur = *parent;
            }
        }

        let mut placed: Vec<Placed> = Vec::new();
        for (ino, o) in &objects {
            let (Some(kind), Some((_, parent, name)), Some(v)) = (o.kind, &o.loc, o.winner())
            else {
                continue;
            };
            if alive.contains(ino) {
                placed.push(Placed {
                    ino: *ino,
                    kind,
                    parent: *parent,
                    name: name.clone(),
                    version: v.clone(),
                    seed: None,
                    conflict: false,
                });
                if kind == FileKind::Dir {
                    continue;
                }
                // Versions written concurrently with the winner survive as copies.
                for loser in o.heads.iter().filter(|h| h.hlc != v.hlc) {
                    let cino = conflict_ino(*ino, loser.hlc);
                    let s = objects.get(&cino);
                    if s.is_some_and(|s| s.max_del.is_some()) {
                        continue; // the user resolved it
                    }
                    let (parent, name) = s
                        .and_then(|s| s.loc.as_ref())
                        .map_or((*parent, conflict_name(name, loser.hlc)), |l| {
                            (l.1, l.2.clone())
                        });
                    placed.push(Placed {
                        ino: cino,
                        kind,
                        parent,
                        name,
                        version: s.and_then(Object::winner).unwrap_or(loser).clone(),
                        seed: Some(loser.hlc),
                        conflict: true,
                    });
                }
            }
        }

        let mut by_parent: HashMap<Ino, Vec<usize>> = HashMap::new();
        for (idx, p) in placed.iter().enumerate() {
            by_parent.entry(p.parent).or_default().push(idx);
        }

        t.dirents.retain(|_, _| false).map_err(db)?;
        t.conflicts.retain(|_, _| false).map_err(db)?;
        t.inodes.retain(|k, _| k == ROOT_INO).map_err(db)?;
        let mut queue = VecDeque::from([ROOT_INO]);
        let mut paths: HashMap<Ino, String> = HashMap::from([(ROOT_INO, String::new())]);
        // Directories that lost a name collision to another directory: their
        // children appear in the winner.
        let mut merged: HashMap<Ino, Vec<Ino>> = HashMap::new();
        while let Some(dir) = queue.pop_front() {
            let mut groups: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
            let sources = std::iter::once(dir).chain(merged.get(&dir).cloned().unwrap_or_default());
            for source in sources {
                for &idx in by_parent.get(&source).into_iter().flatten() {
                    groups
                        .entry(placed[idx].name.as_str())
                        .or_default()
                        .push(idx);
                }
            }
            // (index, name to show, whether it is shown as a conflict)
            let mut shown: Vec<(usize, String, bool)> = Vec::new();
            for (name, mut group) in groups {
                group.sort_by_key(|&i| std::cmp::Reverse(placed[i].version.hlc));
                let winner = group[0];
                shown.push((winner, name.to_string(), placed[winner].conflict));
                for &loser in &group[1..] {
                    if placed[winner].kind == FileKind::Dir && placed[loser].kind == FileKind::Dir {
                        merged
                            .entry(placed[winner].ino)
                            .or_default()
                            .push(placed[loser].ino);
                        continue;
                    }
                    let mut copy = conflict_name(name, placed[loser].version.hlc);
                    while shown.iter().any(|(_, n, _)| *n == copy) {
                        copy.push_str(" 2");
                    }
                    shown.push((loser, copy, true));
                }
            }
            for (idx, name, conflict) in shown {
                let p = &placed[idx];
                let v = &p.version;
                let inode = Inode {
                    kind: p.kind,
                    mode: v.mode,
                    nlink: if p.kind == FileKind::Dir { 2 } else { 1 },
                    size: v.size,
                    mtime: v.mtime,
                    ctime: v.mtime,
                    parent: dir,
                    content: v.content.clone(),
                    xattrs: v.xattrs.clone(),
                };
                t.put(p.ino, &inode)?;
                t.dirents.insert((dir, name.as_str()), p.ino).map_err(db)?;
                let path = format!("{}/{name}", paths[&dir]);
                if conflict {
                    let rec = ConflictRec {
                        path: path.clone(),
                        seed: p.seed,
                    };
                    t.conflicts
                        .insert(p.ino, enc(&rec).as_slice())
                        .map_err(db)?;
                }
                if p.kind == FileKind::Dir {
                    paths.insert(p.ino, path);
                    queue.push_back(p.ino);
                }
            }
        }
        Ok(())
    }
}
