//! What this node knows of the other members' liveness: when each was heard, what they gossiped
//! of what they saw, and when each was seen asleep. This is the collective memory of ADR 0003.
//! [`Liveness`] is the one place that turns it into up, unknown, asleep, down, lost and paused,
//! so `status`, `activity`, the replicator's view and the purge never disagree.

use super::{DOWN_AFTER, STANDIN_AFTER};
use crate::control::{LastSeen, NodeState, Stranded, Tier};
use crate::load::Load;
use crate::members::Member;
use crate::members::MemberId;
use crate::net::NodeId;
use crate::ops::Nanos;
use crate::protocol::{Health, Sighting};
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Duration;

pub(super) const NANOS_PER_SEC: u64 = 1_000_000_000;

/// Since when a member counts as unseen here, for the long waits (taking it for asleep, giving
/// it up for lost): when this daemon last heard it, or, when it has not heard it since it
/// started, from then or from earlier when its peers relayed (in `Health`, ADR 0003) that
/// nobody has seen it for longer. A restarted daemon so does not wait out the stand-in delay
/// or the grace period again for a member long gone. What peers relay never makes a member
/// look more recently seen than the daemon's start. Being down is still judged on this
/// node's own sightings: a peer that just woke from a suspend relays stale ones, and waiting
/// out the short down delay again costs nothing.
fn unseen_since(
    last_heard: Option<Nanos>,
    member: MemberId,
    peer_health: &HashMap<NodeId, (Nanos, Health)>,
    started: Nanos,
) -> Nanos {
    last_heard.unwrap_or_else(|| {
        let relayed = peer_health.values().filter_map(|(arrived, h)| {
            let s = h.seen.iter().find(|s| s.member == member)?;
            Some(arrived.saturating_sub(Duration::from_secs(s.secs_ago).as_nanos() as Nanos))
        });
        relayed.max().map_or(started, |at| at.min(started))
    })
}

/// A member heard from within `DOWN_AFTER` is up.
fn is_up(last_heard: Option<Nanos>, now: Nanos) -> bool {
    last_heard.is_some_and(|t| now.saturating_sub(t) < DOWN_AFTER.as_nanos() as Nanos)
}

/// Not heard from since this daemon started, and not yet for long enough to call it down.
fn is_unknown(last_heard: Option<Nanos>, started: Nanos, now: Nanos) -> bool {
    last_heard.is_none() && now.saturating_sub(started) < DOWN_AFTER.as_nanos() as Nanos
}

/// `paused`: the member announced an upgrade pause and is expected back.
fn node_state(leaving: bool, up: bool, unknown: bool, lost: bool, paused: bool) -> NodeState {
    if leaving {
        NodeState::Leaving
    } else if up {
        NodeState::Online
    } else if unknown {
        NodeState::Unknown
    } else if paused {
        NodeState::Upgrading
    } else if lost {
        NodeState::Lost
    } else {
        NodeState::Down
    }
}

/// The members judged lost and the members paused for an upgrade, for [`Liveness::peer_state`].
#[derive(Clone, Copy)]
pub(super) struct Judged<'a> {
    pub lost: &'a HashSet<MemberId>,
    pub paused: &'a HashSet<MemberId>,
}

/// Which members the replicator should treat as lost, down, asleep, or kept for stand-ins.
pub(super) struct Verdicts {
    pub lost: HashSet<MemberId>,
    pub down: HashSet<MemberId>,
    pub sleeping: HashSet<MemberId>,
    pub kept: HashSet<MemberId>,
}

#[derive(Default)]
struct Memory {
    /// When each peer endpoint was last heard from.
    heard: HashMap<NodeId, Nanos>,
    /// The latest replication health each peer gossiped, and when it arrived.
    peer_health: HashMap<NodeId, (Nanos, Health)>,
    /// When each member was last seen asleep (unseen past the stand-in delay); the extra copies
    /// made for it are kept for the stand-in retention after.
    slept: HashMap<MemberId, Nanos>,
    /// When the cluster was last looked at.
    looked_at: Option<Nanos>,
    /// Until when a long gap since the last look (this node was asleep itself) makes every
    /// peer look unseen.
    settling_until: Nanos,
}

