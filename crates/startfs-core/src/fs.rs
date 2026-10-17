//! A single node's filesystem: metadata in redb, data in the chunk store.
//!
//! Metadata records are versioned (first byte) so the format can grow, for
//! example to add an encryption envelope, without a redesign.

use crate::chunker;
use crate::chunkstore::{ChunkId, ChunkStore};
use crate::clock::Clock;
use crate::net::NodeId;
use crate::oplog::{Entry, Frontier, Hlc, Op, Snapshot};
use crate::ops::*;
use redb::{Database, ReadableTable, Table, TableDefinition};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
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
    /// This node's identity in the filesystem; stamps its log entries and inode numbers.
    pub node: NodeId,
    /// The local user all files appear to belong to on this node.
    pub uid: u32,
    pub gid: u32,
}

#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct ChunkRef {
    id: ChunkId,
    len: u32,
}

#[derive(Serialize, Deserialize, Clone)]
pub(crate) enum Content {
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

pub struct LocalFs {
    db: Database,
    chunks: ChunkStore,
    clock: Arc<dyn Clock>,
    config: NodeConfig,
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
    pub fn open(dir: &Path, config: NodeConfig, clock: Arc<dyn Clock>) -> FsResult<Self> {
        std::fs::create_dir_all(dir).map_err(db)?;
        let database = Database::create(dir.join("meta.redb")).map_err(db)?;
        let fs = Self {
            db: database,
            chunks: ChunkStore::open(&dir.join("chunks"))?,
            clock,
            config,
            lock: Mutex::new(()),
            hlc: Mutex::new(Hlc::ZERO),
        };
        fs.run(true, |t| {
            let wall = t.counters.get("hlc_wall").map_err(db)?.map_or(0, |g| g.value());
            let counter = t.counters.get("hlc_counter").map_err(db)?.map_or(0, |g| g.value());
            *fs.hlc.lock().unwrap() = Hlc {
                wall,
                counter: counter as u32,
                node: fs.config.node.0,
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
        let ino = t.alloc_ino(self.config.node)?;
        t.put(ino, &inode)?;
        t.dirents.insert((parent, name), ino).map_err(db)?;
        p.mtime = self.clock.now();
        p.ctime = p.mtime;
        t.put(parent, &p)?;
        let snap = self.snapshot(&inode)?;
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

    fn read_all(&self, i: &Inode) -> FsResult<Vec<u8>> {
        match &i.content {
            Content::Inline(b) => Ok(b.clone()),
            Content::Chunks(refs) => {
                let mut out = Vec::with_capacity(i.size as usize);
                for r in refs {
                    out.extend(self.chunks.get(&r.id)?);
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
        self.run(true, |t| {
            let mut i = t.inode(ino)?;
            let now = self.clock.now();
            if let Some(size) = set.size {
                let mut i2 = Self::file(t, ino)?;
                let mut data = self.read_all(&i2)?;
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
        self.run(false, |t| {
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
                        let data = self.chunks.get(&r.id)?;
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
        self.run(true, |t| {
            let mut i = Self::file(t, ino)?;
            let mut buf = self.read_all(&i)?;
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
            t.put(ino, &i)
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
            t.put(ino, &i)
        })
    }

    fn stats(&self) -> FsResult<StoreStats> {
        self.chunks.stats()
    }
}
