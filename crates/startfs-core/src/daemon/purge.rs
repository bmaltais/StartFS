//! `startfs purge`: reclaim deleted data now instead of after the retention period.

use super::{DOWN_AFTER, Daemon};
use crate::control::PurgeReport;
use crate::error::Error;
use crate::protocol::Message;
use std::time::Duration;

/// How long a purge also takes in deletes still on their way to this node,
/// so deleting a file and purging at once frees it on every member.
const SETTLE: Duration = Duration::from_secs(30);

/// A retention no orphan reaches: a pass with it only records which chunks are orphans.
const NEVER: Duration = Duration::from_secs(u32::MAX as u64);

impl Daemon {
    /// Ask every member to reclaim the chunks no file uses. All of them must
    /// be up: a member that misses it keeps its copies and, on returning,
    /// spreads them to the others again, which would start the retention over.
    /// Each member then purges on its own; the gate still holds back what a
    /// member that has not caught up may need.
    pub(super) fn purge(&self) -> Result<PurgeReport, Error> {
        let now = self.clock.now();
        let heard = self.heard.lock().unwrap().clone();
        let (me, members) = {
            let m = self.membership.lock().unwrap();
            (m.me(), m.members())
        };
        let up = |endpoint| {
            heard
                .get(&endpoint)
                .is_some_and(|t| now.saturating_sub(*t) < DOWN_AFTER.as_nanos() as u64)
        };
        let away: Vec<String> = members
            .iter()
            .filter(|m| Some(m.id) != me && !up(m.endpoint))
            .map(|m| m.addr.clone())
            .collect();
        if !away.is_empty() {
            return Err(Error::PurgeNeedsEveryMember(away.join(", ")));
        }
        if let Some(transport) = self.transport() {
            for m in members.iter().filter(|m| Some(m.id) != me) {
                transport.send(m.endpoint, Message::Purge.encode());
            }
        }
        self.begin_purge();
        Ok(PurgeReport {
            asked: members.iter().map(|m| m.id).collect(),
        })
    }

    /// Start purging on this node: what is orphaned by now, and what becomes
    /// so within [`SETTLE`], loses its retention; files deleted later keep theirs.
    pub(super) fn begin_purge(&self) {
        // Record the orphans first, so "known by now" includes a delete that
        // no pass has looked at yet.
        if let Err(e) = self.fs.collect_garbage(NEVER, None, |_| false) {
            eprintln!("startfs: cannot record the unused chunks for a purge: {e}");
            return;
        }
        *self.purging.lock().unwrap() = Some(self.clock.now() + SETTLE.as_nanos() as u64);
        // Collect at the next tick, not at the next interval.
        *self.last_gc.lock().unwrap() = None;
    }
}
