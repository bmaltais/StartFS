//! Garbage collection of chunks no file references any more.
//!
//! Deleting or overwriting a file only changes metadata. A chunk that nothing
//! in the visible tree references is an orphan; it is removed once it has been
//! an orphan for the retention period. There is no reference counting: each
//! pass looks at what the tree references right now.

use super::*;
use std::collections::HashSet;
use std::time::Duration;

/// What a collection pass found and did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GcReport {
    pub reclaimed_chunks: u64,
    pub reclaimed_bytes: u64,
    /// Orphans still waiting out the retention period.
    pub pending_chunks: u64,
    pub pending_bytes: u64,
}

impl LocalFs {
    /// Remove chunks (replicas and staged) that have been unreferenced for
    /// at least `retention`, as observed by this node.
    pub fn collect_garbage(&self, retention: Duration) -> FsResult<GcReport> {
        let now = self.clock.now();
        let retention = retention.as_nanos() as u64;
        self.run(true, |t| {
            let mut referenced: HashSet<ChunkId> = HashSet::new();
            for row in t.inodes.iter().map_err(db)? {
                if let Content::Chunks(refs) = decode(row.map_err(db)?.1.value())?.content {
                    referenced.extend(refs.into_iter().map(|r| r.id));
                }
            }
            let mut present: HashMap<ChunkId, u64> = HashMap::new();
            for (id, size, _) in self.chunks.list()?.into_iter().chain(self.staging.list()?) {
                *present.entry(id).or_default() += size;
            }
            let mut report = GcReport::default();
            let known: Vec<(Vec<u8>, u64)> = t
                .orphans
                .iter()
                .map_err(db)?
                .map(|r| r.map(|(k, v)| (k.value().to_vec(), v.value())))
                .collect::<Result<_, _>>()
                .map_err(db)?;
            let mut since: HashMap<Vec<u8>, u64> = known.into_iter().collect();
            for (key, _) in since.clone() {
                let id: Option<ChunkId> = key.as_slice().try_into().ok();
                if id.is_none_or(|id| referenced.contains(&id) || !present.contains_key(&id)) {
                    t.orphans.remove(key.as_slice()).map_err(db)?;
                    since.remove(&key);
                }
            }
            for (id, size) in present {
                if referenced.contains(&id) {
                    continue;
                }
                let first = *since.entry(id.to_vec()).or_insert(now);
                if now.saturating_sub(first) >= retention {
                    self.chunks.remove(&id);
                    self.staging.remove(&id);
                    t.orphans.remove(id.as_slice()).map_err(db)?;
                    report.reclaimed_chunks += 1;
                    report.reclaimed_bytes += size;
                } else {
                    t.orphans.insert(id.as_slice(), first).map_err(db)?;
                    report.pending_chunks += 1;
                    report.pending_bytes += size;
                }
            }
            Ok(report)
        })
    }
}
