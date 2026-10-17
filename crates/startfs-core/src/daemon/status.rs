//! The status report: this node's view of the filesystem and its health.

use super::{DOWN_AFTER, Daemon};
use crate::control::{
    FileHealth, LogStatus, MemberStatus, NodeState, Repair, Staging, Status, Tier, Usage, Verdict,
};
use crate::error::Error;
use crate::ops::FsOps;
use std::time::Duration;

/// Is the data safe right now? Copies beyond what the storage nodes allow are
/// not expected, so a file is only a worry if it has fewer than it could have.
fn verdict(
    wanted: u32,
    possible: u32,
    unreplicated: &[FileHealth],
    repair: Option<&Repair>,
    members: &[MemberStatus],
) -> Verdict {
    let target = wanted.min(possible);
    let behind = || unreplicated.iter().filter(|f| f.copies < target);
    if behind().any(|f| f.tier == Tier::AtRisk) || repair.is_some_and(|r| r.at_risk > 0) {
        Verdict::AtRisk
    } else if behind().next().is_some()
        || repair.is_some()
        || members
            .iter()
            .any(|m| matches!(m.state, NodeState::Down | NodeState::Lost))
    {
        Verdict::Degraded
    } else if possible < wanted {
        Verdict::BestEffort
    } else {
        Verdict::Safe
    }
}

impl Daemon {
    pub(super) fn status(&self) -> Result<Status, Error> {
        let stats = self.fs.stats()?;
        let unreplicated = self
            .with_view(|v| self.replicator.unreplicated(v))
            .unwrap_or_default();
        let repair = self
            .with_view(|v| self.replicator.repair_progress(v))
            .flatten();
        let lost = self.with_view(|v| v.lost.clone()).unwrap_or_default();
        let membership = self.membership.lock().unwrap();
        let filesystem = membership.filesystem().cloned();
        let now = self.clock.now();
        let heard = self.heard.lock().unwrap();
        let members: Vec<MemberStatus> = membership
            .members()
            .into_iter()
            .map(|m| {
                let is_self = membership.me() == Some(m.id);
                let up = is_self
                    || heard
                        .get(&m.endpoint)
                        .is_some_and(|t| now.saturating_sub(*t) < DOWN_AFTER.as_nanos() as u64);
                MemberStatus {
                    id: m.id,
                    addr: m.addr,
                    state: if m.leaving {
                        NodeState::Leaving
                    } else if up {
                        NodeState::Online
                    } else if lost.contains(&m.id) {
                        NodeState::Lost
                    } else {
                        NodeState::Down
                    },
                    offer_bytes: m.offer_bytes,
                    is_self,
                }
            })
            .collect();
        let nodes = members.len() as u32;
        // Only members offering space, and with room left, can hold a copy.
        let full = self.replicator.full_members();
        let storage_nodes = membership
            .members()
            .iter()
            .filter(|m| m.offer_bytes > 0 && !m.leaving && !full.contains(&m.id))
            .count() as u32;
        let copies_possible = filesystem
            .as_ref()
            .map_or(0, |f| f.copies.min(storage_nodes));
        let offered: u64 = members.iter().map(|m| m.offer_bytes).sum();
        let usable_bytes = filesystem
            .as_ref()
            .map_or(0, |f| offered / u64::from(f.copies.max(1)));
        let mut warnings = Vec::new();
        for m in &members {
            if full.contains(&m.id) {
                warnings.push(format!(
                    "{} is full: it cannot take new copies{}",
                    m.addr,
                    if m.is_self { " (this node)" } else { "" }
                ));
            }
        }
        if let Some(f) = &filesystem
            && copies_possible < f.copies
        {
            warnings.push(format!(
                "redundancy: {} wanted, {} possible (only {} node{} can store copies)",
                f.copies,
                copies_possible,
                storage_nodes,
                if storage_nodes == 1 { "" } else { "s" }
            ));
        }
        let verdict = filesystem.as_ref().map(|f| {
            verdict(
                f.copies,
                copies_possible,
                &unreplicated,
                repair.as_ref(),
                &members,
            )
        });
        Ok(Status {
            verdict,
            filesystem,
            node_state: NodeState::Online,
            usage: Usage {
                chunks: stats.chunks,
                stored_bytes: stats.stored_bytes,
                offer_bytes: membership.my_offer(self.config.offer_bytes),
            },
            staging: {
                let s = self.fs.staging_stats()?;
                Staging {
                    chunks: s.chunks,
                    bytes: s.stored_bytes,
                }
            },
            cache: self.fs.cache_stats(),
            members,
            nodes,
            copies_possible,
            usable_bytes,
            unreplicated,
            repair,
            log: LogStatus {
                entries: self.fs.log_len().unwrap_or(0),
                objects: self.fs.object_count().unwrap_or(0),
            },
            scrub: self.scrubber.status(
                now,
                Duration::from_secs(self.config.scrub_period_secs),
                self.on_battery(),
                self.replicator.healing(),
            ),
            warnings,
            conflicts: self.fs.conflicts()?,
        })
    }
}
