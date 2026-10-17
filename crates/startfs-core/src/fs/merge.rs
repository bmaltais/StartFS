//! Merging operation logs into the visible tree.
//!
//! Every log entry is folded into per-object registers that do not depend on
//! arrival order; the visible tree is then rebuilt from those registers, so
//! all nodes holding the same entries show the same tree.

use super::*;
use crate::oplog::{StateChunk, StatePin};
use redb::ReadableTableMetadata;
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

/// Where a name lives: when it got there, the parent directory and the name.
type Loc = (Hlc, Ino, String);

/// Merge registers for one name of an object.
#[derive(Serialize, Deserialize, Clone, Default)]
struct Name {
    /// Latest create, link or rename of this name.
    loc: Option<Loc>,
    /// Latest unlink of this name.
    del: Option<Hlc>,
    /// Latest write made while this name was live and not known to be unlinked.
    seen: Option<Hlc>,
}

impl Name {
    /// A name is live while it has been placed, or written through by someone
    /// who had not seen it unlinked, since it was last unlinked.
    fn alive(&self) -> bool {
        self.loc.as_ref().is_some_and(|l| {
            self.del
                .is_none_or(|d| l.0 > d || self.seen.is_some_and(|s| s > d))
        })
    }

    fn place(&mut self, at: Hlc, parent: Ino, name: &str) {
        if self.loc.as_ref().is_none_or(|l| l.0 < at) {
            self.loc = Some((at, parent, name.to_string()));
        }
    }

    fn join(&mut self, other: Name) {
        if other.loc.as_ref().map(|l| l.0) > self.loc.as_ref().map(|l| l.0) {
            self.loc = other.loc;
        }
        self.del = self.del.max(other.del);
        self.seen = self.seen.max(other.seen);
    }
}

/// Merge registers for one object.
#[derive(Serialize, Deserialize, Clone, Default)]
struct Object {
    kind: Option<FileKind>,
    /// The names, by the stamp of the op that made each (`Hlc::ZERO` for the
    /// one a conflict copy gets when it is renamed or removed).
    names: BTreeMap<Hlc, Name>,
    /// Versions nothing else is derived from.
    heads: Vec<Version>,
    /// Stamps of versions that something else is derived from.
    superseded: Vec<Hlc>,
}

