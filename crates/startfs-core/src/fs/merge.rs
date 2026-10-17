//! Merging operation logs into the visible tree.
//!
//! Every log entry is folded into per-object registers that do not depend on
//! arrival order; the visible tree is then rebuilt from those registers, so
//! all nodes holding the same entries show the same tree.

use super::*;
use std::collections::{BTreeMap, HashMap, VecDeque};

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

/// An unresolved conflict copy, as shown by `status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictCopy {
    /// Where the copy lives in the filesystem.
    pub path: String,
}

fn enc<T: Serialize>(v: &T) -> Vec<u8> {
    postcard::to_stdvec(v).expect("serialises")
}

fn dec<'a, T: Deserialize<'a>>(raw: &'a [u8]) -> FsResult<T> {
    postcard::from_bytes(raw).map_err(|e| FsError::Io(format!("bad record: {e}")))
}

impl Tables<'_> {
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
    fn next_seq(&self, origin: u32) -> FsResult<u64> {
        Ok(self
            .frontier
            .get(origin)
            .map_err(db)?
            .map_or(0, |g| g.value()))
    }
}

impl LocalFs {
    /// The bytes and attributes of an inode as a log snapshot.
    pub(super) fn snapshot(&self, i: &Inode) -> FsResult<Snapshot> {
        Ok(Snapshot {
            mode: i.mode,
            mtime: i.mtime,
            xattrs: i.xattrs.clone(),
            data: self.read_all(i)?,
        })
    }

    /// Record a new state of `ino` (already applied to the visible tree).
    pub(super) fn emit_set(&self, t: &mut Tables<'_>, ino: Ino, i: &Inode) -> FsResult<()> {
        let base = t.object(ino)?.winner().map(|v| v.hlc);
        let snap = self.snapshot(i)?;
        self.emit(t, Op::Set { ino, base, snap })
    }

    /// Append a local operation to this node's log.
    pub(super) fn emit(&self, t: &mut Tables<'_>, op: Op) -> FsResult<()> {
        let origin = self.config.node.0;
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

    fn version(&self, hlc: Hlc, base: Option<Hlc>, snap: &Snapshot) -> FsResult<Version> {
        let mut i = Inode {
            kind: FileKind::File,
            mode: snap.mode,
            nlink: 1,
            size: 0,
            mtime: snap.mtime,
            ctime: snap.mtime,
            parent: 0,
            content: Content::Inline(Vec::new()),
            xattrs: BTreeMap::new(),
        };
        self.store(&mut i, &snap.data)?;
        Ok(Version {
            hlc,
            base,
            size: i.size,
            mode: snap.mode,
            mtime: snap.mtime,
            xattrs: snap.xattrs.clone(),
            content: i.content,
        })
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
                o.add_version(self.version(e.hlc, None, snap)?);
                t.put_object(*ino, &o)
            }
            Op::Set { ino, base, snap } => {
                let mut o = t.object(*ino)?;
                o.max_live = o.max_live.max(e.hlc);
                o.add_version(self.version(e.hlc, *base, snap)?);
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
    pub fn entries_since(&self, have: &Frontier, limit: usize) -> FsResult<Vec<Entry>> {
        self.run(false, |t| {
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
        })
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
        }
        let mut placed: Vec<Placed> = Vec::new();
        for (ino, o) in &objects {
            let (Some(kind), Some((_, parent, name)), Some(v)) = (o.kind, &o.loc, o.winner())
            else {
                continue;
            };
            if o.alive() {
                placed.push(Placed {
                    ino: *ino,
                    kind,
                    parent: *parent,
                    name: name.clone(),
                    version: v.clone(),
                });
            }
        }

        let mut by_parent: HashMap<Ino, Vec<usize>> = HashMap::new();
        for (idx, p) in placed.iter().enumerate() {
            by_parent.entry(p.parent).or_default().push(idx);
        }

        t.dirents.retain(|_, _| false).map_err(db)?;
        t.inodes.retain(|k, _| k == ROOT_INO).map_err(db)?;
        let mut queue = VecDeque::from([ROOT_INO]);
        while let Some(dir) = queue.pop_front() {
            for &idx in by_parent.get(&dir).into_iter().flatten() {
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
                t.dirents.insert((dir, p.name.as_str()), p.ino).map_err(db)?;
                if p.kind == FileKind::Dir {
                    queue.push_back(p.ino);
                }
            }
        }
        Ok(())
    }
}
