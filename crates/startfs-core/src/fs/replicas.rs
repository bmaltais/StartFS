//! Where this node's chunks live while the filesystem replicates them: the
//! staging area for chunks written here and not yet on their replica nodes,
//! and the replica store for chunks this node is responsible for.

use super::*;
use crate::protocol::Replica;
use std::collections::HashSet;
use std::sync::atomic::Ordering;

impl LocalFs {
    /// From now on new chunks are staged and replicated by placement, rather
    /// than kept in the replica store. Set once the node belongs to a filesystem.
    pub fn set_replicating(&self) {
        self.replicating.store(true, Ordering::SeqCst);
    }

    /// Chunks written here that have not been released from staging.
    pub fn staged(&self) -> FsResult<Vec<ChunkId>> {
        Ok(self.staging.list()?.into_iter().map(|c| c.0).collect())
    }

    /// Most bytes the staging area may hold before writes are refused.
    pub fn set_staging_limit(&self, bytes: u64) {
        self.staging_limit.store(bytes, Ordering::SeqCst);
    }

    /// Refuse a write whose new chunks would overflow the staging area. This
    /// is the only reason a write fails for lack of space: copies that cannot
    /// be placed elsewhere never stop a write.
    pub(super) fn make_room(&self, pieces: &[&[u8]]) -> FsResult<()> {
        if !self.replicating.load(Ordering::SeqCst) {
            return Ok(());
        }
        let mut seen = HashSet::new();
        let needed: u64 = pieces
            .iter()
            .filter(|c| {
                let id = *blake3::hash(c).as_bytes();
                !self.has_replica(&id) && !self.has_staged(&id) && seen.insert(id)
            })
            .map(|c| c.len() as u64)
            .sum();
        if self.staging.stats()?.stored_bytes + needed > self.staging_limit.load(Ordering::SeqCst) {
            return Err(FsError::NoSpace);
        }
        Ok(())
    }

    /// Keep a chunk written here: in staging while the node belongs to a
    /// filesystem (replication places it), else straight in the replica store.
    pub(super) fn keep(&self, data: &[u8]) -> FsResult<ChunkId> {
        let id = *blake3::hash(data).as_bytes();
        if !self.replicating.load(Ordering::SeqCst) {
            self.chunks.put(data)?;
        } else if !self.has_replica(&id) {
            self.staging.put(data)?;
        }
        Ok(id)
    }

    /// Chunks this node holds as replicas.
    pub fn held(&self) -> FsResult<Vec<ChunkId>> {
        Ok(self.chunks.list()?.into_iter().map(|c| c.0).collect())
    }

    pub fn has_replica(&self, id: &ChunkId) -> bool {
        self.chunks.size_of(id).is_some()
    }

    pub fn has_staged(&self, id: &ChunkId) -> bool {
        self.staging.size_of(id).is_some()
    }

    /// Whether a replica of `id` is held, or room remains to take one.
    pub fn replica_standing(&self, id: &ChunkId, offer: u64) -> Replica {
        if self.has_replica(id) {
            Replica::Stored
        } else if self
            .chunks
            .stats()
            .map_or(true, |s| s.stored_bytes >= offer)
        {
            Replica::Full
        } else {
            Replica::Missing
        }
    }

    /// Take a replica of a chunk pushed by a peer, if it is genuine and fits
    /// within `offer` bytes of replica storage.
    pub fn store_replica(&self, data: &[u8], offer: u64) -> FsResult<Replica> {
        let id = *blake3::hash(data).as_bytes();
        if self.has_replica(&id) {
            return Ok(Replica::Stored);
        }
        let used = self.chunks.stats()?.stored_bytes;
        if used + data.len() as u64 > offer {
            return Ok(Replica::Full);
        }
        self.chunks.put(data)?;
        Ok(Replica::Stored)
    }

    /// Turn a staged chunk into a replica held here.
    pub fn promote(&self, id: &ChunkId, offer: u64) -> FsResult<Replica> {
        let data = self.staging.get(id)?;
        self.store_replica(&data, offer)
    }

    /// Release a chunk from staging: it is on every node chosen for it.
    pub fn release(&self, id: &ChunkId) {
        self.staging.remove(id);
    }

    /// Stop holding a replica placement no longer asks of this node.
    pub fn drop_replica(&self, id: &ChunkId) {
        self.chunks.remove(id);
    }

    /// What the staging area holds.
    pub fn staging_stats(&self) -> FsResult<StoreStats> {
        self.staging.stats()
    }

    /// Files (path and chunks) that use any of the given chunks.
    pub fn files_using(&self, wanted: &HashSet<ChunkId>) -> FsResult<Vec<(String, Vec<ChunkId>)>> {
        self.run(false, |t| {
            let mut kids: HashMap<Ino, Vec<(String, Ino)>> = HashMap::new();
            for row in t.dirents.iter().map_err(db)? {
                let (k, v) = row.map_err(db)?;
                let (parent, name) = k.value();
                kids.entry(parent)
                    .or_default()
                    .push((name.to_string(), v.value()));
            }
            let mut out = Vec::new();
            let mut stack = vec![(ROOT_INO, String::new())];
            while let Some((dir, path)) = stack.pop() {
                for (name, ino) in kids.remove(&dir).unwrap_or_default() {
                    let child = format!("{path}/{name}");
                    let inode = t.inode(ino)?;
                    match inode.content {
                        _ if inode.kind == FileKind::Dir => stack.push((ino, child)),
                        Content::Chunks(refs) => {
                            let ids: Vec<ChunkId> = refs.into_iter().map(|r| r.id).collect();
                            if ids.iter().any(|id| wanted.contains(id)) {
                                out.push((child, ids));
                            }
                        }
                        Content::Inline(_) => {}
                    }
                }
            }
            out.sort();
            Ok(out)
        })
    }
}