/// The liveness of the other members. Every method takes `now`, so a test can drive time.
/// One lock guards it all; none of the methods calls out while holding it.
pub(super) struct Liveness {
    /// When this daemon started: how long a never-heard member has been unseen.
    started: Nanos,
    memory: Mutex<Memory>,
}

impl Liveness {
    pub fn new(started: Nanos) -> Self {
        Self {
            started,
            memory: Mutex::new(Memory::default()),
        }
    }

    pub fn started(&self) -> Nanos {
        self.started
    }

    /// A frame arrived from `endpoint`.
    pub fn heard(&self, endpoint: NodeId, now: Nanos) {
        self.memory.lock().unwrap().heard.insert(endpoint, now);
    }

    /// The transport saw a frame from `endpoint` at `at`, which may be before the tick got to it.
    pub fn heard_at(&self, endpoint: NodeId, at: Nanos) {
        let mut m = self.memory.lock().unwrap();
        let seen = m.heard.entry(endpoint).or_insert(at);
        *seen = (*seen).max(at);
    }

    /// Take in the `Health` the member at `from` gossiped. `me` is never marked asleep.
    pub fn record_health(&self, from: NodeId, health: Health, me: Option<MemberId>, now: Nanos) {
        let mut m = self.memory.lock().unwrap();
        // Keep the latest sleep anyone saw: the extra copies made for a member stay as long as
        // the freshest sighting says.
        for s in health.slept.iter().filter(|s| Some(s.member) != me) {
            let at = now.saturating_sub(s.secs_ago * NANOS_PER_SEC);
            let known = m.slept.entry(s.member).or_insert(at);
            *known = (*known).max(at);
        }
        m.peer_health.insert(from, (now, health));
    }

    pub fn last_heard(&self, endpoint: NodeId) -> Option<Nanos> {
        self.memory.lock().unwrap().heard.get(&endpoint).copied()
    }

    pub fn is_up(&self, endpoint: NodeId, now: Nanos) -> bool {
        is_up(self.last_heard(endpoint), now)
    }

    /// Seen within the stand-in delay, going by this node's own sightings (a never-heard
    /// member counts from this daemon's start).
    pub fn is_present(&self, endpoint: NodeId, now: Nanos) -> bool {
        let seen = self.last_heard(endpoint).unwrap_or(self.started);
        now.saturating_sub(seen) < STANDIN_AFTER.as_nanos() as u64
    }

    /// The state `status` and `activity` show for another member. `judged` holds the verdicts
    /// from the same [`verdicts`](Self::verdicts) pass, so a caller names no flag by position.
    pub fn peer_state(&self, member: &Member, judged: Judged<'_>, now: Nanos) -> NodeState {
        let heard = self.last_heard(member.endpoint);
        node_state(
            member.leaving,
            is_up(heard, now),
            is_unknown(heard, self.started, now),
            judged.lost.contains(&member.id),
            judged.paused.contains(&member.id),
        )
    }

    /// The state shown for this node itself: always up unless it is leaving.
    pub fn own_state(member: &Member) -> NodeState {
        node_state(member.leaving, true, false, false, false)
    }

    /// Lost, down, asleep and kept-for-stand-ins members, with the paused ones counted as none
    /// of down or asleep (they are expected back). `grace` is the repair grace period in
    /// seconds; `standin_days` is the stand-in retention.
    pub fn verdicts(
        &self,
        members: &[Member],
        me: MemberId,
        grace: u64,
        standin_days: u64,
        paused: &HashSet<MemberId>,
        now: Nanos,
    ) -> Verdicts {
        let mut m = self.memory.lock().unwrap();
        let lost = self.unseen_members(&m, members, me, grace, true, now);
        let mut down =
            self.unseen_members(&m, members, me, grace.min(DOWN_AFTER.as_secs()), false, now);
        down.retain(|x| !paused.contains(x));
        let (mut sleeping, kept) = self.sleepers(&mut m, members, me, grace, standin_days, now);
        sleeping.retain(|x| !paused.contains(x));
        Verdicts {
            lost,
            down,
            sleeping,
            kept,
        }
    }

