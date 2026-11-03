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

const NANOS_PER_SEC: u64 = 1_000_000_000;

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
    /// When the cluster was last looked at, and until when a long gap since then (this node
    /// was asleep itself) makes every peer look unseen.
    looked: (Option<Nanos>, Nanos),
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

    /// The state `status` and `activity` show for `member`. This node (`is_self`) is always up.
    pub fn state(
        &self,
        member: &Member,
        is_self: bool,
        lost: bool,
        paused: bool,
        now: Nanos,
    ) -> NodeState {
        let heard = self.last_heard(member.endpoint);
        let up = is_self || is_up(heard, now);
        let unknown = !is_self && is_unknown(heard, self.started, now);
        node_state(member.leaving, up, unknown, lost, paused)
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
            .looked
            .0
            .is_some_and(|at| now.saturating_sub(at) >= STANDIN_AFTER.as_nanos() as u64);
        m.looked.0 = Some(now);
        if stalled {
            m.looked.1 = now + DOWN_AFTER.as_nanos() as u64;
        }
        let settling = now < m.looked.1;
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

    /// When each member in `members` (but `me`) was last heard, by member id.
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

    /// Run `f` on what `endpoint` last gossiped, if anything.
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

#[cfg(test)]
mod tests {
    use super::*;

    const S: Nanos = 1_000_000_000;
    const START: Nanos = 1_000 * S;
    const GRACE: u64 = 3600;

    fn member(id: u32) -> Member {
        Member {
            id: MemberId(id),
            endpoint: NodeId(id),
            addr: String::new(),
            offer_bytes: 1,
            weight_bytes: 0,
            site: String::new(),
            storage_only: false,
            rev: 0,
            leaving: false,
            removed: false,
        }
    }

    fn secs(n: u64) -> Nanos {
        n * S
    }

    fn none() -> HashSet<MemberId> {
        HashSet::new()
    }

    #[test]
    fn a_member_heard_lately_is_up_and_goes_down_after_the_delay() {
        let l = Liveness::new(START);
        l.heard(NodeId(2), START);
        assert!(l.is_up(NodeId(2), START + DOWN_AFTER.as_nanos() as Nanos - 1));
        assert!(!l.is_up(NodeId(2), START + DOWN_AFTER.as_nanos() as Nanos));
    }

    #[test]
    fn state_follows_the_order_leaving_up_unknown_paused_lost_down() {
        let l = Liveness::new(START);
        let mut m = member(2);
        // Not heard since the start, and the start is recent: unknown.
        assert_eq!(
            l.state(&m, false, false, false, START + secs(1)),
            NodeState::Unknown
        );
        let late = START + DOWN_AFTER.as_nanos() as Nanos + secs(1);
        assert_eq!(l.state(&m, false, false, false, late), NodeState::Down);
        assert_eq!(l.state(&m, false, false, true, late), NodeState::Upgrading);
        assert_eq!(l.state(&m, false, true, false, late), NodeState::Lost);
        l.heard(NodeId(2), late);
        assert_eq!(l.state(&m, false, true, true, late), NodeState::Online);
        m.leaving = true;
        assert_eq!(l.state(&m, false, false, false, late), NodeState::Leaving);
        // This node is always up, whatever it heard of itself.
        assert_eq!(
            l.state(&member(1), true, false, false, late),
            NodeState::Online
        );
    }

    #[test]
    fn unseen_past_the_grace_is_lost_and_past_the_down_delay_is_down() {
        let l = Liveness::new(START);
        let members = [member(1), member(2)];
        l.heard(NodeId(2), START);
        let now = START + secs(GRACE);
        let v = l.verdicts(&members, MemberId(1), GRACE, 0, &none(), now);
        assert!(v.lost.contains(&MemberId(2)));
        assert!(v.down.contains(&MemberId(2)));
        assert!(!v.lost.contains(&MemberId(1)), "this node is never lost");
        let v = l.verdicts(
            &members,
            MemberId(1),
            GRACE,
            0,
            &none(),
            START + secs(GRACE - 1),
        );
        assert!(v.lost.is_empty());
        assert!(v.down.contains(&MemberId(2)));
    }

    #[test]
    fn a_paused_member_is_neither_down_nor_asleep() {
        let l = Liveness::new(START);
        let members = [member(1), member(2)];
        l.heard(NodeId(2), START);
        let paused: HashSet<_> = [MemberId(2)].into();
        let now = START + secs(2 * 3600);
        let v = l.verdicts(&members, MemberId(1), GRACE, 7, &paused, now);
        assert!(v.down.is_empty() && v.sleeping.is_empty());
    }

    #[test]
    fn asleep_members_are_remembered_for_the_retention_then_dropped() {
        let l = Liveness::new(START);
        let members = [member(1), member(2)];
        l.heard(NodeId(2), START);
        // A first look, then another once the member has been unseen past the stand-in delay.
        let t1 = START + secs(10);
        l.verdicts(&members, MemberId(1), GRACE, 7, &none(), t1);
        let t2 = START + STANDIN_AFTER.as_nanos() as Nanos - secs(1);
        let v = l.verdicts(&members, MemberId(1), GRACE, 7, &none(), t2);
        assert!(v.sleeping.is_empty());
        let t3 = START + STANDIN_AFTER.as_nanos() as Nanos + secs(10);
        let v = l.verdicts(&members, MemberId(1), GRACE, 7, &none(), t3);
        assert!(v.sleeping.contains(&MemberId(2)));
        assert!(v.kept.contains(&MemberId(2)));
        assert!(l.standin_secs_left(MemberId(2), 7, t3).is_some());
        // It is back, and a week later the extra copies are no longer kept for it.
        let after = t3 + secs(8 * 24 * 3600);
        l.heard(NodeId(2), after);
        let v = l.verdicts(&members, MemberId(1), GRACE, 7, &none(), after);
        assert!(v.kept.is_empty() && l.standin_secs_left(MemberId(2), 7, after).is_none());
    }

    #[test]
    fn after_a_long_gap_nobody_is_taken_for_asleep_until_hellos_return() {
        let l = Liveness::new(START);
        let members = [member(1), member(2)];
        l.heard(NodeId(2), START);
        l.verdicts(&members, MemberId(1), GRACE, 7, &none(), START + secs(10));
        // This node was suspended: the next look comes long after the last.
        let woke = START + secs(3 * 3600);
        let v = l.verdicts(&members, MemberId(1), GRACE, 7, &none(), woke);
        assert!(v.sleeping.is_empty(), "settling after our own sleep");
    }

    #[test]
    fn a_restarted_daemon_relearns_how_long_a_member_has_been_gone_from_its_peers() {
        let l = Liveness::new(START + secs(7200));
        let members = [member(1), member(2), member(3)];
        let now = START + secs(7200) + secs(5);
        // Peer 3 says it saw member 2 an hour and a half ago.
        let health = Health {
            seen: vec![Sighting {
                member: MemberId(2),
                secs_ago: 5400,
            }],
            ..Health::default()
        };
        l.record_health(NodeId(3), health, Some(MemberId(1)), now);
        let v = l.verdicts(&members, MemberId(1), GRACE, 0, &none(), now);
        assert!(
            v.lost.contains(&MemberId(2)),
            "lost by what the peer relayed"
        );
        assert!(!v.lost.contains(&MemberId(3)) || l.is_up(NodeId(3), now));
        // What it passes on never makes a member look more recently seen than relayed.
        let s = l.sightings(&members, Some(MemberId(1)), now);
        let two = s.iter().find(|x| x.member == MemberId(2)).unwrap();
        assert!(two.secs_ago >= 5400);
    }

    #[test]
    fn what_a_peer_gossiped_of_sleep_never_names_this_node() {
        let l = Liveness::new(START);
        let health = Health {
            slept: vec![
                Sighting {
                    member: MemberId(1),
                    secs_ago: 10,
                },
                Sighting {
                    member: MemberId(2),
                    secs_ago: 10,
                },
            ],
            ..Health::default()
        };
        l.record_health(NodeId(3), health, Some(MemberId(1)), START + secs(100));
        let slept = l.slept_sightings(START + secs(100));
        assert_eq!(slept.len(), 1);
        assert_eq!(slept[0].member, MemberId(2));
    }
}
