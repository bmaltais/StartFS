//! Garbage collection of chunks no file references any more.
//!
//! Deleting or overwriting a file only changes metadata. A chunk that nothing
//! in the visible tree references is an orphan; it is removed once it has been
//! an orphan for the retention period. There is no reference counting: each
//! pass looks at what the tree references right now.
//!
//! An eligible orphan is only reclaimed once the reclaim gate opens for it:
//! the caller vouches that this member has caught up with every other member
//! since the orphan became eligible, so no re-reference can be pending.

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
    /// When the latest orphan the reclaim gate is holding back became
    /// eligible; `None` if the gate held nothing back.
    pub held_since: Option<u64>,
}

impl LocalFs {
    /// Remove chunks (replicas and staged) that have been unreferenced for
    /// at least `retention`, as observed by this node, and for which
    /// `gate_open(eligible_at)` holds. Orphans first seen at or before
    /// `purge_cutoff` need no retention at all (a purge); later ones still do.
    pub fn collect_garbage(
        &self,
        retention: Duration,
        purge_cutoff: Option<u64>,
        gate_open: impl Fn(u64) -> bool,
    ) -> FsResult<GcReport> {
        let now = self.clock.now();
        let retention = retention.as_nanos() as u64;
        // Chunk files are removed only after the transaction below has committed durably, which
        // also persists the lazy commits before it: a delete that a crash would undo never
        // loses the chunks of the file it brings back.
        let mut doomed: Vec<ChunkId> = Vec::new();
        let report = self.run(true, |t| {
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
                let eligible_at = if purge_cutoff.is_some_and(|cutoff| first <= cutoff) {
                    first
                } else {
                    first.saturating_add(retention)
                };
                let held = now >= eligible_at && !gate_open(eligible_at);
                if held {
                    report.held_since = report.held_since.max(Some(eligible_at));
                }
                if now >= eligible_at && !held {
                    doomed.push(id);
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
        })?;
        for id in doomed {
            self.chunks.remove(&id);
            self.staging.remove(&id);
        }
        Ok(report)
    }

    /// Remove from staging those of `ids` that no file in the visible tree refers to, and
    /// return them. A chunk is staged in the same transaction that makes a file refer to it,
    /// and this reads the tree under the same lock, so a chunk that is about to be referenced
    /// is never taken for an orphan.
    pub fn drop_unreferenced(&self, ids: &[ChunkId]) -> FsResult<Vec<ChunkId>> {
        let orphans = self.run(true, |t| {
            let mut referenced: HashSet<ChunkId> = HashSet::new();
            for row in t.inodes.iter().map_err(db)? {
                if let Content::Chunks(refs) = decode(row.map_err(db)?.1.value())?.content {
                    referenced.extend(refs.into_iter().map(|r| r.id));
                }
            }
            let orphans: Vec<ChunkId> = ids
                .iter()
                .filter(|id| !referenced.contains(*id) && self.has_staged(id))
                .copied()
                .collect();
            Ok(orphans)
        })?;
        // The commit above is durable and persists the lazy ones before it, so a delete that
        // made these orphans cannot be undone by a crash once they are gone.
        for id in &orphans {
            self.staging.remove(id);
        }
        Ok(orphans)
    }
}
