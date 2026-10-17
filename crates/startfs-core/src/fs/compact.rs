//! Compaction of the operation log.
//!
//! Entries every live peer holds are dropped: their effect lives on in the
//! merge registers. A node that was away while that happened is caught up
//! with a copy of the registers instead of the entries it missed.

use super::*;
use redb::ReadableTableMetadata;

impl LocalFs {
    /// Entries currently kept in the log.
    pub fn log_len(&self) -> FsResult<u64> {
        self.run(false, |t| Ok(t.log.len().map_err(db)?))
    }

    /// Objects whose merge state is kept.
    pub fn object_count(&self) -> FsResult<u64> {
        self.run(false, |t| Ok(t.objects.len().map_err(db)?))
    }

    /// Drop log entries below `upto` (per origin, never beyond what this node
    /// holds itself). With `settled`, every member holds the whole log, so the
    /// bookkeeping that guards against late arrivals goes too.
    pub fn compact(&self, upto: &Frontier, settled: bool) -> FsResult<()> {
        self.run(true, |t| {
            let mine: Vec<(u32, u64)> = t
                .frontier
                .iter()
                .map_err(db)?
                .map(|r| r.map(|(k, v)| (k.value(), v.value())))
                .collect::<Result<_, _>>()
                .map_err(db)?;
            for (origin, next) in mine {
                let target = upto.get(&origin).copied().unwrap_or(0).min(next);
                let base = t.logbase.get(origin).map_err(db)?.map_or(0, |g| g.value());
                if target > base {
                    t.log
                        .retain_in((origin, base)..(origin, target), |_, _| false)
                        .map_err(db)?;
                    t.logbase.insert(origin, target).map_err(db)?;
                }
            }
            if settled {
                t.purge_settled()?;
            }
            Ok(())
        })
    }
}
