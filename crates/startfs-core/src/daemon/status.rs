//! The status report: this node's view of the filesystem and its health.

use super::Daemon;
use super::standing::{is_unknown, is_up, node_state};
use crate::control::{
    GcStatus, LastSeen, LogStatus, MemberStatus, NodeReport, NodeState, NodeStats, Repair,
    Shortfall, Staging, Status, Stranded, TickBusy, Tier, Usage, Verdict,
};
use crate::error::Error;
use crate::load::Load;
use crate::members::{Member, MemberId, Membership};
use crate::net::NodeId;
use crate::ops::FsOps;
use crate::ops::Nanos;
use crate::protocol::{Health, Message, Sighting};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::time::Duration;

const NANOS_PER_SEC: u64 = 1_000_000_000;

/// Is the data safe right now? Copies beyond what the storage nodes allow are
/// not expected, so a file is only a worry if it has fewer than it could have.
/// A lost member's copies no longer count and have been re-made elsewhere, so
/// once nothing is short and no repair runs it no longer reduces redundancy;
/// a merely down member may still return, so it does.
fn verdict_of(
    wanted: u32,
    possible: u32,
    unreplicated: &[Shortfall],
    repair: Option<&Repair>,
    members: &[MemberStatus],
    stranded: &[Stranded],
) -> Verdict {
    let target = wanted.min(possible);
    let behind = || unreplicated.iter().filter(|f| f.copies < target);
    // A single copy that only an unreachable member is known to hold.
    let stuck = target > 1 && !stranded.is_empty();
    if stuck || behind().any(|f| f.tier() == Tier::AtRisk) || repair.is_some_and(|r| r.at_risk > 0)
    {
        Verdict::AtRisk
    } else if behind().next().is_some()
        || repair.is_some()
        || members.iter().any(|m| m.state == NodeState::Down)
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

/// When a member was last seen by anyone, as the cluster remembers it: the
/// freshest of this node's own sighting and those its peers gossiped, each
/// aged by the time since it arrived. When nobody has seen it since this
/// daemon started, only a lower bound is known.
fn last_seen<'a>(
    member: MemberId,
    own: Option<Nanos>,
    peers: impl Iterator<Item = &'a (Nanos, Health)>,
    started: Nanos,
    now: Nanos,
) -> LastSeen {
    let own = own.map(|at| now.saturating_sub(at) / NANOS_PER_SEC);
    let theirs = peers.filter_map(|(arrived, h)| {
        let s = h.seen.iter().find(|s| s.member == member)?;
        Some(s.secs_ago + now.saturating_sub(*arrived) / NANOS_PER_SEC)
    });
    match own.into_iter().chain(theirs).min() {
        Some(secs) => LastSeen {
            secs_ago: secs,
            exact: true,
        },
        None => LastSeen {
            secs_ago: now.saturating_sub(started) / NANOS_PER_SEC,
            exact: false,
        },
    }
}

