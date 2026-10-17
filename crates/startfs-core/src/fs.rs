//! A single node's filesystem: metadata in redb, data in the chunk store.
//!
//! Metadata records are versioned (first byte) so the format can grow, for
//! example to add an encryption envelope, without a redesign.

use crate::cache::{CacheStats, ChunkCache};
use crate::chunker;
use crate::chunkstore::{ChunkId, ChunkStore};
use crate::clock::Clock;
use crate::net::NodeId;
use crate::oplog::{Entry, Frontier, Hlc, Op, Snapshot};
use crate::ops::*;
use redb::{Database, ReadableTable, Table, TableDefinition};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

/// Files up to this size live inside their metadata record, with no chunk.
pub const INLINE_MAX: usize = 2048;
const NAME_MAX: usize = 255;
const RECORD_VERSION: u8 = 1;

const INODES: TableDefinition<u64, &[u8]> = TableDefinition::new("inodes");
const DIRENTS: TableDefinition<(u64, &str), u64> = TableDefinition::new("dirents");
const COUNTERS: TableDefinition<&str, u64> = TableDefinition::new("counters");
/// The operation log: (origin node, seq) -> encoded [`Entry`].
const LOG: TableDefinition<(u32, u64), &[u8]> = TableDefinition::new("log");
/// Next expected log seq per origin node.
const FRONTIER: TableDefinition<u32, u64> = TableDefinition::new("frontier");
/// Merge registers per object, derived from the log.
const OBJECTS: TableDefinition<u64, &[u8]> = TableDefinition::new("objects");
/// Unresolved conflict copies currently visible in the tree.
const CONFLICTS: TableDefinition<u64, &[u8]> = TableDefinition::new("conflicts");

mod merge;
pub use merge::ConflictCopy;

#[derive(Debug, Clone)]
pub struct NodeConfig {
    /// The member id that stamps this node's log entries and inode numbers,
    /// until `join` assigns a real one.
    pub node: NodeId,
    /// The local user all files appear to belong to on this node.
    pub uid: u32,
    pub gid: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ChunkRef {
    pub id: ChunkId,
    pub len: u32,
}

/// What a file holds. Only the references travel in the operation log:
/// chunks are fetched from a peer when a read needs them.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum Content {
    /// File bytes, or the target of a symlink.
    Inline(Vec<u8>),
    Chunks(Vec<ChunkRef>),
}

#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct Inode {
    kind: FileKind,
    mode: u32,
    nlink: u32,
    size: u64,
    mtime: Nanos,
    ctime: Nanos,
    /// Parent directory (directories only; used to reject rename cycles).
    parent: Ino,
    content: Content,
    xattrs: BTreeMap<String, Vec<u8>>,
}

fn encode(inode: &Inode) -> Vec<u8> {
    let mut out = vec![RECORD_VERSION];
    out.extend(postcard::to_stdvec(inode).expect("inode serialises"));
    out
}

fn decode(raw: &[u8]) -> FsResult<Inode> {
    match raw.split_first() {
        Some((&RECORD_VERSION, rest)) => {
            postcard::from_bytes(rest).map_err(|e| FsError::Io(format!("bad inode record: {e}")))
        }
        _ => Err(FsError::Io("unknown inode record version".into())),
    }
}

fn db<E: std::fmt::Display>(e: E) -> FsError {
    FsError::Io(e.to_string())
}

pub(crate) struct Tables<'a> {
    inodes: Table<'a, u64, &'static [u8]>,
    dirents: Table<'a, (u64, &'static str), u64>,
    counters: Table<'a, &'static str, u64>,
    log: Table<'a, (u32, u64), &'static [u8]>,
    frontier: Table<'a, u32, u64>,
    objects: Table<'a, u64, &'static [u8]>,
    conflicts: Table<'a, u64, &'static [u8]>,
}

