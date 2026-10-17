//! What the scrub needs from the filesystem: reading a replica back against
//! its hash, and remembering when the last full pass finished.

use super::*;

impl LocalFs {
    /// Whether the replica reads back intact (its bytes match its hash).
    pub fn replica_is_sound(&self, id: &ChunkId) -> bool {
        self.chunks.get(id).is_ok()
    }

    /// When the last full scrub pass finished, if one ever did.
    pub fn scrub_last_done(&self) -> Option<Nanos> {
        self.run(false, |t| {
            Ok(t.counters.get("scrub_done").map_err(db)?.map(|g| g.value()))
        })
        .ok()
        .flatten()
    }

    pub fn set_scrub_done(&self, when: Nanos) {
        let _ = self.run(true, |t| {
            t.counters.insert("scrub_done", when).map_err(db)?;
            Ok(())
        });
    }
}
