//! Bounded LRU cache of chunks fetched from peers.
//!
//! It lives apart from replica storage: evicting from it never touches the
//! chunks this node is responsible for, and it does not count toward the
//! filesystem's N copies. Chunks are stored in the same self-describing,
//! BLAKE3-verified format as replicas.

use crate::chunkstore::{ChunkId, ChunkStore};
use crate::ops::FsResult;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CacheStats {
    pub chunks: u64,
    pub used_bytes: u64,
    pub capacity_bytes: u64,
}

#[derive(Default)]
struct Lru {
    clock: u64,
    /// Recency stamp and on-disk size of each cached chunk.
    entries: HashMap<ChunkId, (u64, u64)>,
    /// Oldest first.
    order: BTreeMap<u64, ChunkId>,
    used: u64,
}

impl Lru {
    fn touch(&mut self, id: &ChunkId) {
        self.clock += 1;
        if let Some((stamp, _)) = self.entries.get_mut(id) {
            self.order.remove(stamp);
            *stamp = self.clock;
            self.order.insert(self.clock, *id);
        }
    }

    fn insert(&mut self, id: ChunkId, size: u64) {
        self.clock += 1;
        self.entries.insert(id, (self.clock, size));
        self.order.insert(self.clock, id);
        self.used += size;
    }

    fn remove(&mut self, id: &ChunkId) {
        if let Some((stamp, size)) = self.entries.remove(id) {
            self.order.remove(&stamp);
            self.used -= size;
        }
    }
}

pub struct ChunkCache {
    store: ChunkStore,
    capacity: u64,
    lru: Mutex<Lru>,
}

impl ChunkCache {
    /// Open the cache in `root`, keeping at most `capacity` bytes. Chunks left
    /// by an earlier run stay usable, oldest first.
    pub fn open(root: &Path, capacity: u64) -> FsResult<Self> {
        let store = ChunkStore::open(root)?;
        let mut lru = Lru::default();
        let mut found = store.list()?;
        found.sort_by_key(|(_, _, mtime)| *mtime);
        for (id, size, _) in found {
            lru.insert(id, size);
        }
        let cache = Self {
            store,
            capacity,
            lru: Mutex::new(lru),
        };
        cache.evict(&mut cache.lru.lock().unwrap());
        Ok(cache)
    }

    fn evict(&self, lru: &mut Lru) {
        while lru.used > self.capacity {
            let Some((_, id)) = lru.order.pop_first() else {
                break;
            };
            lru.remove(&id);
            self.store.remove(&id);
        }
    }

    /// A cached chunk, verified; a damaged one is dropped and reported as absent.
    pub fn get(&self, id: &ChunkId) -> Option<Vec<u8>> {
        let mut lru = self.lru.lock().unwrap();
        if !lru.entries.contains_key(id) {
            return None;
        }
        match self.store.get(id) {
            Ok(data) => {
                lru.touch(id);
                Some(data)
            }
            Err(_) => {
                lru.remove(id);
                self.store.remove(id);
                None
            }
        }
    }

    /// Remember a chunk that the caller has already verified.
    pub fn put(&self, data: &[u8]) {
        if data.len() as u64 > self.capacity {
            return;
        }
        let Ok(id) = self.store.put(data) else {
            return;
        };
        let Some(size) = self.store.size_of(&id) else {
            return;
        };
        let mut lru = self.lru.lock().unwrap();
        if lru.entries.contains_key(&id) {
            lru.touch(&id);
        } else {
            lru.insert(id, size);
        }
        self.evict(&mut lru);
    }

    pub fn stats(&self) -> CacheStats {
        let lru = self.lru.lock().unwrap();
        CacheStats {
            chunks: lru.entries.len() as u64,
            used_bytes: lru.used,
            capacity_bytes: self.capacity,
        }
    }
}
