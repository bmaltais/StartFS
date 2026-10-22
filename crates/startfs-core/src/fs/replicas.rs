//! Where this node's chunks live while the filesystem replicates them: the
//! staging area for chunks written here and not yet on their replica nodes,
//! and the replica store for chunks this node is responsible for.

use super::*;
use crate::protocol::Replica;
use std::collections::HashSet;
use std::sync::atomic::Ordering;

/// A chunk's bytes with their BLAKE3 hash, computed once for everything that
/// writes the chunk.
pub(super) struct Piece<'a> {
    pub id: ChunkId,
    pub data: &'a [u8],
}

impl<'a> Piece<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            id: *blake3::hash(data).as_bytes(),
            data,
        }
    }
}

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

    /// Chunks staged since the last call. Together with one [`Self::staged`]
    /// listing, this follows the staging area without listing it again.
    pub fn newly_staged(&self) -> Vec<ChunkId> {
        self.staging.take_added()
    }

    /// Most bytes the staging area may hold before writes are refused. Only
    /// nodes that offer no space are held to it (see [`Self::set_staging_limit`]
    /// callers): a storage node is limited by its disk alone.
    pub fn set_staging_limit(&self, bytes: u64) {
        self.staging_limit.store(bytes, Ordering::SeqCst);
    }

    /// Refuse a write whose new chunks would overflow the staging area. With
    /// the cap in force (a node offering no space), this and a full local disk
    /// are the only reasons a write fails for lack of space: copies that cannot
    /// be placed elsewhere never stop a write.
    pub(super) fn make_room(&self, pieces: &[Piece<'_>]) -> FsResult<()> {
        if !self.replicating.load(Ordering::SeqCst) {
            return Ok(());
        }
        let mut seen = HashSet::new();
        let needed: u64 = pieces
            .iter()
            .filter(|p| !self.has_replica(&p.id) && !self.has_staged(&p.id) && seen.insert(p.id))
            .map(|p| p.data.len() as u64)
            .sum();
        if self.staging.stats()?.stored_bytes + needed > self.staging_limit.load(Ordering::SeqCst) {
            return Err(FsError::NoSpace);
        }
        Ok(())
    }

    /// Keep a chunk written here: in staging while the node belongs to a
    /// filesystem (replication places it), else straight in the replica store.
    pub(super) fn keep(&self, piece: &Piece<'_>) -> FsResult<ChunkId> {
        if !self.replicating.load(Ordering::SeqCst) {
            self.chunks.put_lazy_hashed(piece.id, piece.data)?;
        } else if !self.has_replica(&piece.id) {
            self.staging.put_lazy_hashed(piece.id, piece.data)?;
        }
        Ok(piece.id)
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
        self.chunks.put_hashed(id, data)?;
        Ok(Replica::Stored)
    }

    /// Turn a staged chunk into a replica held here.
    pub fn promote(&self, id: &ChunkId, offer: u64) -> FsResult<Replica> {
        if self.has_replica(id) {
            return Ok(Replica::Stored);
        }
        let size = self.staging.size_of(id).ok_or(FsError::NotFound)?;
        if self.chunks.stats()?.stored_bytes + size > offer {
            return Ok(Replica::Full);
        }
        // The staged file is the chunk, already hashed and encoded: link it rather than copy it.
        if self.chunks.adopt(&self.staging, id).is_ok() {
            return Ok(Replica::Stored);
        }
        let data = self.staging.get(id)?;
        self.store_replica(&data, offer)
    }

    /// Release a chunk from staging: it is on every node chosen for it. One
    /// this node does not hold as a replica moves to the read cache, since a
    /// file just written is often rewritten, and re-reading its chunks would
    /// otherwise need a peer.
    pub fn release(&self, id: &ChunkId) {
        if !self.has_replica(id)
            && !self.cache.touch(id)
            && let Ok(data) = self.staging.get(id)
        {
            self.cache.put(&data);
        }
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
}