    /// The members asleep now (unseen for the stand-in delay) and those seen asleep within
    /// the retention, which includes the former.
    fn sleepers(
        &self,
        m: &mut Memory,
        members: &[Member],
        me: MemberId,
        grace: u64,
        standin_days: u64,
        now: Nanos,
    ) -> (HashSet<MemberId>, HashSet<MemberId>) {
        if standin_days == 0 {
            return (HashSet::new(), HashSet::new());
        }
        // After a long gap this node was likely asleep itself, so every peer looks unseen until
        // their hellos arrive: nobody is taken for asleep then.
        let stalled = m
            .looked_at
            .is_some_and(|at| now.saturating_sub(at) >= STANDIN_AFTER.as_nanos() as u64);
        m.looked_at = Some(now);
        if stalled {
            m.settling_until = now + DOWN_AFTER.as_nanos() as u64;
        }
        let settling = now < m.settling_until;
        let sleeping = if settling {
            HashSet::new()
        } else {
            self.unseen_members(
                m,
                members,
                me,
                grace.min(STANDIN_AFTER.as_secs()),
                true,
                now,
            )
        };
        let retention = Duration::from_secs(standin_days * 24 * 3600).as_nanos() as u64;
        for x in &sleeping {
            m.slept.insert(*x, now);
        }
        m.slept.retain(|_, at| now.saturating_sub(*at) < retention);
        let kept = m.slept.keys().copied().collect();
        (sleeping, kept)
    }

    /// Members unseen for `secs`; with `relayed`, by what peers relayed too ([`unseen_since`]),
    /// for the long waits only.
    fn unseen_members(
        &self,
        m: &Memory,
        members: &[Member],
        me: MemberId,
        grace: u64,
        relayed: bool,
        now: Nanos,
    ) -> HashSet<MemberId> {
        let grace = Duration::from_secs(grace).as_nanos() as u64;
        members
            .iter()
            .filter(|x| x.id != me)
            .filter(|x| {
                let own = m.heard.get(&x.endpoint).copied();
                let seen = if relayed {
                    unseen_since(own, x.id, &m.peer_health, self.started)
                } else {
                    own.unwrap_or(self.started)
                };
                now.saturating_sub(seen) >= grace
            })
            .map(|x| x.id)
            .collect()
    }

    /// Seconds the extra copies made for `member` are still kept, if it was seen asleep within
    /// the retention.
    pub fn standin_secs_left(
        &self,
        member: MemberId,
        standin_days: u64,
        now: Nanos,
    ) -> Option<u64> {
        let retention = Duration::from_secs(standin_days * 24 * 3600).as_nanos() as u64;
        let at = *self.memory.lock().unwrap().slept.get(&member)?;
        let left = retention.checked_sub(now.saturating_sub(at))?;
        Some(Duration::from_nanos(left).as_secs())
    }

    /// When each member in `members` that was ever heard was last heard, by member id.
    pub fn heard_by_member(&self, members: &[Member]) -> HashMap<MemberId, Nanos> {
        let m = self.memory.lock().unwrap();
        members
            .iter()
            .filter_map(|x| Some((x.id, *m.heard.get(&x.endpoint)?)))
            .collect()
    }