impl Object {
    /// Fold another node's register for the same object into this one.
    fn join(&mut self, other: Object) {
        self.kind = self.kind.or(other.kind);
        for (slot, n) in other.names {
            self.names.entry(slot).or_default().join(n);
        }
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

    /// The live names.
    fn live(&self) -> Vec<(Hlc, &Loc)> {
        self.names
            .iter()
            .filter(|(_, n)| n.alive())
            .filter_map(|(slot, n)| Some((*slot, n.loc.as_ref()?)))
            .collect()
    }

    /// Where the most recently placed name is, live or not.
    fn home(&self) -> Option<(Hlc, &Loc)> {
        self.names
            .iter()
            .filter_map(|(slot, n)| Some((*slot, n.loc.as_ref()?)))
            .max_by_key(|(_, l)| l.0)
    }

    /// Where to show the object: its live names, or for a directory that a
    /// live object needs to sit in, the place it last was.
    fn placements(&self) -> Vec<(Hlc, &Loc)> {
        let live = self.live();
        if live.is_empty() {
            self.home().into_iter().collect()
        } else {
            live
        }
    }

    /// Whether any of its names has been unlinked.
    fn unlinked(&self) -> bool {
        self.names.values().any(|n| n.del.is_some())
    }

    fn winner(&self) -> Option<&Version> {
        self.heads.iter().max_by_key(|v| v.hlc)
    }
    fn alive(&self) -> bool {
        !self.live().is_empty()
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
    /// What the peer misses was compacted away: start a state transfer.
    State(StatePin),
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
    /// The directory entry that shows it.
    entry: (Ino, String),
    /// The directory and name the copy is named after: the name it lost, or
    /// the name of the object it was saved from.
    base: (Ino, String),
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
    h.update(&from.node.0.to_le_bytes());
    let raw: [u8; 8] = h.finalize().as_bytes()[..8].try_into().expect("8 bytes");
    u64::from_le_bytes(raw) | (1 << 63)
}

const LOST_FOUND: &str = "lost+found";

/// Stable inode number of the `lost+found` directory that cycle-breaking adds.
pub(super) fn lost_found_ino() -> Ino {
    let raw: [u8; 8] = blake3::hash(LOST_FOUND.as_bytes()).as_bytes()[..8]
        .try_into()
        .expect("8 bytes");
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
            } else {
                // Nobody can still be told about a removed name.
                let keep_dead = !o.names.values().any(Name::alive);
                o.names.retain(|_, n| keep_dead || n.alive());
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
    /// The records of the entries showing `ino` as a conflict: one for each
    /// name of a hardlinked file that lost a collision.
    fn conflict_recs(&self, ino: Ino) -> FsResult<Vec<ConflictRec>> {
        match self.conflicts.get(ino).map_err(db)? {
            Some(g) => dec(g.value()),
            None => Ok(Vec::new()),
        }
    }
    fn conflict(&self, ino: Ino) -> FsResult<Option<ConflictRec>> {
        Ok(self.conflict_recs(ino)?.into_iter().next())
    }
    /// Forget the conflict record of one name of `ino`, which has gone.
    pub(super) fn drop_conflict(&mut self, ino: Ino, parent: Ino, name: &str) -> FsResult<()> {
        let mut recs = self.conflict_recs(ino)?;
        let before = recs.len();
        recs.retain(|r| r.entry != (parent, name.to_string()));
        if recs.len() != before {
            if recs.is_empty() {
                self.conflicts.remove(ino).map_err(db)?;
            } else {
                self.conflicts
                    .insert(ino, enc(&recs).as_slice())
                    .map_err(db)?;
            }
        }
        Ok(())
    }
    /// The names (object and name) that the entry `(parent, name)` shows. Several
    /// when names of one file landed on one string, or directories of one name
    /// were merged: the entry stands for all of them.
    pub(super) fn slots(&self, parent: Ino, name: &str, shown: Ino) -> FsResult<Vec<(Ino, Hlc)>> {
        match self.names.get((parent, name)).map_err(db)? {
            Some(g) => dec(g.value()),
            None => Ok(vec![(shown, Hlc::ZERO)]),
        }
    }
    pub(super) fn set_slots(
        &mut self,
        parent: Ino,
        name: &str,
        slots: &[(Ino, Hlc)],
    ) -> FsResult<()> {
        self.names
            .insert((parent, name), enc(&slots).as_slice())
            .map_err(db)?;
        Ok(())
    }

    /// Whether a directory only shows because something in it needs the path:
    /// it was removed, and goes once its last entry does.
    pub(super) fn is_ghost(&self, dir: Ino) -> FsResult<bool> {
        Ok(dir != ROOT_INO && {
            let o = self.object(dir)?;
            o.kind == Some(FileKind::Dir) && !o.alive()
        })
    }

    /// Whether a conflict copy is named after `(parent, name)`.
    pub(super) fn copies_at(&self, parent: Ino, name: &str) -> FsResult<bool> {
        for row in self.conflicts.iter().map_err(db)? {
            let recs: Vec<ConflictRec> = dec(row.map_err(db)?.1.value())?;
            if recs.iter().any(|r| r.base.0 == parent && r.base.1 == name) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Whether the object takes part in a name collision, which its newest
    /// version decides: it lost one, or conflict copies are named after its name.
    pub(super) fn collides(&self, ino: Ino) -> FsResult<bool> {
        if self.conflicts.is_empty().map_err(db)? {
            return Ok(false);
        }
        if self.conflicts.get(ino).map_err(db)?.is_some() {
            return Ok(true);
        }
        for (_, loc) in self.object(ino)?.placements() {
            if self.copies_at(loc.1, &loc.2)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Whether the object has versions shown as conflict copies, which take
    /// their names from the object's.
    pub(super) fn has_copies(&self, ino: Ino) -> FsResult<bool> {
        Ok(self.object(ino)?.heads.len() > 1)
    }
    fn next_seq(&self, origin: MemberId) -> FsResult<u64> {
        Ok(self
            .frontier
            .get(origin.0)
            .map_err(db)?
            .map_or(0, |g| g.value()))
    }
}

struct Placed {
    ino: Ino,
    kind: FileKind,
    parent: Ino,
    name: String,
    /// Which of the object's names this is.
    slot: Hlc,
    version: Version,
    /// Set for conflict copies of an older version.
    seed: Option<Hlc>,
    /// For those, the directory and name of the object they were saved from.
    base: Option<(Ino, String)>,
    conflict: bool,
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
        self.emit_set_at(t, ino, i, self.clock.now())
    }

    /// Like [`Self::emit_set`], stamped with the time the change was made
    /// rather than now (a buffered write is committed later than it happened).
    pub(super) fn emit_set_at(
        &self,
        t: &mut Tables<'_>,
        ino: Ino,
        i: &Inode,
        wall: Nanos,
    ) -> FsResult<()> {
        let base = match t.object(ino)?.winner() {
            Some(v) => Some(v.hlc),
            None => t.conflict(ino)?.and_then(|c| c.seed),
        };
        let snap = self.snapshot(i);
        let names = t
            .object(ino)?
            .live()
            .iter()
            .map(|(slot, _)| *slot)
            .collect();
        let op = Op::Set {
            ino,
            base,
            snap,
            names,
        };
        self.emit_at(t, op, wall)?;
        // The newest version decides who keeps a name two objects claim.
        if t.collides(ino)? {
            self.reconcile(t)?;
        }
        Ok(())
    }

    /// Append a local operation to this node's log.
    /// Returns the entry's stamp.
    pub(super) fn emit(&self, t: &mut Tables<'_>, op: Op) -> FsResult<Hlc> {
        self.emit_at(t, op, self.clock.now())
    }

    fn emit_at(&self, t: &mut Tables<'_>, op: Op, wall: Nanos) -> FsResult<Hlc> {
        let origin = self.node();
        let hlc = {
            let mut h = self.hlc.lock().unwrap();
            *h = Hlc::tick(*h, wall, origin);
            *h
        };
        let seq = t.next_seq(origin)?;
        let e = Entry {
            origin,
            seq,
            hlc,
            op,
        };
        self.record(t, &e)?;
        Ok(hlc)
    }

    /// Store an entry in the log and fold it into the registers.
    fn record(&self, t: &mut Tables<'_>, e: &Entry) -> FsResult<()> {
        t.log
            .insert((e.origin.0, e.seq), enc(e).as_slice())
            .map_err(db)?;
        t.frontier.insert(e.origin.0, e.seq + 1).map_err(db)?;
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
                o.names
                    .entry(e.hlc)
                    .or_default()
                    .place(e.hlc, *parent, name);
                o.add_version(self.version(e.hlc, None, snap));
                t.put_object(*ino, &o)
            }
            Op::Set {
                ino,
                base,
                snap,
                names,
            } => {
                let mut o = t.object(*ino)?;
                for slot in names {
                    let n = o.names.entry(*slot).or_default();
                    n.seen = n.seen.max(Some(e.hlc));
                }
                o.add_version(self.version(e.hlc, *base, snap));
                t.put_object(*ino, &o)
            }
            Op::Link { ino, parent, name } => {
                let mut o = t.object(*ino)?;
                o.names
                    .entry(e.hlc)
                    .or_default()
                    .place(e.hlc, *parent, name);
                t.put_object(*ino, &o)
            }
            Op::Rename {
                ino,
                slot,
                parent,
                name,
            } => {
                let mut o = t.object(*ino)?;
                o.names
                    .entry(*slot)
                    .or_default()
                    .place(e.hlc, *parent, name);
                t.put_object(*ino, &o)
            }
            Op::Unlink { ino, slot } => {
                let mut o = t.object(*ino)?;
                let n = o.names.entry(*slot).or_default();
                n.del = n.del.max(Some(e.hlc));
                t.put_object(*ino, &o)
            }
        }
    }

    /// What a peer holding `have` needs to catch up: the entries it lacks, or,
    /// when some of those were compacted away, where to start a state transfer.
    pub fn catch_up(&self, have: &Frontier, limit: usize) -> FsResult<CatchUp> {
        self.run(false, |t| {
            for row in t.logbase.iter().map_err(db)? {
                let (origin, base) = row.map_err(db)?;
                if have.get(&MemberId(origin.value())).copied().unwrap_or(0) < base.value() {
                    return Ok(CatchUp::State(self.state_pin(t)?));
                }
            }
            Ok(CatchUp::Entries(Self::entries_in(t, have, limit)?))
        })
    }

    /// Where a state transfer starts: our frontier and clock right now.
    fn state_pin(&self, t: &Tables<'_>) -> FsResult<StatePin> {
        let mut frontier = Frontier::new();
        for row in t.frontier.iter().map_err(db)? {
            let (k, v) = row.map_err(db)?;
            frontier.insert(MemberId(k.value()), v.value());
        }
        Ok(StatePin {
            frontier,
            hlc: *self.hlc.lock().unwrap(),
        })
    }

    /// The next piece of a state transfer: our merge registers after `after`
    /// (from the first when `None`), up to about `max_bytes` encoded, cut on
    /// object boundaries (one object over the bound goes whole).
    pub fn state_chunk(
        &self,
        pin: &StatePin,
        after: Option<Ino>,
        max_bytes: usize,
    ) -> FsResult<StateChunk> {
        self.run(false, |t| {
            let from = after.map_or(0, |a| a + 1);
            let mut objects = Vec::new();
            let mut bytes = 0;
            let mut last = true;
            for row in t.objects.range(from..).map_err(db)? {
                let (k, v) = row.map_err(db)?;
                let size = std::mem::size_of::<Ino>() + v.value().len();
                if !objects.is_empty() && bytes + size > max_bytes {
                    last = false;
                    break;
                }
                bytes += size;
                objects.push((k.value(), v.value().to_vec()));
            }
            Ok(StateChunk {
                pin: pin.clone(),
                objects,
                last,
            })
        })
    }

    /// Join a chunk of a peer's merge state into ours. With the last chunk,
    /// carry on from where the peer's log stood. Returns whether this moved
    /// us forward. Earlier chunks leave our frontier alone, so a transfer that
    /// breaks off never claims history we do not hold.
    pub fn apply_state_chunk(&self, chunk: &StateChunk) -> FsResult<bool> {
        self.run(true, |t| {
            for (ino, raw) in &chunk.objects {
                let mut o = t.object(*ino)?;
                o.join(dec(raw)?);
                t.put_object(*ino, &o)?;
            }
            if !chunk.last {
                return Ok(false);
            }
            let mut ahead = false;
            for (origin, next) in &chunk.pin.frontier {
                if t.next_seq(*origin)? < *next {
                    ahead = true;
                    // Entries below it are folded into the state we are given.
                    t.log
                        .retain_in((origin.0, 0)..(origin.0, *next), |_, _| false)
                        .map_err(db)?;
                    t.frontier.insert(origin.0, *next).map_err(db)?;
                    t.logbase.insert(origin.0, *next).map_err(db)?;
                }
            }
            {
                let mut h = self.hlc.lock().unwrap();
                *h = Hlc::observe(*h, chunk.pin.hlc, self.clock.now());
                t.counters.insert("hlc_wall", h.wall).map_err(db)?;
                t.counters
                    .insert("hlc_counter", u64::from(h.counter))
                    .map_err(db)?;
            }
            self.reconcile(t)?;
            Ok(ahead)
        })
    }

    /// What this node holds of every node's log.
    pub fn frontier(&self) -> FsResult<Frontier> {
        self.run(false, |t| {
            let mut out = Frontier::new();
            for row in t.frontier.iter().map_err(db)? {
                let (k, v) = row.map_err(db)?;
                out.insert(MemberId(k.value()), v.value());
            }
            Ok(out)
        })
    }

    /// Entries a peer holding `have` lacks, in per-origin order, at most `limit`.
    fn entries_in(t: &Tables<'_>, have: &Frontier, limit: usize) -> FsResult<Vec<Entry>> {
        let mut out = Vec::new();
        for row in t.frontier.iter().map_err(db)? {
            let origin = MemberId(row.map_err(db)?.0.value());
            let from = have.get(&origin).copied().unwrap_or(0);
            for row in t
                .log
                .range((origin.0, from)..=(origin.0, u64::MAX))
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
                let recs: Vec<ConflictRec> = dec(row.map_err(db)?.1.value())?;
                out.extend(recs.into_iter().map(|r| ConflictCopy { path: r.path }));
            }
            out.sort_by(|a, b| a.path.cmp(&b.path));
            Ok(out)
        })
    }

    /// Concurrent renames can leave directories pointing at each other, out of
    /// reach of the root. In each such cycle the directory whose location
    /// changed last gives way: it moves, with its subtree, to `lost+found`.
    fn break_cycles(objects: &BTreeMap<Ino, Object>, placed: &mut Vec<Placed>) {
        let dirs: HashMap<Ino, usize> = placed
            .iter()
            .enumerate()
            .filter(|(_, p)| p.kind == FileKind::Dir && !p.conflict)
            .map(|(i, p)| (p.ino, i))
            .collect();
        let moved_at = |i: usize| objects[&placed[i].ino].home().map(|(_, l)| l.0);
        // 0 = unvisited, 1 = on the current walk, 2 = done.
        let mut state: HashMap<Ino, u8> = HashMap::new();
        let mut losers: Vec<usize> = Vec::new();
        for &start in dirs.keys() {
            let mut walk = Vec::new();
            let mut cur = start;
            loop {
                match state.get(&cur).copied().unwrap_or(0) {
                    0 => {
                        state.insert(cur, 1);
                        walk.push(cur);
                        match dirs.get(&cur).map(|&i| placed[i].parent) {
                            Some(parent) if dirs.contains_key(&parent) => cur = parent,
                            _ => break,
                        }
                    }
                    1 => {
                        let from = walk.iter().position(|&d| d == cur).expect("on walk");
                        let loser = walk[from..]
                            .iter()
                            .map(|d| dirs[d])
                            .max_by_key(|&i| (moved_at(i), placed[i].ino))
                            .expect("cycle is not empty");
                        losers.push(loser);
                        break;
                    }
                    _ => break,
                }
            }
            for d in walk {
                state.insert(d, 2);
            }
        }
        if losers.is_empty() {
            return;
        }
        let wall = losers
            .iter()
            .filter_map(|&i| moved_at(i))
            .map(|h| h.wall)
            .max()
            .unwrap_or(0);
        let lost_found = lost_found_ino();
        for i in losers {
            placed[i].parent = lost_found;
        }
        // Lowest stamp, so a user's own `lost+found` directory wins the name
        // and this one merges into it.
        let version = Version {
            hlc: Hlc::default(),
            base: None,
            size: 0,
            mode: 0o755,
            mtime: wall,
            xattrs: BTreeMap::new(),
            content: Content::Inline(Vec::new()),
        };
        placed.push(Placed {
            ino: lost_found,
            kind: FileKind::Dir,
            parent: ROOT_INO,
            name: LOST_FOUND.to_string(),
            slot: Hlc::ZERO,
            version,
            seed: None,
            base: None,
            conflict: false,
        });
    }

    /// Rebuild the visible tree from the registers.
    pub(super) fn reconcile(&self, t: &mut Tables<'_>) -> FsResult<()> {
        let mut objects: BTreeMap<Ino, Object> = BTreeMap::new();
        for row in t.objects.iter().map_err(db)? {
            let (k, v) = row.map_err(db)?;
            objects.insert(k.value(), dec(v.value())?);
        }

        // Live objects, plus any directory a live object needs to sit in: a
        // write into a deleted directory recreates the path, for every name.
        let mut alive: HashSet<Ino> = objects
            .iter()
            .filter(|(_, o)| o.kind.is_some() && o.alive())
            .map(|(ino, _)| *ino)
            .collect();
        for start in alive.clone() {
            for (_, loc) in objects[&start].placements() {
                let mut parent = loc.1;
                for _ in 0..4096 {
                    if parent == ROOT_INO {
                        break;
                    }
                    let Some(p) = objects
                        .get(&parent)
                        .filter(|p| p.kind == Some(FileKind::Dir))
                    else {
                        break;
                    };
                    let Some((_, up)) = p.home() else { break };
                    alive.insert(parent);
                    parent = up.1;
                }
            }
        }

        let mut placed: Vec<Placed> = Vec::new();
        for (ino, o) in &objects {
            let (Some(kind), Some(v)) = (o.kind, o.winner()) else {
                continue;
            };
            let places = o.placements();
            if !alive.contains(ino) || places.is_empty() {
                continue;
            }
            for (slot, (_, parent, name)) in &places {
                placed.push(Placed {
                    ino: *ino,
                    kind,
                    parent: *parent,
                    name: name.clone(),
                    slot: *slot,
                    version: v.clone(),
                    seed: None,
                    base: None,
                    conflict: false,
                });
            }
            if kind == FileKind::Dir {
                continue;
            }
            // Versions written concurrently with the winner survive as copies,
            // next to the object's first name.
            let (_, (_, primary_parent, primary_name)) = places[0];
            for loser in o.heads.iter().filter(|h| h.hlc != v.hlc) {
                let cino = conflict_ino(*ino, loser.hlc);
                let s = objects.get(&cino);
                if s.is_some_and(Object::unlinked) {
                    continue; // the user resolved it
                }
                let (parent, name) = s.and_then(Object::home).map_or(
                    (*primary_parent, conflict_name(primary_name, loser.hlc)),
                    |(_, l)| (l.1, l.2.clone()),
                );
                placed.push(Placed {
                    ino: cino,
                    kind,
                    parent,
                    name,
                    slot: Hlc::ZERO,
                    version: s.and_then(Object::winner).unwrap_or(loser).clone(),
                    seed: Some(loser.hlc),
                    base: Some((*primary_parent, primary_name.clone())),
                    conflict: true,
                });
            }
        }

        Self::break_cycles(&objects, &mut placed);

        let mut by_parent: HashMap<Ino, Vec<usize>> = HashMap::new();
        for (idx, p) in placed.iter().enumerate() {
            by_parent.entry(p.parent).or_default().push(idx);
        }

        t.dirents.retain(|_, _| false).map_err(db)?;
        t.names.retain(|_, _| false).map_err(db)?;
        t.conflicts.retain(|_, _| false).map_err(db)?;
        t.inodes.retain(|k, _| k == ROOT_INO).map_err(db)?;
        let mut queue = VecDeque::from([ROOT_INO]);
        let mut paths: HashMap<Ino, String> = HashMap::from([(ROOT_INO, String::new())]);
        // Directories that lost a name collision to another directory: their
        // children appear in the winner.
        let mut merged: HashMap<Ino, Vec<Ino>> = HashMap::new();
        // Names shown per file, for its link count.
        let mut links: HashMap<Ino, u32> = HashMap::new();
        // Conflict records by file; a hardlinked file can lose several collisions.
        let mut recs: HashMap<Ino, Vec<ConflictRec>> = HashMap::new();
        // The other names that a shown entry stands for, by index in `placed`.
        let mut also: HashMap<usize, Vec<(Ino, Hlc)>> = HashMap::new();
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
            // (index, name to show, whether it is shown as a conflict, name it collided on)
            let mut shown: Vec<(usize, String, bool, &str)> = Vec::new();
            for (name, mut group) in groups {
                group.sort_by_key(|&i| std::cmp::Reverse(placed[i].version.hlc));
                // Two names of one object under one string are one entry that
                // stands for both.
                let mut first: HashMap<Ino, usize> = HashMap::new();
                group.retain(|&i| match first.get(&placed[i].ino) {
                    Some(&kept) => {
                        also.entry(kept)
                            .or_default()
                            .push((placed[i].ino, placed[i].slot));
                        false
                    }
                    None => {
                        first.insert(placed[i].ino, i);
                        true
                    }
                });
                let winner = group[0];
                // A copy named earlier may have taken this very string: it gives way.
                if let Some(pos) = shown.iter().position(|(_, n, _, _)| n == name) {
                    let mut renamed = format!("{name} 2");
                    while shown.iter().any(|(_, n, _, _)| *n == renamed) {
                        renamed.push_str(" 2");
                    }
                    shown[pos].1 = renamed;
                }
                shown.push((winner, name.to_string(), placed[winner].conflict, name));
                for &loser in &group[1..] {
                    if placed[winner].kind == FileKind::Dir && placed[loser].kind == FileKind::Dir {
                        merged
                            .entry(placed[winner].ino)
                            .or_default()
                            .push(placed[loser].ino);
                        also.entry(winner)
                            .or_default()
                            .push((placed[loser].ino, placed[loser].slot));
                        continue;
                    }
                    let mut copy = conflict_name(name, placed[loser].version.hlc);
                    while shown.iter().any(|(_, n, _, _)| *n == copy) {
                        copy.push_str(" 2");
                    }
                    shown.push((loser, copy, true, name));
                }
            }
            for (idx, name, conflict, collided) in shown {
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
                let slots: Vec<(Ino, Hlc)> = std::iter::once((p.ino, p.slot))
                    .chain(also.get(&idx).into_iter().flatten().copied())
                    .collect();
                t.set_slots(dir, &name, &slots)?;
                if p.kind != FileKind::Dir {
                    *links.entry(p.ino).or_insert(0u32) += 1;
                }
                let path = format!("{}/{name}", paths[&dir]);
                if conflict {
                    let rec = ConflictRec {
                        path: path.clone(),
                        entry: (dir, name.clone()),
                        base: p.base.clone().unwrap_or((dir, collided.to_string())),
                        seed: p.seed,
                    };
                    recs.entry(p.ino).or_default().push(rec);
                }
                if p.kind == FileKind::Dir {
                    paths.insert(p.ino, path);
                    queue.push_back(p.ino);
                }
            }
        }
        for (ino, recs) in recs {
            t.conflicts.insert(ino, enc(&recs).as_slice()).map_err(db)?;
        }
        for (ino, n) in links.into_iter().filter(|&(_, n)| n > 1) {
            let mut inode = t.inode(ino)?;
            inode.nlink = n;
            t.put(ino, &inode)?;
        }
        Ok(())
    }
}
