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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
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

    /// Choose whether chunks cached from now on are compressed.
    pub fn set_compression(&self, mode: crate::chunkstore::Compression) {
        self.store.set_compression(mode);
    }

    /// Whether the chunk is cached, counting it as used just now (without reading it).
    pub fn touch(&self, id: &ChunkId) -> bool {
        let mut lru = self.lru.lock().unwrap();
        let held = lru.entries.contains_key(id);
        if held {
            lru.touch(id);
        }
        held
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

/// Default size of the in-memory cache of decoded chunks.
pub const DEFAULT_DECODED_BYTES: u64 = 32 << 20;

#[derive(Default)]
struct Decoded {
    clock: u64,
    /// Recency stamp and bytes of each chunk held.
    entries: HashMap<ChunkId, (u64, Arc<Vec<u8>>)>,
    /// Oldest first.
    order: BTreeMap<u64, ChunkId>,
    used: u64,
}

/// Recently read chunks, decoded and already verified, kept in memory.
///
/// A read of a few KiB would otherwise load, decompress and hash a whole
/// chunk of up to 4 MiB each time. A chunk's id is the hash of its bytes, so
/// an entry can never be stale; the cache only has to stay within its size.
pub struct DecodedCache {
    limit: AtomicU64,
    inner: Mutex<Decoded>,
}

impl DecodedCache {
    pub fn new(limit: u64) -> Self {
        Self {
            limit: AtomicU64::new(limit),
            inner: Mutex::new(Decoded::default()),
        }
    }

    /// Hold at most `bytes`; 0 turns the cache off.
    pub fn set_limit(&self, bytes: u64) {
        self.limit.store(bytes, Ordering::SeqCst);
        self.evict(&mut self.inner.lock().unwrap());
    }

    pub fn get(&self, id: &ChunkId) -> Option<Arc<Vec<u8>>> {
        let mut c = self.inner.lock().unwrap();
        let (stamp, data) = c.entries.get(id).map(|(s, d)| (*s, d.clone()))?;
        c.clock += 1;
        let now = c.clock;
        c.order.remove(&stamp);
        c.order.insert(now, *id);
        if let Some(e) = c.entries.get_mut(id) {
            e.0 = now;
        }
        Some(data)
    }

    /// Remember a chunk the caller has verified. One larger than the whole cache is not kept.
    pub fn put(&self, id: ChunkId, data: Arc<Vec<u8>>) {
        let size = data.len() as u64;
        if size > self.limit.load(Ordering::SeqCst) {
            return;
        }
        let mut c = self.inner.lock().unwrap();
        if c.entries.contains_key(&id) {
            return;
        }
        c.clock += 1;
        let now = c.clock;
        c.entries.insert(id, (now, data));
        c.order.insert(now, id);
        c.used += size;
        self.evict(&mut c);
    }

    fn evict(&self, c: &mut Decoded) {
        let limit = self.limit.load(Ordering::SeqCst);
        while c.used > limit {
            let Some((_, id)) = c.order.pop_first() else {
                break;
            };
            if let Some((_, data)) = c.entries.remove(&id) {
                c.used -= data.len() as u64;
            }
        }
    }
}
