//! The status report: this node's view of the filesystem and its health.

use super::{DOWN_AFTER, Daemon};
use crate::control::{
    GcStatus, LogStatus, MemberStatus, NodeReport, NodeState, NodeStats, Repair, Shortfall,
    Staging, Status, Tier, Usage, Verdict,
};
use crate::error::Error;
use crate::members::{Member, Membership};
use crate::net::NodeId;
use crate::ops::FsOps;
use crate::protocol::{Health, Message};
use std::time::Duration;

/// Is the data safe right now? Copies beyond what the storage nodes allow are
/// not expected, so a file is only a worry if it has fewer than it could have.
fn verdict(
    wanted: u32,
    possible: u32,
    unreplicated: &[Shortfall],
    repair: Option<&Repair>,
    members: &[MemberStatus],
) -> Verdict {
    let target = wanted.min(possible);
    let behind = || unreplicated.iter().filter(|f| f.copies < target);
    if behind().any(|f| f.tier() == Tier::AtRisk) || repair.is_some_and(|r| r.at_risk > 0) {
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

/// Fold a member's report into the cluster view: each member counts only its
/// own share of the chunks, so the counts and the repair work add up.
fn merge_health(into: &mut Health, from: &Health) {
    for f in &from.unreplicated {
        match into.unreplicated.iter_mut().find(|g| g.copies == f.copies) {
            Some(g) => g.chunks += f.chunks,
            None => into.unreplicated.push(*f),
        }
    }
    into.unreplicated.sort_by_key(|f| f.copies);
    if let Some(r) = from.repair {
        let mine = into.repair.get_or_insert(Repair {
            at_risk: 0,
            degraded: 0,
        });
        mine.at_risk += r.at_risk;
        mine.degraded += r.degraded;
    }
}

/// Roughly how much data fits: the space members offer, divided by N.
pub(super) fn usable_bytes(membership: &Membership) -> u64 {
    let offered: u64 = membership.members().iter().map(|m| m.offer_bytes).sum();
    membership
        .filesystem()
        .map_or(0, |f| offered / u64::from(f.copies.max(1)))
}

impl Daemon {
    /// This node's own replication state: what it gossips and contributes to `status`.
    pub(super) fn local_health(&self) -> Health {
        Health {
            unreplicated: self
                .with_view(|v| self.replicator.unreplicated(v))
                .unwrap_or_default(),
            repair: self
                .with_view(|v| self.replicator.repair_progress(v))
                .flatten(),
        }
    }

    /// This node's storage figures: what it answers peers that ask.
    pub(super) fn local_node_stats(&self) -> Result<NodeStats, Error> {
        let stats = self.fs.stats()?;
        Ok(NodeStats {
            usage: Usage {
                chunks: stats.chunks,
                stored_bytes: stats.stored_bytes,
                offer_bytes: self
                    .membership
                    .lock()
                    .unwrap()
                    .my_offer(self.config.offer_bytes),
            },
            efficiency: self.fs.efficiency()?,
            pending_gc_bytes: self.gc.lock().unwrap().last.pending_bytes,
        })
    }

    /// Ask every member that is up for its storage figures, and wait for the
    /// answers together. A member that does not answer is listed without any.
    pub(super) fn node_reports(&self) -> Result<Vec<NodeReport>, Error> {
        let now = self.clock.now();
        let (members, me) = {
            let m = self.membership.lock().unwrap();
            (m.members(), m.me())
        };
        let heard = self.heard.lock().unwrap().clone();
        let up = |m: &Member| {
            heard
                .get(&m.endpoint)
                .is_some_and(|t| now.saturating_sub(*t) < DOWN_AFTER.as_nanos() as u64)
        };
        let asked: Vec<NodeId> = members
            .iter()
            .filter(|m| Some(m.id) != me && up(m))
            .map(|m| m.endpoint)
            .collect();
        self.stats_replies.lock().unwrap().clear();
        if let Some(transport) = self.transport() {
            for peer in &asked {
                transport.send(*peer, Message::StatsRequest.encode());
            }
        }
        self.wait_for(|| {
            let got = self.stats_replies.lock().unwrap();
            asked.iter().all(|p| got.contains_key(p)).then_some(())
        });
        let mut replies = std::mem::take(&mut *self.stats_replies.lock().unwrap());
        let mut rows = Vec::new();
        for m in members {
            let is_self = Some(m.id) == me;
            rows.push(NodeReport {
                member: m.id,
                addr: m.addr.clone(),
                is_self,
                stats: if is_self {
                    Some(self.local_node_stats()?)
                } else {
                    replies.remove(&m.endpoint)
                },
            });
        }
        Ok(rows)
    }

    pub(super) fn status(&self) -> Result<Status, Error> {
        let stats = self.fs.stats()?;
        let mut health = self.local_health();
        let held_since = self.gc.lock().unwrap().last.held_since;
        let gc_waiting_on = held_since.map_or_else(Vec::new, |since| self.gc_waiting_on(since));
        let lost = self.with_view(|v| v.lost.clone()).unwrap_or_default();
        let membership = self.membership.lock().unwrap();
        let filesystem = membership.filesystem().cloned();
        let now = self.clock.now();
        let heard = self.heard.lock().unwrap();
        let peer_health = self.peer_health.lock().unwrap();
        let transport = self.transport();
        let mut unheard = Vec::new();
        let members: Vec<MemberStatus> = membership
            .members()
            .into_iter()
            .map(|m| {
                let is_self = membership.me() == Some(m.id);
                let up = is_self
                    || heard
                        .get(&m.endpoint)
                        .is_some_and(|t| now.saturating_sub(*t) < DOWN_AFTER.as_nanos() as u64);
                if !is_self {
                    // Only a member that is up speaks for its own chunks.
                    match peer_health.get(&m.endpoint) {
                        Some(h) if up => merge_health(&mut health, h),
                        _ => unheard.push(m.id),
                    }
                }
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
                    problem: if up {
                        None
                    } else {
                        transport.as_ref().and_then(|t| t.last_error(m.endpoint))
                    },
                }
            })
            .collect();
        drop(peer_health);
        let Health {
            unreplicated,
            repair,
        } = health;
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
        let usable_bytes = usable_bytes(&membership);
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
            // A failed survey should not hide the rest of the report.
            efficiency: self.fs.efficiency().unwrap_or_default(),
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
            unheard,
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
            gc: {
                let gc = self.gc.lock().unwrap();
                GcStatus {
                    pending_chunks: gc.last.pending_chunks,
                    pending_bytes: gc.last.pending_bytes,
                    reclaimed_chunks: gc.reclaimed_chunks,
                    reclaimed_bytes: gc.reclaimed_bytes,
                    waiting_on: gc_waiting_on,
                }
            },
            warnings,
            conflicts: self.fs.conflicts()?,
        })
    }
}
