//! `startfs purge`: reclaim deleted data now instead of after the retention period.

use super::Daemon;
use super::standing::is_up;
use crate::control::PurgeReport;
use crate::error::Error;
use crate::net::Transport;
use crate::protocol::Message;
use std::collections::HashSet;
use std::time::Duration;

/// How long a purge also takes in deletes still on their way to this node,
/// so deleting a file and purging at once frees it on every member.
const SETTLE: Duration = Duration::from_secs(30);

/// A retention no orphan reaches: a pass with it only records which chunks are orphans.
const NEVER: Duration = Duration::from_secs(u32::MAX as u64);

impl Daemon {
    /// Ask every member to reclaim the chunks no file or kept version uses. The
    /// members that are up purge now; each one that is away is told once it is
    /// back and holds everything this node held now, so it has seen every delete
    /// the purge covers. Meanwhile the gate does not wait on it for what the
    /// purge frees: a version holds what a recent delete freed, which is all an
    /// edit it made while away could bring back.
    pub(super) fn purge(&self) -> Result<PurgeReport, Error> {
        let now = self.clock.now();
        let heard = self.heard.lock().unwrap().clone();
        let (me, members) = {
            let m = self.membership.lock().unwrap();
            (m.me(), m.members())
        };
        let up = |endpoint| is_up(heard.get(&endpoint).copied(), now);
        let (asked, away): (Vec<_>, Vec<_>) = members
            .iter()
            .partition(|m| Some(m.id) == me || up(m.endpoint));
        if let Some(transport) = self.transport() {
            for m in asked.iter().filter(|m| Some(m.id) != me) {
                transport.send(m.endpoint, Message::Purge.encode());
            }
        }
        let frontier = self.fs.frontier()?;
        self.purge_owed
            .lock()
            .unwrap()
            .extend(away.iter().map(|m| (m.endpoint, frontier.clone())));
        self.begin_purge();
        Ok(PurgeReport {
            asked: asked.iter().map(|m| m.id).collect(),
            deferred: away.iter().map(|m| m.id).collect(),
        })
    }

    /// Tell each member that missed a purge this node ran, once it is back and
    /// holds everything this node held then. Returns whether one was told.
    pub(super) fn pass_on_purge(&self, transport: &dyn Transport) -> bool {
        let mut owed = self.purge_owed.lock().unwrap();
        if owed.is_empty() {
            return false;
        }
        let members: HashSet<_> = self
            .membership
            .lock()
            .unwrap()
            .peers()
            .into_iter()
            .collect();
        // A member removed meanwhile is owed nothing.
        owed.retain(|peer, _| members.contains(peer));
        let caught_up: Vec<_> = {
            let syncer = self.syncer.lock().unwrap();
            let Some(syncer) = syncer.as_ref() else {
                return false;
            };
            owed.iter()
                .filter(|(peer, then)| {
                    syncer.confirmed(**peer).is_some_and(|holds| {
                        then.iter()
                            .all(|(origin, seq)| holds.get(origin).is_some_and(|s| s >= seq))
                    })
                })
                .map(|(peer, _)| *peer)
                .collect()
        };
        for peer in &caught_up {
            transport.send(*peer, Message::Purge.encode());
            owed.remove(peer);
        }
        !caught_up.is_empty()
    }

    /// Start purging on this node: what is orphaned by now, and what becomes
    /// so within [`SETTLE`], loses its retention; files deleted later keep theirs.
    pub(super) fn begin_purge(&self) {
        // The members away now: the gate does not wait on them for what this purge frees.
        let now = self.clock.now();
        let heard = self.heard.lock().unwrap().clone();
        *self.purge_excused.lock().unwrap() = self
            .membership
            .lock()
            .unwrap()
            .peers()
            .into_iter()
            .filter(|p| !is_up(heard.get(p).copied(), now))
            .collect();
        // Record the orphans first, so "known by now" includes a delete that
        // no pass has looked at yet.
        if let Err(e) = self.fs.collect_garbage(NEVER, None, |_, _| false) {
            eprintln!("startfs: cannot record the unused chunks for a purge: {e}");
            return;
        }
        *self.purging.lock().unwrap() = Some(self.clock.now() + SETTLE.as_nanos() as u64);
        // Collect at the next tick, not at the next interval.
        *self.last_gc.lock().unwrap() = None;
    }
}