/// What the cluster last heard from `member`, a member that is not up: its
/// own last report as this node received it, or one a peer relayed, whichever
/// is fresher. `None` when no one has a report.
fn last_report(
    member: MemberId,
    endpoint: NodeId,
    peers: &HashMap<NodeId, (Nanos, Health)>,
    now: Nanos,
) -> Option<Stranded> {
    let age = |arrived: &Nanos| now.saturating_sub(*arrived) / NANOS_PER_SEC;
    let own = peers.get(&endpoint).map(|(arrived, h)| Stranded {
        member,
        chunks: h
            .unreplicated
            .iter()
            .filter(|f| f.tier() == Tier::AtRisk)
            .map(|f| f.chunks)
            .sum(),
        secs_ago: age(arrived),
    });
    let relayed = peers
        .iter()
        .filter(|(e, _)| **e != endpoint)
        .filter_map(|(_, (arrived, h))| {
            let r = h.reports.iter().find(|r| r.member == member)?;
            Some(Stranded {
                secs_ago: r.secs_ago + age(arrived),
                ..*r
            })
        });
    own.into_iter().chain(relayed).min_by_key(|r| r.secs_ago)
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
        let now = self.clock.now();
        let seen = {
            let membership = self.membership.lock().unwrap();
            let heard = self.heard.lock().unwrap();
            membership
                .members()
                .iter()
                .filter(|m| Some(m.id) != membership.me())
                .filter_map(|m| {
                    let at = heard.get(&m.endpoint)?;
                    Some(Sighting {
                        member: m.id,
                        secs_ago: now.saturating_sub(*at) / NANOS_PER_SEC,
                    })
                })
                .collect()
        };
        let reports = {
            let membership = self.membership.lock().unwrap();
            let heard = self.heard.lock().unwrap();
            let peers = self.peer_health.lock().unwrap();
            let up = |endpoint: &NodeId| is_up(heard.get(endpoint).copied(), now);
            membership
                .members()
                .iter()
                .filter(|m| Some(m.id) != membership.me() && !up(&m.endpoint))
                .filter_map(|m| last_report(m.id, m.endpoint, &peers, now))
                .collect()
        };
        // Rounded up, so a time handed from node to node never creeps later.
        let slept = self
            .slept
            .lock()
            .unwrap()
            .iter()
            .map(|(member, at)| Sighting {
                member: *member,
                secs_ago: now.saturating_sub(*at).div_ceil(NANOS_PER_SEC),
            })
            .collect();
        Health {
            load: self.local_load(),
            seen,
            reports,
            slept,
            unreplicated: self
                .with_view(|v| self.replicator.unreplicated(v))
                .unwrap_or_default(),
            repair: self
                .with_view(|v| self.replicator.repair_progress(v))
                .flatten(),
        }
    }

    /// How far behind this node's tick and inbox are, as it gossips it.
    pub(super) fn local_load(&self) -> Load {
        let began = self.last_tick.lock().unwrap().unwrap_or(self.started);
        let inbox = self.transport().and_then(|t| t.inbox());
        Load {
            tick_secs_ago: self.clock.now().saturating_sub(began) / NANOS_PER_SEC,
            inbox_messages: inbox.map_or(0, |i| i.messages),
            inbox_bytes: inbox.map_or(0, |i| i.bytes),
        }
    }

    /// Remember whether the member at `from` is behind, going by what it just
    /// gossiped; `View::behind` follows.
    pub(super) fn note_load(&self, from: NodeId, load: Load) {
        let Some(member) = self
            .membership
            .lock()
            .unwrap()
            .members()
            .iter()
            .find(|m| m.endpoint == from)
            .map(|m| m.id)
        else {
            return;
        };
        let mut behind = self.behind.lock().unwrap();
        let was = behind.contains(&member);
        if load.is_behind(self.config.behind_inbox_messages, was) {
            behind.insert(member);
        } else {
            behind.remove(&member);
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
        let up = |m: &Member| is_up(heard.get(&m.endpoint).copied(), now);
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
        self.status_with_gate().map(|(status, _)| status)
    }

    /// The status, and whether the data is safe enough for the next member of a rolling
    /// upgrade to restart: the verdict is as it is but for members that are down.
    pub(super) fn status_with_gate(&self) -> Result<(Status, bool), Error> {
        let stats = self.fs.stats()?;
        let mut health = self.local_health();
        let held_since = self.gc.lock().unwrap().last.held_since;
        let gc_waiting_on = held_since.map_or_else(Vec::new, |since| self.gc_waiting_on(since));
        let lost = self.with_view(|v| v.lost.clone()).unwrap_or_default();
        // Both look at the member list: ask before `status` holds it.
        let paused = self.paused_members(self.clock.now());
        let upgrade = self.upgrade_status(self.clock.now());
        let rechecking = self
            .with_view(|v| self.replicator.rechecking(v))
            .unwrap_or(0);
        let membership = self.membership.lock().unwrap();
        let filesystem = membership.filesystem().cloned();
        let now = self.clock.now();
        let heard = self.heard.lock().unwrap();
        let peer_health = self.peer_health.lock().unwrap();
        let behind_now = self.behind.lock().unwrap().clone();
        let transport = self.transport();
        let standin_days = filesystem.as_ref().map_or(0, |f| f.standin_days);
        let mut unheard = Vec::new();
        let mut stranded = Vec::new();
        let members: Vec<MemberStatus> = membership
            .members()
            .into_iter()
            .map(|m| {
                let is_self = membership.me() == Some(m.id);
                let up = is_self || is_up(heard.get(&m.endpoint).copied(), now);
                // A member in its upgrade pause is back soon: what it last reported
                // still counts and nothing about it is alarming.
                let expected = up || paused.contains(&m.id);
                let (release, protocol, upgrade) = self.member_upgrade(m.endpoint, is_self);
                if !is_self {
                    // Only a member that is up speaks for its own chunks.
                    match peer_health.get(&m.endpoint) {
                        Some((_, h)) if expected => merge_health(&mut health, h),
                        _ => unheard.push(m.id),
                    }
                    if !expected
                        && let Some(r) = last_report(m.id, m.endpoint, &peer_health, now)
                        && r.chunks > 0
                    {
                        stranded.push(r);
                    }
                }
                MemberStatus {
                    id: m.id,
                    addr: m.addr,
                    state: node_state(
                        m.leaving,
                        up,
                        !is_self && is_unknown(heard.get(&m.endpoint).copied(), self.started, now),
                        lost.contains(&m.id),
                        paused.contains(&m.id),
                    ),
                    release,
                    protocol,
                    upgrade,
                    offer_bytes: m.offer_bytes,
                    weight_bytes: m.weight_bytes,
                    site: m.site,
                    is_self,
                    behind: if !up {
                        None
                    } else if is_self {
                        Some(self.local_load())
                    } else {
                        peer_health.get(&m.endpoint).map(|(_, h)| h.load)
                    }
                    .filter(|l| {
                        // A peer is behind by the same verdict the replicator acts on
                        // (with its hysteresis); this node has no gossiped verdict.
                        if is_self {
                            l.is_behind(self.config.behind_inbox_messages, false)
                        } else {
                            behind_now.contains(&m.id)
                        }
                    }),
                    standin_secs_left: self.standin_secs_left(m.id, standin_days, now),
                    last_seen: (!up).then(|| {
                        last_seen(
                            m.id,
                            heard.get(&m.endpoint).copied(),
                            peer_health.values(),
                            self.started,
                            now,
                        )
                    }),
                    rtt_micros: (up && !is_self)
                        .then(|| {
                            transport
                                .as_ref()
                                .and_then(|t| t.rtt(m.endpoint))
                                .map(|d| d.as_micros() as u64)
                                .or_else(|| {
                                    let nanos =
                                        self.latency.lock().unwrap().estimate(m.endpoint, now);
                                    nanos.map(|n| n / 1_000)
                                })
                        })
                        .flatten(),
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
            seen: _,
            reports: _,
            slept: _,
            load: _,
        } = health;
        let nodes = members.len() as u32;
        // Only members offering space, and with room left, can hold a copy.
        let full = self.replicator.full_members();
        let storage_nodes = membership
            .members()
            .iter()
            .filter(|m| m.takes_copies() && !full.contains(&m.id))
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
        for m in &members {
            if let Some(load) = &m.behind {
                warnings.push(format!(
                    "{} is {}{}",
                    m.addr,
                    load.behind_text(),
                    if m.is_self {
                        " (this node)"
                    } else {
                        "; it is sent nothing new until it catches up"
                    }
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
        if let Some(f) = &filesystem {
            // A site is a label, or the member itself when it has none.
            let site = |m: &crate::members::Member| {
                if m.site.is_empty() {
                    format!("#{}", m.id)
                } else {
                    format!("={}", m.site)
                }
            };
            let all = membership.members();
            // A site only counts if some member there offers space; a full or lost one still does.
            let offering = |m: &&crate::members::Member| m.takes_copies();
            let declared: std::collections::HashSet<String> =
                all.iter().filter(offering).map(&site).collect();
            let reachable: std::collections::HashSet<String> = all
                .iter()
                .filter(|m| m.takes_copies() && !full.contains(&m.id) && !lost.contains(&m.id))
                .map(&site)
                .collect();
            let wanted = (f.copies as usize).min(declared.len());
            if all.iter().filter(offering).any(|m| !m.site.is_empty()) && reachable.len() < wanted {
                warnings.push(format!(
                    "sites: copies should span {wanted} sites, but only {} can take them \
                     (a site is full, lost or offers nothing)",
                    reachable.len()
                ));
            }
        }
        let verdict = filesystem.as_ref().map(|f| {
            verdict_of(
                f.copies,
                copies_possible,
                &unreplicated,
                repair.as_ref(),
                &members,
                &stranded,
            )
        });
        // Members that are gone hold nothing back from the next restart: the data is
        // safe when nothing is short of copies among the rest. One that is only briefly
        // unseen may be back at once, so it still counts.
        let gone = |m: &MemberStatus| {
            m.state == NodeState::Down
                && m.last_seen
                    .is_some_and(|l| l.secs_ago >= super::STANDIN_AFTER.as_secs())
        };
        let present: Vec<MemberStatus> = members.iter().filter(|m| !gone(m)).cloned().collect();
        let gate = filesystem.as_ref().is_some_and(|f| {
            matches!(
                verdict_of(
                    f.copies,
                    copies_possible,
                    &unreplicated,
                    repair.as_ref(),
                    &present,
                    &stranded,
                ),
                Verdict::Safe | Verdict::BestEffort
            )
        });
        Ok((
            Status {
                stopping: self.stopping.load(Ordering::SeqCst),
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
                        waiting_on: if s.chunks > 0 {
                            self.replicator.staged_waiting_on()
                        } else {
                            None
                        },
                    }
                },
                cache: self.fs.cache_stats(),
                members,
                upgrade,
                nodes,
                copies_possible,
                usable_bytes,
                unreplicated,
                repair,
                rechecking,
                unheard,
                stranded,
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
                        purging: self.purging.lock().unwrap().is_some(),
                    }
                },
                warnings,
                conflicts: self.fs.conflicts()?,
                tick_busy: self.tick_busy().filter(|(busy, _)| *busy > 0.8).map(
                    |(busy, avg_ms)| TickBusy {
                        percent: (busy * 100.0).round() as u32,
                        avg_ms: avg_ms.round() as u64,
                    },
                ),
            },
            gate,
        ))
    }
}
