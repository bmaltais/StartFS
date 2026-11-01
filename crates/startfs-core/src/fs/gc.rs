//! Garbage collection of chunks no file references any more.
//!
//! Deleting or overwriting a file only changes metadata. A chunk that nothing
//! in the visible tree references is an orphan; it is removed once it has been
//! an orphan for the retention period. There is no reference counting: each
//! pass looks at what the tree references right now.
//!
//! A chunk a kept version uses (ADR 0008) is still an orphan, so its retention
//! runs alongside the window, but it is reclaimed only once no version needs it.
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
    /// `gate_open(eligible_at, purged)` holds. Orphans first seen at or before
    /// `purge_cutoff` need no retention at all (a purge, so `purged` is set for
    /// them); later ones still do.
    pub fn collect_garbage(
        &self,
        retention: Duration,
        purge_cutoff: Option<u64>,
        gate_open: impl Fn(u64, bool) -> bool,
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
            let versioned = t.version_chunks(now, self.window(), true)?;
            let only_versions = versioned
                .iter()
                .filter(|(id, _)| !referenced.contains(*id))
                .map(|(_, len)| len)
                .sum();
            self.version_bytes.store(only_versions, Ordering::SeqCst);
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
                if versioned.contains_key(&id) {
                    // Held by a version, not waiting for GC: its retention runs meanwhile, but
                    // never more than one retention behind, so that once the version is released
                    // the gate still needs a catch-up from about then (a restore made elsewhere
                    // just before may be on its way).
                    let first = first.max(now.saturating_sub(retention));
                    t.orphans.insert(id.as_slice(), first).map_err(db)?;
                    continue;
                }
                let purged = purge_cutoff.is_some_and(|cutoff| first <= cutoff);
                let eligible_at = if purged {
                    first
                } else {
                    first.saturating_add(retention)
                };
                let held = now >= eligible_at && !gate_open(eligible_at, purged);
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

    /// Work out again which chunks a file or kept version uses, if the tree has
    /// changed since. It reads every register, so the replicator does it at most
    /// once a round, and only when it has held chunks to look at.
    pub fn refresh_used(&self) {
        let generation = self.tree_generation.load(Ordering::SeqCst);
        if self.used.lock().unwrap().0 == Some(generation) {
            return;
        }
        let (now, window) = (self.clock.now(), self.window());
        let used = self.run(false, |t| {
            let mut used: HashSet<ChunkId> = HashSet::new();
            for row in t.inodes.iter().map_err(db)? {
                if let Content::Chunks(refs) = decode(row.map_err(db)?.1.value())?.content {
                    used.extend(refs.into_iter().map(|r| r.id));
                }
            }
            used.extend(t.version_chunks(now, window, false)?.into_keys());
            Ok(used)
        });
        match used {
            Ok(used) => *self.used.lock().unwrap() = (Some(generation), used),
            Err(e) => eprintln!("startfs: cannot work out which chunks are in use: {e}"),
        }
    }

    /// Whether no file or kept version used the chunk when [`Self::refresh_used`] last
    /// looked, so it is garbage waiting to be reclaimed: repair does not copy it.
    /// Never, before the first look.
    pub fn is_unused(&self, id: &ChunkId) -> bool {
        let used = self.used.lock().unwrap();
        used.0.is_some() && !used.1.contains(id)
    }

    /// Remove from staging those of `ids` that no file in the visible tree or kept version
    /// refers to, and return them. A chunk is staged in the same transaction that makes a file refer to it,
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
            let mut orphans: Vec<ChunkId> = ids
                .iter()
                .filter(|id| !referenced.contains(*id) && self.has_staged(id))
                .copied()
                .collect();
            // A kept version's chunks are replicated like the tree's. Looked up only when
            // the tree leaves something out, since it reads every object's register.
            if !orphans.is_empty() {
                let versioned = t.version_chunks(self.clock.now(), self.window(), false)?;
                orphans.retain(|id| !versioned.contains_key(id));
            }
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