impl Tables<'_> {
    fn inode(&self, ino: Ino) -> FsResult<Inode> {
        let g = self.inodes.get(ino).map_err(db)?.ok_or(FsError::NotFound)?;
        decode(g.value())
    }
    fn put(&mut self, ino: Ino, inode: &Inode) -> FsResult<()> {
        self.inodes
            .insert(ino, encode(inode).as_slice())
            .map_err(db)?;
        Ok(())
    }
    fn dir(&self, ino: Ino) -> FsResult<Inode> {
        let i = self.inode(ino)?;
        if i.kind == FileKind::Dir {
            Ok(i)
        } else {
            Err(FsError::NotDir)
        }
    }
    fn entry(&self, parent: Ino, name: &str) -> FsResult<Option<Ino>> {
        Ok(self
            .dirents
            .get((parent, name))
            .map_err(db)?
            .map(|g| g.value()))
    }
    fn is_empty_dir(&self, ino: Ino) -> FsResult<bool> {
        Ok(self
            .dirents
            .range((ino, "")..(ino + 1, ""))
            .map_err(db)?
            .next()
            .is_none())
    }
    /// Inode numbers are global: the allocating node's tag above a local counter.
    fn alloc_ino(&mut self, node: NodeId) -> FsResult<Ino> {
        let next = self
            .counters
            .get("next_ino")
            .map_err(db)?
            .map(|g| g.value())
            .unwrap_or(1);
        self.counters.insert("next_ino", next + 1).map_err(db)?;
        Ok((u64::from(node.0 + 1) << 40) | next)
    }
}

/// Where chunks this node lacks can be fetched from.
pub trait ChunkSource: Send + Sync {
    /// Ask peers for a chunk. `accept` says whether received bytes are the
    /// right chunk; a source tries another peer when it says no.
    fn fetch(&self, id: &ChunkId, accept: &dyn Fn(&[u8]) -> bool) -> Option<Vec<u8>>;
}

/// Chunks fetched during one operation, and the one it is waiting for.
#[derive(Default)]
struct Spill {
    got: HashMap<ChunkId, Vec<u8>>,
    miss: Option<ChunkId>,
}

pub struct LocalFs {
    db: Database,
    /// Replica storage: chunks this node is responsible for.
    chunks: ChunkStore,
    /// Chunks fetched on demand; bounded, and apart from replica storage.
    cache: ChunkCache,
    source: Mutex<Option<Arc<dyn ChunkSource>>>,
    clock: Arc<dyn Clock>,
    config: NodeConfig,
    /// This node's member id; changes once, when it joins a filesystem.
    node: AtomicU32,
    /// Serialises operations on this node.
    lock: Mutex<()>,
    /// Latest hybrid logical clock value issued or seen by this node.
    hlc: Mutex<Hlc>,
}

fn check_name(name: &str) -> FsResult<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        Err(FsError::Invalid)
    } else if name.len() > NAME_MAX {
        Err(FsError::NameTooLong)
    } else {
        Ok(())
    }
}

fn is_acl(name: &str) -> bool {
    name.starts_with("system.posix_acl_")
}

impl LocalFs {
    pub fn open(
        dir: &Path,
        config: NodeConfig,
        cache_bytes: u64,
        clock: Arc<dyn Clock>,
    ) -> FsResult<Self> {
        std::fs::create_dir_all(dir).map_err(db)?;
        let database = Database::create(dir.join("meta.redb")).map_err(db)?;
        let fs = Self {
            db: database,
            chunks: ChunkStore::open(&dir.join("chunks"))?,
            cache: ChunkCache::open(&dir.join("cache"), cache_bytes)?,
            source: Mutex::new(None),
            clock,
            node: AtomicU32::new(config.node.0),
            config,
            lock: Mutex::new(()),
            hlc: Mutex::new(Hlc::ZERO),
        };
        fs.run(true, |t| {
            let wall = t
                .counters
                .get("hlc_wall")
                .map_err(db)?
                .map_or(0, |g| g.value());
            let counter = t
                .counters
                .get("hlc_counter")
                .map_err(db)?
                .map_or(0, |g| g.value());
            *fs.hlc.lock().unwrap() = Hlc {
                wall,
                counter: counter as u32,
                node: fs.node().0,
            };
            if t.inodes.get(ROOT_INO).map_err(db)?.is_none() {
                let now = fs.clock.now();
                let root = Inode {
                    kind: FileKind::Dir,
                    mode: 0o755,
                    nlink: 2,
                    size: 0,
                    mtime: now,
                    ctime: now,
                    parent: ROOT_INO,
                    content: Content::Inline(Vec::new()),
                    xattrs: BTreeMap::new(),
                };
                t.put(ROOT_INO, &root)?;
            }
            Ok(())
        })?;
        Ok(fs)
    }

