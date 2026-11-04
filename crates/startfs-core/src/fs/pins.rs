//! Pins: which chunks a write has stored but no file refers to yet, and the protocol that
//! keeps a removal of unused chunks from deleting one of them.
//!
//! A write that stores a chunk already on disk skips the write, so a removal that looked at
//! the tree before the write committed, and removed afterwards, would delete the chunk the
//! committed file points at. A writer therefore pins its chunks before it stores them and
//! keeps the [`Pinned`] guard until its file refers to them. A removal ([`Pins::remove_unused`])
//! looks at the tree without the lock, then removes in small batches under it, skipping every
//! chunk pinned since it began.
//!
//! The pin state and the removal's log of pins share one lock, so they cannot disagree.

use super::*;
use std::collections::HashSet;
use std::time::Duration;

#[derive(Default)]
struct State {
    held: HashMap<ChunkId, u32>,
    /// While a removal is under way: the chunks pinned since it began.
    log: Option<HashSet<ChunkId>>,
}

#[derive(Default)]
pub(super) struct Pins {
    state: Mutex<State>,
    /// One removal at a time, since they share the log.
    removing: Mutex<()>,
}

/// Chunks a write commit stores before the transaction that refers to them: until it
/// is dropped, garbage collection takes them as referenced.
pub(super) struct Pinned<'a> {
    pins: &'a Pins,
    ids: Vec<ChunkId>,
}

impl Pins {
    pub(super) fn hold(&self, ids: impl IntoIterator<Item = ChunkId>) -> Pinned<'_> {
        let ids: Vec<ChunkId> = ids.into_iter().collect();
        let mut state = self.state.lock().unwrap();
        if let Some(log) = state.log.as_mut() {
            log.extend(&ids);
        }
        for id in &ids {
            *state.held.entry(*id).or_default() += 1;
        }
        Pinned { pins: self, ids }
    }

    /// The chunks pinned right now.
    pub(super) fn held(&self) -> Vec<ChunkId> {
        self.state.lock().unwrap().held.keys().copied().collect()
    }

    /// Remove those of `ids` that `look` leaves in the set it is given and that no write has
    /// pinned since the removal began. `look` removes from the set every chunk the tree (or a
    /// kept version) refers to; it runs without the lock, so writes go on. Removal goes in
    /// small batches under the lock: a chunk that was unpinned and unreferenced at the look,
    /// and has not been pinned since, is still unused. Chunks pinned meanwhile are left for a
    /// later pass.
    pub(super) fn remove_unused(
        &self,
        ids: &[ChunkId],
        look: impl FnOnce(&mut HashSet<ChunkId>) -> FsResult<()>,
        remove: impl Fn(&ChunkId),
    ) -> FsResult<()> {
        const BATCH: usize = 128;
        if ids.is_empty() {
            return Ok(());
        }
        let _alone = self.removing.lock().unwrap();
        let mut unused: HashSet<ChunkId> = {
            let mut state = self.state.lock().unwrap();
            state.log = Some(HashSet::new());
            ids.iter()
                .filter(|id| !state.held.contains_key(*id))
                .copied()
                .collect()
        };
        // Ends the log however this function is left.
        let _log = EndLog(self);
        look(&mut unused)?;
        for batch in ids.chunks(BATCH) {
            {
                let state = self.state.lock().unwrap();
                let log = state.log.as_ref().expect("set above");
                (batch.iter())
                    .filter(|id| unused.contains(*id) && !log.contains(*id))
                    .for_each(&remove);
            }
            // The lock is not fair: without a pause this thread takes it straight back and a
            // write waiting to pin starves until the whole removal is done.
            std::thread::sleep(Duration::from_micros(200));
        }
        Ok(())
    }
}

struct EndLog<'a>(&'a Pins);

impl Drop for EndLog<'_> {
    fn drop(&mut self) {
        self.0.state.lock().unwrap().log = None;
    }
}

impl Drop for Pinned<'_> {
    fn drop(&mut self) {
        let mut state = self.pins.state.lock().unwrap();
        for id in &self.ids {
            if let Some(n) = state.held.get_mut(id) {
                *n -= 1;
                if *n == 0 {
                    state.held.remove(id);
                }
            }
        }
    }
}
