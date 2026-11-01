//! Versioned history: the states an overwrite or a delete replaced.
//!
//! A version is kept for the filesystem's window from when it was replaced
//! (ADR 0008). Versions are derived from log entries like the merge registers,
//! so members holding the same entries hold the same versions; they live in
//! their own table so that rebuilding the tree never decodes them.

use super::merge::{Object, Version};
use super::*;
use crate::chunkstore::ChunkId;
use std::collections::HashSet;
use std::time::Duration;

/// A state that lasted less than this is not kept: the files a build writes and
/// deletes within the replication hold still never leave the node.
pub const MIN_VERSION_LIFE: Duration = Duration::from_secs(30);

/// One earlier version of an object, and the stamp of what replaced it.
#[derive(Serialize, Deserialize, Clone)]
pub(super) struct Past {
    pub(super) version: Version,
    pub(super) until: Hlc,
}

type Key = (u64, u64, u32, u32);

fn key(ino: Ino, h: Hlc) -> Key {
    (ino, h.wall, h.counter, h.node.0)
}

fn every(ino: Ino) -> std::ops::RangeInclusive<Key> {
    (ino, 0, 0, 0)..=(ino, u64::MAX, u32::MAX, u32::MAX)
}

fn enc<T: Serialize>(v: &T) -> Vec<u8> {
    postcard::to_stdvec(v).expect("serialises")
}

fn dec<'a, T: Deserialize<'a>>(raw: &'a [u8]) -> FsResult<T> {
    postcard::from_bytes(raw).map_err(|e| FsError::Io(format!("bad record: {e}")))
}

/// Whether a state that began at `start` and ended at `end` is kept at `now`.
fn kept(start: Hlc, end: Hlc, now: Nanos, window: Nanos) -> bool {
    end.wall.saturating_sub(start.wall) >= MIN_VERSION_LIFE.as_nanos() as u64
        && end.wall.saturating_add(window) > now
}

impl Tables<'_> {
    /// Keep `version` of `ino` as replaced at `until`. Recording it again keeps
    /// the earliest replacement, so the result does not depend on arrival order.
    pub(super) fn retire(&mut self, ino: Ino, version: Version, until: Hlc) -> FsResult<()> {
        let k = key(ino, version.hlc);
        let until = match self.history.get(k).map_err(db)? {
            Some(g) => dec::<Past>(g.value())?.until.min(until),
            None => until,
        };
        self.history
            .insert(k, enc(&Past { version, until }).as_slice())
            .map_err(db)?;
        Ok(())
    }

    /// A version derived from `of` stamped `by` arrived: if `of` is already
    /// kept, it was replaced no later than that.
    pub(super) fn replaced_by(&mut self, ino: Ino, of: Hlc, by: Hlc) -> FsResult<()> {
        let k = key(ino, of);
        let Some(mut past) = self
            .history
            .get(k)
            .map_err(db)?
            .map(|g| dec::<Past>(g.value()))
            .transpose()?
        else {
            return Ok(());
        };
        if by < past.until {
            past.until = by;
            self.history.insert(k, enc(&past).as_slice()).map_err(db)?;
        }
        Ok(())
    }

    /// The kept versions of `ino`, oldest first.
    pub(super) fn versions_of(&self, ino: Ino) -> FsResult<Vec<Past>> {
        self.raw_versions_of(ino)?.iter().map(|r| dec(r)).collect()
    }

    /// [`Self::versions_of`], encoded, for a state transfer.
    pub(super) fn raw_versions_of(&self, ino: Ino) -> FsResult<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        for row in self.history.range(every(ino)).map_err(db)? {
            out.push(row.map_err(db)?.1.value().to_vec());
        }
        Ok(out)
    }

    /// Fold in a version a peer sent in a state transfer.
    pub(super) fn join_version(&mut self, ino: Ino, raw: &[u8]) -> FsResult<()> {
        let past: Past = dec(raw)?;
        self.retire(ino, past.version, past.until)
    }

    /// Forget every version of an object that is itself forgotten.
    pub(super) fn forget_versions(&mut self, ino: Ino) -> FsResult<()> {
        self.history
            .retain_in(every(ino), |_, _| false)
            .map_err(db)?;
        Ok(())
    }

    /// The chunks of every version kept at `now`: the earlier versions of
    /// objects, and the last version of each deleted one. Versions no longer
    /// kept are forgotten on the way when `prune` is set.
    pub(super) fn version_chunks(
        &mut self,
        now: Nanos,
        window: Nanos,
        prune: bool,
    ) -> FsResult<HashSet<ChunkId>> {
        let mut out = HashSet::new();
        let mut gone = Vec::new();
        for row in self.history.iter().map_err(db)? {
            let (k, v) = row.map_err(db)?;
            let past: Past = dec(v.value())?;
            if !kept(past.version.hlc, past.until, now, window) {
                gone.push(k.value());
            } else if let Content::Chunks(refs) = past.version.content {
                out.extend(refs.into_iter().map(|r| r.id));
            }
        }
        if prune {
            for k in gone {
                self.history.remove(k).map_err(db)?;
            }
        }
        for row in self.objects.iter().map_err(db)? {
            let (k, v) = row.map_err(db)?;
            if k.value() & (1 << 63) != 0 {
                continue; // a conflict copy's own register
            }
            let o: Object = dec(v.value())?;
            let (Some(death), Some(last)) = (o.death(), o.winner()) else {
                continue;
            };
            if kept(last.hlc, death, now, window)
                && let Content::Chunks(refs) = &last.content
            {
                out.extend(refs.iter().map(|r| r.id));
            }
        }
        Ok(out)
    }
}

impl LocalFs {
    /// Set how long a replaced version is kept (the filesystem's window).
    pub fn set_keep_versions(&self, window: Duration) {
        self.keep_versions
            .store(window.as_nanos() as u64, Ordering::SeqCst);
    }

    /// The window, in nanoseconds.
    pub(super) fn window(&self) -> Nanos {
        self.keep_versions.load(Ordering::SeqCst)
    }
}