    pub fn node(&self) -> NodeId {
        NodeId(self.node.load(Ordering::SeqCst))
    }

    /// Take the member id assigned at join. Only valid before the node has
    /// written anything, since ids stamp log entries and inode numbers.
    pub fn set_node(&self, node: NodeId) {
        self.node.store(node.0, Ordering::SeqCst);
        self.hlc.lock().unwrap().node = node.0;
    }

    /// Run `f` in one transaction; read-only runs are rolled back.
    fn run<T>(&self, write: bool, f: impl FnOnce(&mut Tables<'_>) -> FsResult<T>) -> FsResult<T> {
        let _guard = self.lock.lock().unwrap();
        let txn = self.db.begin_write().map_err(db)?;
        let out = {
            let mut t = Tables {
                inodes: txn.open_table(INODES).map_err(db)?,
                dirents: txn.open_table(DIRENTS).map_err(db)?,
                counters: txn.open_table(COUNTERS).map_err(db)?,
                log: txn.open_table(LOG).map_err(db)?,
                frontier: txn.open_table(FRONTIER).map_err(db)?,
                objects: txn.open_table(OBJECTS).map_err(db)?,
                conflicts: txn.open_table(CONFLICTS).map_err(db)?,
            };
            f(&mut t)
        };
        match out {
            Ok(v) if write => {
                txn.commit().map_err(db)?;
                Ok(v)
            }
            other => {
                txn.abort().map_err(db)?;
                other
            }
        }
    }

    fn attr(&self, ino: Ino, i: &Inode) -> Attr {
        Attr {
            ino,
            kind: i.kind,
            mode: i.mode,
            nlink: i.nlink,
            uid: self.config.uid,
            gid: self.config.gid,
            size: i.size,
            mtime: i.mtime,
            ctime: i.ctime,
        }
    }

    fn new_inode(
        &self,
        kind: FileKind,
        mode: u32,
        parent: Ino,
        content: Content,
        size: u64,
    ) -> Inode {
        let now = self.clock.now();
        Inode {
            kind,
            mode: mode & 0o7777,
            nlink: if kind == FileKind::Dir { 2 } else { 1 },
            size,
            mtime: now,
            ctime: now,
            parent,
            content,
            xattrs: BTreeMap::new(),
        }
    }

    /// Add a new entry under `parent`.
    fn add(&self, t: &mut Tables<'_>, parent: Ino, name: &str, inode: Inode) -> FsResult<Attr> {
        check_name(name)?;
        let mut p = t.dir(parent)?;
        if t.entry(parent, name)?.is_some() {
            return Err(FsError::Exists);
        }
        let ino = t.alloc_ino(self.node())?;
        t.put(ino, &inode)?;
        t.dirents.insert((parent, name), ino).map_err(db)?;
        p.mtime = self.clock.now();
        p.ctime = p.mtime;
        t.put(parent, &p)?;
        let snap = self.snapshot(&inode);
        self.emit(
            t,
            Op::Create {
                ino,
                kind: inode.kind,
                parent,
                name: name.to_string(),
                snap,
            },
        )?;
        Ok(self.attr(ino, &inode))
    }

    fn touch_dir(&self, t: &mut Tables<'_>, dir: Ino) -> FsResult<()> {
        let mut p = t.inode(dir)?;
        p.mtime = self.clock.now();
        p.ctime = p.mtime;
        t.put(dir, &p)
    }

    /// Where to fetch chunks this node does not hold.
    pub fn set_chunk_source(&self, source: Arc<dyn ChunkSource>) {
        *self.source.lock().unwrap() = Some(source);
    }

    /// A chunk held here, replica or cached, verified against its hash.
    /// This is what a peer's fetch is answered from.
    pub fn local_chunk(&self, id: &ChunkId) -> Option<Vec<u8>> {
        self.chunks.get(id).ok().or_else(|| self.cache.get(id))
    }

    pub fn cache_stats(&self) -> CacheStats {
        self.cache.stats()
    }

    fn chunk(&self, id: &ChunkId, spill: &mut Spill) -> FsResult<Vec<u8>> {
        if let Some(data) = self.local_chunk(id).or_else(|| spill.got.get(id).cloned()) {
            return Ok(data);
        }
        spill.miss = Some(*id);
        Err(FsError::Unavailable)
    }

    /// Fetch a chunk from a peer, never accepting bytes that fail their hash.
    fn fetch_remote(&self, id: &ChunkId) -> FsResult<Vec<u8>> {
        let source = self.source.lock().unwrap().clone();
        let is_chunk = |d: &[u8]| blake3::hash(d).as_bytes() == id;
        match source.and_then(|s| s.fetch(id, &is_chunk)) {
            Some(data) if is_chunk(&data) => {
                self.cache.put(&data);
                Ok(data)
            }
            // A damaged local copy that no peer could replace is an I/O error.
            _ if self.chunks.size_of(id).is_some() => Err(FsError::Io("chunk is corrupt".into())),
            _ => Err(FsError::Unavailable),
        }
    }

    /// Run `f`, fetching from peers (outside any lock) each chunk it finds
    /// missing and then running it again.
    fn fetching<T>(
        &self,
        write: bool,
        mut f: impl FnMut(&mut Tables<'_>, &mut Spill) -> FsResult<T>,
    ) -> FsResult<T> {
        let mut spill = Spill::default();
        loop {
            spill.miss = None;
            match self.run(write, |t| f(t, &mut spill)) {
                Err(FsError::Unavailable) if spill.miss.is_some() => {
                    let id = spill.miss.take().expect("checked");
                    let data = self.fetch_remote(&id)?;
                    spill.got.insert(id, data);
                }
                other => return other,
            }
        }
    }

    fn read_all(&self, i: &Inode, spill: &mut Spill) -> FsResult<Vec<u8>> {
        match &i.content {
            Content::Inline(b) => Ok(b.clone()),
            Content::Chunks(refs) => {
                let mut out = Vec::with_capacity(i.size as usize);
                for r in refs {
                    out.extend(self.chunk(&r.id, spill)?);
                }
                Ok(out)
            }
        }
    }

    /// Replace a file's content, chunking, deduplicating and compressing it.
    fn store(&self, i: &mut Inode, data: &[u8]) -> FsResult<()> {
        i.content = if data.len() <= INLINE_MAX {
            Content::Inline(data.to_vec())
        } else {
            let mut refs = Vec::new();
            for c in chunker::split(data) {
                refs.push(ChunkRef {
                    id: self.chunks.put(c)?,
                    len: c.len() as u32,
                });
            }
            Content::Chunks(refs)
        };
        i.size = data.len() as u64;
        Ok(())
    }

    fn file(t: &Tables<'_>, ino: Ino) -> FsResult<Inode> {
        let i = t.inode(ino)?;
        match i.kind {
            FileKind::File => Ok(i),
            FileKind::Dir => Err(FsError::IsDir),
            FileKind::Symlink => Err(FsError::Invalid),
        }
    }

    /// Remove an entry and record its deletion in the log.
    fn remove_entry(&self, t: &mut Tables<'_>, parent: Ino, name: &str, ino: Ino) -> FsResult<()> {
        t.dirents.remove((parent, name)).map_err(db)?;
        t.inodes.remove(ino).map_err(db)?;
        t.conflicts.remove(ino).map_err(db)?;
        self.emit(t, Op::Delete { ino })
    }
}

impl FsOps for LocalFs {
    fn lookup(&self, parent: Ino, name: &str) -> FsResult<Attr> {
        self.run(false, |t| {
            let p = t.dir(parent)?;
            let ino = match name {
                "." => parent,
                ".." => p.parent,
                _ => t.entry(parent, name)?.ok_or(FsError::NotFound)?,
            };
            Ok(self.attr(ino, &t.inode(ino)?))
        })
    }

    fn getattr(&self, ino: Ino) -> FsResult<Attr> {
        self.run(false, |t| Ok(self.attr(ino, &t.inode(ino)?)))
    }

    fn setattr(&self, ino: Ino, set: SetAttr) -> FsResult<Attr> {
        self.fetching(true, |t, spill| {
            let mut i = t.inode(ino)?;
            let now = self.clock.now();
            if let Some(size) = set.size {
                let mut i2 = Self::file(t, ino)?;
                let mut data = self.read_all(&i2, spill)?;
                data.resize(size as usize, 0);
                self.store(&mut i2, &data)?;
                i2.mtime = now;
                i = i2;
            }
            if let Some(mode) = set.mode {
                i.mode = mode & 0o7777;
            }
            if let Some(mtime) = set.mtime {
                i.mtime = mtime;
            }
            // uid/gid are accepted but ownership stays mapped to the local user.
            i.ctime = now;
            t.put(ino, &i)?;
            self.emit_set(t, ino, &i)?;
            Ok(self.attr(ino, &i))
        })
    }

    fn mkdir(&self, parent: Ino, name: &str, mode: u32) -> FsResult<Attr> {
        self.run(true, |t| {
            let i = self.new_inode(FileKind::Dir, mode, parent, Content::Inline(Vec::new()), 0);
            self.add(t, parent, name, i)
        })
    }

    fn create(&self, parent: Ino, name: &str, mode: u32) -> FsResult<Attr> {
        self.run(true, |t| {
            let i = self.new_inode(FileKind::File, mode, parent, Content::Inline(Vec::new()), 0);
            self.add(t, parent, name, i)
        })
    }

    fn symlink(&self, parent: Ino, name: &str, target: &str) -> FsResult<Attr> {
        self.run(true, |t| {
            let content = Content::Inline(target.as_bytes().to_vec());
            let i = self.new_inode(
                FileKind::Symlink,
                0o777,
                parent,
                content,
                target.len() as u64,
            );
            self.add(t, parent, name, i)
        })
    }

    fn readlink(&self, ino: Ino) -> FsResult<String> {
        self.run(false, |t| {
            let i = t.inode(ino)?;
            match (i.kind, i.content) {
                (FileKind::Symlink, Content::Inline(b)) => String::from_utf8(b)
                    .map_err(|_| FsError::Io("symlink target is not utf-8".into())),
                _ => Err(FsError::Invalid),
            }
        })
    }

    fn link(&self, _ino: Ino, _new_parent: Ino, _new_name: &str) -> FsResult<Attr> {
        // POSIX filesystems without hardlink support report EPERM.
        Err(FsError::NotPermitted)
    }

    fn read(&self, ino: Ino, offset: u64, len: u32) -> FsResult<Vec<u8>> {
        self.fetching(false, |t, spill| {
            let i = Self::file(t, ino)?;
            let end = offset.saturating_add(len as u64).min(i.size);
            if offset >= end {
                return Ok(Vec::new());
            }
            match &i.content {
                Content::Inline(b) => Ok(b[offset as usize..end as usize].to_vec()),
                Content::Chunks(refs) => {
                    let mut out = Vec::with_capacity((end - offset) as usize);
                    let mut pos = 0u64;
                    for r in refs {
                        let (start, stop) = (pos, pos + r.len as u64);
                        pos = stop;
                        if stop <= offset {
                            continue;
                        }
                        if start >= end {
                            break;
                        }
                        let data = self.chunk(&r.id, spill)?;
                        let from = offset.saturating_sub(start) as usize;
                        let to = (end.min(stop) - start) as usize;
                        out.extend_from_slice(&data[from..to]);
                    }
                    Ok(out)
                }
            }
        })
    }

    fn write(&self, ino: Ino, offset: u64, data: &[u8]) -> FsResult<usize> {
        self.fetching(true, |t, spill| {
            let mut i = Self::file(t, ino)?;
            let mut buf = self.read_all(&i, spill)?;
            let end = offset as usize + data.len();
            if buf.len() < end {
                buf.resize(end, 0);
            }
            buf[offset as usize..end].copy_from_slice(data);
            self.store(&mut i, &buf)?;
            i.mtime = self.clock.now();
            i.ctime = i.mtime;
            t.put(ino, &i)?;
            self.emit_set(t, ino, &i)?;
            Ok(data.len())
        })
    }

    fn unlink(&self, parent: Ino, name: &str) -> FsResult<()> {
        self.run(true, |t| {
            t.dir(parent)?;
            let ino = t.entry(parent, name)?.ok_or(FsError::NotFound)?;
            if t.inode(ino)?.kind == FileKind::Dir {
                return Err(FsError::IsDir);
            }
            self.remove_entry(t, parent, name, ino)?;
            self.touch_dir(t, parent)
        })
    }

    fn rmdir(&self, parent: Ino, name: &str) -> FsResult<()> {
        self.run(true, |t| {
            t.dir(parent)?;
            let ino = t.entry(parent, name)?.ok_or(FsError::NotFound)?;
            t.dir(ino)?;
            if !t.is_empty_dir(ino)? {
                return Err(FsError::NotEmpty);
            }
            self.remove_entry(t, parent, name, ino)?;
            self.touch_dir(t, parent)
        })
    }

    fn rename(&self, parent: Ino, name: &str, new_parent: Ino, new_name: &str) -> FsResult<()> {
        self.run(true, |t| {
            check_name(new_name)?;
            t.dir(parent)?;
            t.dir(new_parent)?;
            let src = t.entry(parent, name)?.ok_or(FsError::NotFound)?;
            let mut src_inode = t.inode(src)?;
            let is_dir = src_inode.kind == FileKind::Dir;
            if is_dir {
                // A directory cannot move beneath itself.
                let mut cur = new_parent;
                loop {
                    if cur == src {
                        return Err(FsError::Invalid);
                    }
                    if cur == ROOT_INO {
                        break;
                    }
                    cur = t.inode(cur)?.parent;
                }
            }
            if let Some(dst) = t.entry(new_parent, new_name)? {
                if dst == src {
                    return Ok(());
                }
                let dst_is_dir = t.inode(dst)?.kind == FileKind::Dir;
                match (is_dir, dst_is_dir) {
                    (true, false) => return Err(FsError::NotDir),
                    (false, true) => return Err(FsError::IsDir),
                    (true, true) if !t.is_empty_dir(dst)? => return Err(FsError::NotEmpty),
                    _ => {}
                }
                self.remove_entry(t, new_parent, new_name, dst)?;
            }
            t.dirents.remove((parent, name)).map_err(db)?;
            t.dirents.insert((new_parent, new_name), src).map_err(db)?;
            src_inode.parent = new_parent;
            src_inode.ctime = self.clock.now();
            t.put(src, &src_inode)?;
            self.emit(
                t,
                Op::Rename {
                    ino: src,
                    parent: new_parent,
                    name: new_name.to_string(),
                },
            )?;
            self.touch_dir(t, parent)?;
            self.touch_dir(t, new_parent)
        })
    }

    fn readdir(&self, ino: Ino) -> FsResult<Vec<DirEntry>> {
        self.run(false, |t| {
            t.dir(ino)?;
            let mut out = Vec::new();
            for row in t.dirents.range((ino, "")..(ino + 1, "")).map_err(db)? {
                let (k, v) = row.map_err(db)?;
                let child = v.value();
                out.push(DirEntry {
                    name: k.value().1.to_string(),
                    ino: child,
                    kind: t.inode(child)?.kind,
                });
            }
            Ok(out)
        })
    }

    fn setxattr(&self, ino: Ino, name: &str, value: &[u8]) -> FsResult<()> {
        if is_acl(name) {
            return Err(FsError::NotSupported);
        }
        if name.is_empty() {
            return Err(FsError::Invalid);
        }
        self.run(true, |t| {
            let mut i = t.inode(ino)?;
            i.xattrs.insert(name.to_string(), value.to_vec());
            i.ctime = self.clock.now();
            t.put(ino, &i)?;
            self.emit_set(t, ino, &i)
        })
    }

    fn getxattr(&self, ino: Ino, name: &str) -> FsResult<Vec<u8>> {
        if is_acl(name) {
            return Err(FsError::NotSupported);
        }
        self.run(false, |t| {
            t.inode(ino)?
                .xattrs
                .get(name)
                .cloned()
                .ok_or(FsError::NoData)
        })
    }

    fn listxattr(&self, ino: Ino) -> FsResult<Vec<String>> {
        self.run(false, |t| Ok(t.inode(ino)?.xattrs.into_keys().collect()))
    }

    fn removexattr(&self, ino: Ino, name: &str) -> FsResult<()> {
        if is_acl(name) {
            return Err(FsError::NotSupported);
        }
        self.run(true, |t| {
            let mut i = t.inode(ino)?;
            i.xattrs.remove(name).ok_or(FsError::NoData)?;
            i.ctime = self.clock.now();
            t.put(ino, &i)?;
            self.emit_set(t, ino, &i)
        })
    }

    fn stats(&self) -> FsResult<StoreStats> {
        self.chunks.stats()
    }
}