    /// What this node passes on of when the other members were last seen. A member not heard
    /// since this daemon started is passed on as unseen at least since what the peers relayed
    /// (never later than this start): a lower bound, so the cluster keeps knowing how long it
    /// has been gone through restarts, as long as one member keeps running. With nothing
    /// relayed, this daemon's own start is passed on only once it has run for the stand-in
    /// delay: a member that just restarted gossips before it has heard anyone, and its fresh
    /// start would otherwise win over what the others know (receivers take the latest sighting)
    /// and reset it at every rolling restart.
    pub fn sightings(&self, members: &[Member], me: Option<MemberId>, now: Nanos) -> Vec<Sighting> {
        let m = self.memory.lock().unwrap();
        let settled = now.saturating_sub(self.started) >= STANDIN_AFTER.as_nanos() as u64;
        members
            .iter()
            .filter(|x| Some(x.id) != me)
            .filter_map(|x| {
                let own = m.heard.get(&x.endpoint).copied();
                let relayed = m
                    .peer_health
                    .values()
                    .any(|(_, h)| h.seen.iter().any(|s| s.member == x.id));
                if own.is_none() && !relayed && !settled {
                    return None;
                }
                let at = unseen_since(own, x.id, &m.peer_health, self.started);
                Some(Sighting {
                    member: x.id,
                    secs_ago: now.saturating_sub(at) / NANOS_PER_SEC,
                })
            })
            .collect()
    }

    /// The sleeps this node passes on. Rounded up, so a time handed from node to node never
    /// creeps later.
    pub fn slept_sightings(&self, now: Nanos) -> Vec<Sighting> {
        self.memory
            .lock()
            .unwrap()
            .slept
            .iter()
            .map(|(member, at)| Sighting {
                member: *member,
                secs_ago: now.saturating_sub(*at).div_ceil(NANOS_PER_SEC),
            })
            .collect()
    }

    /// What the cluster last heard from each member that is not up, for the ones that have a
    /// report.
    pub fn reports_for_absent(
        &self,
        members: &[Member],
        me: Option<MemberId>,
        now: Nanos,
    ) -> Vec<Stranded> {
        let m = self.memory.lock().unwrap();
        members
            .iter()
            .filter(|x| Some(x.id) != me && !is_up(m.heard.get(&x.endpoint).copied(), now))
            .filter_map(|x| last_report(x.id, x.endpoint, &m.peer_health, now))
            .collect()
    }

    /// What the cluster last heard from `member`, which is not up.
    pub fn last_report(&self, member: &Member, now: Nanos) -> Option<Stranded> {
        last_report(
            member.id,
            member.endpoint,
            &self.memory.lock().unwrap().peer_health,
            now,
        )
    }

    /// When `member` was last seen by anyone, as the cluster remembers it: the freshest of this
    /// node's own sighting and those its peers gossiped, each aged by the time since it
    /// arrived. When nobody has seen it since this daemon started, only a lower bound is known.
    pub fn last_seen(&self, member: &Member, now: Nanos) -> LastSeen {
        let m = self.memory.lock().unwrap();
        let own = m
            .heard
            .get(&member.endpoint)
            .map(|at| now.saturating_sub(*at) / NANOS_PER_SEC);
        let theirs = m.peer_health.values().filter_map(|(arrived, h)| {
            let s = h.seen.iter().find(|s| s.member == member.id)?;
            Some(s.secs_ago + now.saturating_sub(*arrived) / NANOS_PER_SEC)
        });
        match own.into_iter().chain(theirs).min() {
            Some(secs) => LastSeen {
                secs_ago: secs,
                exact: true,
            },
            None => LastSeen {
                secs_ago: now.saturating_sub(self.started) / NANOS_PER_SEC,
                exact: false,
            },
        }
    }

    /// Run `f` on what `endpoint` last gossiped, if anything. `f` runs under the lock: it must
    /// not call back into `Liveness`.
    pub fn with_health<R>(&self, endpoint: NodeId, f: impl FnOnce(&Health) -> R) -> Option<R> {
        let m = self.memory.lock().unwrap();
        m.peer_health.get(&endpoint).map(|(_, h)| f(h))
    }

    /// The load `endpoint` last gossiped.
    pub fn load_of(&self, endpoint: NodeId) -> Option<Load> {
        self.with_health(endpoint, |h| h.load)
    }
}

/// What the cluster last heard from `member`, a member that is not up: its own last report as
/// this node received it, or one a peer relayed, whichever is fresher. `None` when no one has a
/// report.
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
