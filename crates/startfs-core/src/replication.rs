//! Background replication: gets every chunk to the nodes placement chose for
//! it, and tracks how many distinct nodes hold each chunk.
//!
//! A node works on the chunks in its staging area (written here, not yet on
//! their replica nodes). For each it asks the chosen nodes whether they hold
//! the chunk, pushes the bytes to those that lack it, and releases the staged
//! copy once every chosen node confirms. Nodes only learn who holds what from
//! these answers, so a node tracks the chunks it is working on and nothing else.

use crate::activity::Activity;
use crate::chunkstore::ChunkId;
use crate::control::{Repair, Shortfall, Tier};
use crate::fs::LocalFs;
use crate::members::{Member, MemberId};
use crate::net::{NodeId, Transport};
use crate::ops::Nanos;
use crate::placement::{is_steward, place};
use crate::protocol::{Message, Replica};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long to wait for a peer's answer before asking again.
pub const RETRY: Duration = Duration::from_secs(5);

/// The longest wait between asks to a peer that has answered nothing for a long time (#91): a
/// frozen or unreachable peer is asked less and less often, not every [`RETRY`] for as long as
/// it stays silent.
pub const MAX_RETRY: Duration = Duration::from_secs(60);

/// Fewest reachable copies a chunk is left with: below this, repair does not
/// wait out the grace period for a member to return.
const MIN_COPIES: u32 = 2;
/// Chunk bytes the transport may hold for one peer before more are withheld (#91): the chunk
/// is pushed on a later tick, once the peer has taken what is queued.
pub const BACKLOG_LIMIT: u64 = 32 << 20;

/// The most chunks one [`Replicator::poll`] works. A placement change makes all of them due
/// (about 21,000 on a live node, a ~1 s round on a throttled CPU), which would hold up the
/// receive and gossip phases of that tick.
const POLL_BATCH: usize = 1024;

/// How long a chunk held back by the send backlog or the repair rate waits before it is
/// tried again, so it does not take a place in every round from chunks that can proceed.
const BLOCKED_WAIT: Duration = Duration::from_secs(1);

/// How often the staged chunks are checked for ones no file refers to any more, at least. Each
/// check reads every inode, so the wait grows to [`SWEEP_SHARE`] times what the last one cost.
const SWEEP: Duration = Duration::from_secs(2);

/// A sweep may take one part in this many of the time.
const SWEEP_SHARE: u32 = 20;

/// Most asks (pushes and probes) a peer may have unanswered at once. Past it the chunks wait
/// without being visited, so a peer that answers slowly is not buried and a round does not
/// cost more the longer the queue is.
const ASKS_IN_FLIGHT: u64 = 512;

/// The most real time one round spends visiting chunks; the rest are visited by the next rounds,
/// so the receive and gossip phases of a tick stay timely however long the queue is.
const WORK_BUDGET: Duration = Duration::from_millis(10);

/// What the replicator needs to know about the node and filesystem right now.
pub struct View<'a> {
    pub me: MemberId,
    /// N.
    pub copies: u32,
    pub members: &'a [Member],
    /// Members unseen past the repair grace period: never chosen to hold copies.
    pub lost: &'a HashSet<MemberId>,
    /// Members unseen long enough to count as down (a superset of `lost`):
    /// they may come back, but their copies cannot be relied on for now.
    pub down: &'a HashSet<MemberId>,
    /// Members unseen long enough to be taken for asleep (a subset of `down`
    /// once the stand-in delay is past): other members cover for them with
    /// stand-in copies, up to N. Empty when stand-ins are off.
    pub sleeping: &'a HashSet<MemberId>,
    /// Members seen asleep within the stand-in retention (including those
    /// asleep now): copies made to cover for them are not trimmed yet.
    pub kept: &'a HashSet<MemberId>,
    /// Members that are up but behind: sent nothing new until they catch up.
    /// Unlike `full`, this is not part of the epoch, so it never starts a re-check.
    pub behind: &'a HashSet<MemberId>,
    /// When each member was last heard from (anything it sent): a member heard since it was
    /// first asked is not silent.
    pub heard: &'a HashMap<MemberId, Nanos>,
    /// Space this node offers for replicas.
    pub offer: u64,
    /// Most bytes per second sent when re-replicating chunks already held.
    pub repair_rate: u64,
    pub now: Nanos,
}

impl View<'_> {
    fn endpoint(&self, member: MemberId) -> Option<NodeId> {
        self.members
            .iter()
            .find(|m| m.id == member)
            .map(|m| m.endpoint)
    }

    fn member_at(&self, endpoint: NodeId) -> Option<MemberId> {
        self.members
            .iter()
            .find(|m| m.endpoint == endpoint && m.id != self.me)
            .map(|m| m.id)
    }
}

/// What this node has learned about one chunk it is working on.
#[derive(Default)]
struct Track {
    /// Other members known to hold the chunk.
    holders: BTreeSet<MemberId>,
    /// Members that said they lack it and would take it.
    missing: BTreeSet<MemberId>,
    /// When we last asked or pushed, per member.
    sent: HashMap<MemberId, Nanos>,
    /// Members that just said they lack the chunk and have not been sent it yet.
    answered: BTreeSet<MemberId>,
    /// Every node placement allows holds it, yet there are fewer than N.
    settled: bool,
    /// Some peer has answered about this chunk, so the holders are known.
    heard: bool,
    /// When the replicator first worked on the chunk.
    queued_at: Option<Nanos>,
    /// Nothing can happen to the chunk before this time unless a peer answers: every ask is in
    /// flight, so polling it sooner only costs time. Zero means due now.
    wake: Nanos,
}

/// What placement and trimming depend on.
#[derive(PartialEq)]
struct Epoch {
    copies: u32,
    /// Each member's (id, offer, revision).
    members: Vec<(MemberId, u64, u64)>,
    /// Members lost or down.
    unreachable: BTreeSet<MemberId>,
    /// Members lost: never chosen to hold copies.
    lost: BTreeSet<MemberId>,
    asleep: BTreeSet<MemberId>,
    /// Members whose stand-ins are kept.
    kept: BTreeSet<MemberId>,
}

#[derive(Default)]
struct State {
    tracks: BTreeMap<ChunkId, Track>,
    /// Members that turned a chunk away for lack of room; placement skips them.
    full: HashSet<MemberId>,
    /// Peers that have not answered anything since this time (when they were first asked).
    silent: HashMap<MemberId, Nanos>,
    /// The copies and membership (id, offer, revision) placement last used.
    epoch: Option<Epoch>,
    /// The members that epoch placed chunks among, to tell which chunks a change moves.
    placed: Vec<Member>,
    /// Bytes of re-replication allowed right now; goes negative after a send.
    allowance: i64,
    allowance_at: Option<Nanos>,
    /// Whether staging has been listed since the last epoch change. After
    /// that, new chunks are learned of as they are staged.
    scanned: bool,
    /// Asks each peer has unanswered, counted at the start of the round and kept up to date as
    /// the round sends more.
    asks: HashMap<MemberId, u64>,
    /// When staged chunks were last checked against the files that refer to them, and how long
    /// to wait before the next check.
    swept: Option<(Nanos, Nanos)>,
}

/// A replica this node lost to corruption and is fetching afresh.
#[derive(Default)]
struct Heal {
    asked: Option<Nanos>,
    /// Peers asked so far; the next one is asked after a retry delay.
    tried: usize,
}

pub struct Replicator {
    fs: Arc<LocalFs>,
    activity: Arc<Activity>,
    state: Mutex<State>,
    heal: Mutex<BTreeMap<ChunkId, Heal>>,
}

/// How long to wait before asking `peer` again: [`RETRY`], growing to half the time it has been
/// silent, up to [`MAX_RETRY`].
fn retry_delay(silent: &HashMap<MemberId, Nanos>, view: &View<'_>, peer: MemberId) -> Nanos {
    let since = silent.get(&peer).map(|at| match view.heard.get(&peer) {
        Some(heard) => *at.max(heard),
        None => *at,
    });
    let silence = since.map_or(0, |at| view.now.saturating_sub(at));
    (silence / 2).clamp(RETRY.as_nanos() as u64, MAX_RETRY.as_nanos() as u64)
}

/// How many stand-in copies a chunk needs on top of its targets, given who is known to hold it
/// and whether this node does. A down member may return, so its copy is only replaced once the
/// grace period ends, unless that leaves too few reachable copies; a member that is only asleep
/// is covered for up to N copies, so the chunk does not go degraded for the night.
fn standins_needed(
    view: &View<'_>,
    targets: &[MemberId],
    holders: &BTreeSet<MemberId>,
    held: bool,
) -> u32 {
    let reachable =
        holders.iter().filter(|h| !view.down.contains(*h)).count() as u32 + u32::from(held);
    let coming = targets
        .iter()
        .filter(|t| **t != view.me && !view.down.contains(*t) && !holders.contains(*t))
        .count() as u32;
    let needed = view
        .copies
        .min(MIN_COPIES)
        .saturating_sub(reachable + coming);
    let awake = holders
        .iter()
        .filter(|h| !view.sleeping.contains(*h))
        .count() as u32
        + u32::from(held);
    let awake_coming = targets
        .iter()
        .filter(|t| **t != view.me && !view.sleeping.contains(*t) && !holders.contains(*t))
        .count() as u32;
    let asleep = targets
        .iter()
        .chain(holders)
        .filter(|m| view.sleeping.contains(*m))
        .collect::<HashSet<_>>()
        .len() as u32;
    needed.max(asleep.min(view.copies.saturating_sub(awake + awake_coming)))
}

/// Whether a chunk held here is only waiting for peers to say who holds it: an ask went out
/// less than a retry delay ago and has not been answered by a member that could. Such a chunk
/// is being checked, not short of copies: a restart or a change of membership asks about many
/// chunks at once and the answers take a while.
fn awaiting(track: &Track, view: &View<'_>, silent: &HashMap<MemberId, Nanos>) -> bool {
    track.sent.iter().any(|(m, at)| {
        // A peer silent for more than a couple of retries is not slow to answer, it is not
        // answering: the chunk is short of copies, not being checked.
        retry_delay(silent, view, *m) <= RETRY.as_nanos() as u64
            && !track.holders.contains(m)
            && !track.answered.contains(m)
            && !view.down.contains(m)
            && !view.lost.contains(m)
            && view.now.saturating_sub(*at) < RETRY.as_nanos() as u64
    })
}

pub fn tier(copies: u32) -> Tier {
    if copies <= 1 {
        Tier::AtRisk
    } else {
        Tier::Degraded
    }
}

impl Replicator {
    pub fn new(fs: Arc<LocalFs>, activity: Arc<Activity>) -> Self {
        Self {
            fs,
            activity,
            state: Mutex::new(State::default()),
            heal: Mutex::new(BTreeMap::new()),
        }
    }

    /// This node's replica of `id` is gone (found corrupt and deleted): get a
    /// fresh copy from another replica holder.
    pub fn heal(&self, id: ChunkId) {
        self.heal.lock().unwrap().entry(id).or_default();
    }

    /// Chunks this node is still re-fetching.
    pub fn healing(&self) -> u32 {
        self.heal.lock().unwrap().len() as u32
    }

    /// A peer answered a request for a chunk being healed; returns whether
    /// the answer was for that (and so is consumed here).
    pub fn on_heal_reply(&self, view: &View<'_>, id: &ChunkId, data: Option<&[u8]>) -> bool {
        let mut heal = self.heal.lock().unwrap();
        let Some(h) = heal.get_mut(id) else {
            return false;
        };
        match data {
            Some(bytes)
                if blake3::hash(bytes).as_bytes() == id
                    && self.fs.store_replica(bytes, view.offer).ok() == Some(Replica::Stored) =>
            {
                heal.remove(id);
            }
            // Not held there, or not genuine: try the next peer at once.
            _ => h.asked = None,
        }
        true
    }

    fn poll_heal(&self, view: &View<'_>, transport: &dyn Transport) -> bool {
        let mut busy = false;
        let peers: Vec<NodeId> = view
            .members
            .iter()
            .filter(|m| m.id != view.me && !view.lost.contains(&m.id))
            .map(|m| m.endpoint)
            .collect();
        if peers.is_empty() {
            return false;
        }
        for (id, h) in self.heal.lock().unwrap().iter_mut() {
            let due = h
                .asked
                .is_none_or(|at| view.now.saturating_sub(at) >= RETRY.as_nanos() as u64);
            if due {
                let peer = peers[h.tried % peers.len()];
                if h.asked.is_some() {
                    self.activity.resent_after_timeout(peer);
                }
                transport.send(peer, Message::ChunkRequest(*id).encode());
                h.tried += 1;
                h.asked = Some(view.now);
                busy = true;
            }
        }
        busy
    }

    /// Start working on every chunk written here that is not yet tracked.
    fn track_staged(&self, st: &mut State) {
        // Listing the staging area costs disk reads: list it once, then follow additions.
        let added = self.fs.newly_staged();
        if st.scanned {
            for id in added {
                st.tracks.entry(id).or_default();
            }
            return;
        }
        st.scanned = true;
        for id in self.fs.staged().unwrap_or_default() {
            st.tracks.entry(id).or_default();
        }
    }

    /// Drop the staged chunks no file refers to any more (a build's temporary files, an
    /// overwritten file): pushing them to peers only for the garbage collector to remove them
    /// there is wasted work. Like the collector, this counts only the chunks the visible tree
    /// refers to, so the retention that keeps a deleted file recoverable does not apply to a
    /// chunk that never left staging.
    fn sweep_unreferenced(&self, st: &mut State, now: Nanos) {
        if st
            .swept
            .is_some_and(|(at, wait)| now.saturating_sub(at) < wait)
        {
            return;
        }
        // Only chunks nobody has been asked about yet: once a push is out, it is not recalled.
        let unsent: Vec<ChunkId> = st
            .tracks
            .iter()
            .filter(|(_, t)| t.sent.is_empty() && !t.settled)
            .map(|(id, _)| *id)
            .collect();
        if unsent.is_empty() {
            return;
        }
        let started = std::time::Instant::now();
        for id in self.fs.drop_unreferenced(&unsent).unwrap_or_default() {
            if !self.fs.has_replica(&id) {
                st.tracks.remove(&id);
            }
        }
        let wait = (started.elapsed() * SWEEP_SHARE).max(SWEEP);
        st.swept = Some((now, wait.as_nanos() as u64));
    }

    /// Membership, offers or reachability changed: check again the chunks the change touches.
    /// What was learned about the chunks being worked on is kept; of the others (held and
    /// complete) only those whose targets moved, or that need stand-ins or a hand-off now, are
    /// tracked again, so a member that restarts or flaps does not make every node ask about
    /// every chunk it holds.
    fn replan(&self, st: &mut State, view: &View<'_>, epoch: Epoch) {
        let old = st.epoch.replace(epoch);
        let old_members = std::mem::replace(&mut st.placed, view.members.to_vec());
        let old_full = std::mem::take(&mut st.full);
        st.scanned = false;
        let new = st.epoch.as_ref().expect("just set");
        let Some(old) = old.filter(|old| old.copies == new.copies) else {
            // The first look, or N changed: every chunk is checked against placement.
            st.tracks.clear();
            for id in self.fs.held().unwrap_or_default() {
                st.tracks.entry(id).or_default();
            }
            return;
        };
        // What the old view made of the answers (who would take the chunk, who was asked) does
        // not carry over: those chunks are asked again. Who holds a chunk does, unless the
        // member is down or lost, whose copy is not relied on.
        for track in st.tracks.values_mut() {
            track.holders.retain(|h| !new.unreachable.contains(h));
            track.missing.clear();
            track.answered.clear();
            track.sent.clear();
            track.settled = false;
            track.wake = 0;
        }
        let old_skip: HashSet<MemberId> = old_full
            .into_iter()
            .chain(old.lost.iter().copied())
            .collect();
        let new_skip = view.lost.clone();
        for id in self.fs.held().unwrap_or_default() {
            if st.tracks.contains_key(&id) {
                continue;
            }
            let was = place(&id, &old_members, old.copies, &old_skip);
            let now = place(&id, view.members, view.copies, &new_skip);
            let holders: BTreeSet<MemberId> =
                now.iter().copied().filter(|m| *m != view.me).collect();
            if was != now
                || !now.contains(&view.me)
                || standins_needed(view, &now, &holders, true) > 0
            {
                st.tracks.insert(id, Track::default());
            }
        }
    }

    /// One round: do whatever is due. Returns whether anything was sent.
    pub fn poll(&self, view: &View<'_>, transport: &dyn Transport) -> bool {
        let mut busy = false;
        let mut st = self.state.lock().unwrap();
        let epoch = Epoch {
            copies: view.copies,
            members: view
                .members
                .iter()
                .map(|m| (m.id, m.offer_bytes, m.rev))
                .collect(),
            unreachable: view.lost.iter().chain(view.down).copied().collect(),
            lost: view.lost.iter().copied().collect(),
            asleep: view.sleeping.iter().copied().collect(),
            kept: view.kept.iter().copied().collect(),
        };
        if st.epoch.as_ref() != Some(&epoch) {
            self.replan(&mut st, view, epoch);
        }
        self.track_staged(&mut st);
        self.sweep_unreferenced(&mut st, view.now);
        // Earn this round's share of the repair rate (at most one second's worth).
        let earned = st.allowance_at.map_or(i64::MAX, |at| {
            (view.now.saturating_sub(at) as u128 * view.repair_rate as u128 / 1_000_000_000)
                .min(i64::MAX as u128) as i64
        });
        let cap = view.repair_rate.min(i64::MAX as u64) as i64;
        st.allowance = st.allowance.saturating_add(earned).min(cap);
        st.allowance_at = Some(view.now);
        // The least replicated chunks first, so the most at risk are made safe first.
        // Chunks that are settled have nothing left to do until placement changes, and chunks
        // waiting for answers have nothing to do until a retry is due or an answer arrives.
        // A placement change makes every chunk due at once: work only the most at risk of them
        // each round, so one round stays short and the rest follow over the next ones.
        // One pass over the tracks collects what is due and how many asks each peer has
        // unanswered; no disk is touched for chunks that are not visited.
        let mut due: Vec<(usize, ChunkId)> = Vec::new();
        let mut asks: HashMap<MemberId, u64> = HashMap::new();
        for (id, t) in st.tracks.iter().filter(|(_, t)| !t.settled) {
            for m in t.sent.keys() {
                *asks.entry(*m).or_default() += 1;
            }
            if t.wake <= view.now {
                due.push((t.holders.len(), *id));
            }
        }
        st.asks = asks;
        if due.len() > POLL_BATCH {
            due.select_nth_unstable_by_key(POLL_BATCH, |(known, _)| *known);
            due.truncate(POLL_BATCH);
        }
        // Of the chunks taken, the least replicated first (counting a copy held here).
        for entry in &mut due {
            entry.0 += usize::from(self.fs.has_replica(&entry.1));
        }
        due.sort_unstable_by_key(|(replicas, _)| *replicas);
        let started = std::time::Instant::now();
        for (_, id) in due {
            if started.elapsed() > WORK_BUDGET {
                break;
            }
            busy |= self.work(&mut st, view, transport, &id);
        }
        drop(st);
        busy | self.poll_heal(view, transport)
    }

    /// When the replicator next has something to do without a peer's answer arriving: zero if it
    /// is due now, `None` if there is nothing to wait for.
    pub fn next_wake(&self) -> Option<Nanos> {
        let tracks = self
            .state
            .lock()
            .unwrap()
            .tracks
            .values()
            .filter(|t| !t.settled)
            .map(|t| t.wake)
            .min();
        // A heal that has not asked yet is due now; one that has asked retries after RETRY.
        let heals = self
            .heal
            .lock()
            .unwrap()
            .values()
            .map(|h| h.asked.map_or(0, |at| at + RETRY.as_nanos() as u64))
            .min();
        tracks.into_iter().chain(heals).min()
    }

    fn work(
        &self,
        st: &mut State,
        view: &View<'_>,
        transport: &dyn Transport,
        id: &ChunkId,
    ) -> bool {
        let mut busy = false;
        if st.tracks[id].settled {
            return false;
        }
        let staged = self.fs.has_staged(id);
        if !staged && !self.fs.has_replica(id) {
            st.tracks.remove(id);
            return false;
        }
        let skip: HashSet<MemberId> = st.full.union(view.lost).copied().collect();
        let mut targets = place(id, view.members, view.copies, &skip);
        if staged
            && targets.contains(&view.me)
            && !self.fs.has_replica(id)
            && self.fs.promote(id, view.offer).ok() != Some(Replica::Stored)
        {
            // No room here: choose again without this node.
            st.full.insert(view.me);
            return true;
        }
        let track = st.tracks.get_mut(id).expect("tracked");
        track.queued_at.get_or_insert(view.now);
        let held = self.fs.has_replica(id);
        let needed = standins_needed(view, &targets, &track.holders, held);
        if needed > 0 {
            let out: HashSet<MemberId> = skip
                .union(view.down)
                .chain(view.sleeping)
                .copied()
                .collect();
            let stand_ins: Vec<MemberId> = place(id, view.members, view.members.len() as u32, &out)
                .into_iter()
                .filter(|m| *m != view.me && !track.holders.contains(m) && !targets.contains(m))
                .take(needed as usize)
                .collect();
            targets.extend(stand_ins);
        }
        // Of several holders, only one (picked per chunk by hash, so the work
        // spreads) pushes to targets that lack the chunk, so a new target is
        // not sent it more than once.
        let steward = staged
            || !targets.contains(&view.me)
            || is_steward(id, view.me, track.holders.iter().copied());
        // Whether the chunk may do more than wait for answers or retries.
        let mut blocked = false;
        for t in &targets {
            if *t == view.me || track.holders.contains(t) {
                continue;
            }
            // A target that just said it lacks the chunk is sent it at once by
            // the steward; everyone else asks again only after a retry delay.
            let retry = retry_delay(&st.silent, view, *t);
            let due = (steward && track.answered.contains(t))
                || track
                    .sent
                    .get(t)
                    .is_none_or(|at| view.now.saturating_sub(*at) >= retry);
            let Some(to) = view.endpoint(*t) else {
                blocked = true;
                continue;
            };
            if view.behind.contains(t) {
                blocked = true;
                continue;
            }
            if !due {
                continue;
            }
            if st.asks.get(t).copied().unwrap_or(0) >= ASKS_IN_FLIGHT && !track.sent.contains_key(t)
            {
                blocked = true;
                continue;
            }
            let timed_out = !track.answered.contains(t)
                && track
                    .sent
                    .get(t)
                    .is_some_and(|at| view.now.saturating_sub(*at) >= retry);
            let mut data = None;
            if steward && track.missing.contains(t) {
                if transport.backlog(to) >= BACKLOG_LIMIT {
                    blocked = true;
                    continue;
                }
                data = self.fs.local_chunk(id);
                if let Some(bytes) = &data
                    && !staged
                {
                    // Chunks already held are re-replicated within the rate limit.
                    if st.allowance < 0 {
                        blocked = true;
                        continue;
                    }
                    st.allowance -= bytes.len() as i64;
                }
            }
            if let Some(bytes) = &data {
                self.activity.chunk_replicated(bytes.len());
            }
            if timed_out {
                self.activity.resent_after_timeout(to);
            }
            transport.send(to, Message::ChunkPush(*id, data).encode());
            if track.sent.insert(*t, view.now).is_none() {
                *st.asks.entry(*t).or_default() += 1;
            }
            track.answered.remove(t);
            st.silent.entry(*t).or_insert(view.now);
            busy = true;
        }
        // Everything asked is in flight: sleep until the first retry is due (an answer wakes it).
        if blocked {
            track.wake = view.now.saturating_add(BLOCKED_WAIT.as_nanos() as u64);
        } else {
            track.wake = targets
                .iter()
                .filter(|t| **t != view.me && !track.holders.contains(*t))
                .filter_map(|t| {
                    let at = track.sent.get(t)?;
                    Some(at.saturating_add(retry_delay(&st.silent, view, *t)))
                })
                .min()
                .unwrap_or(0);
        }
        let complete = !targets.is_empty()
            && targets
                .iter()
                .all(|t| *t == view.me && held || track.holders.contains(t));
        // Never let go of a copy while no replica exists elsewhere or here.
        if complete && track.holders.len() + usize::from(held) > 0 {
            // Every node placement allows is accounted for, even if none was asked.
            track.heard = true;
            self.fs.release(id);
            // Placement no longer asks this node to hold it: hand-off done,
            // unless it is a stand-in for a member that slept lately and may
            // sleep again.
            if held && !targets.contains(&view.me) && !targets.iter().any(|t| view.kept.contains(t))
            {
                self.fs.drop_replica(id);
            }
            busy = true;
            let replicas = track.holders.len() as u32 + u32::from(self.fs.has_replica(id));
            // With fewer copies than wanted the chunk stays listed as
            // degraded: everything placement allows has been done.
            if replicas >= view.copies {
                st.tracks.remove(id);
            } else {
                track.settled = true;
            }
        }
        busy
    }

    /// A peer asks us to hold a chunk (or asks whether we do).
    pub fn on_push(&self, view: &View<'_>, id: &ChunkId, data: Option<&[u8]>) -> Replica {
        match data {
            None => self.fs.replica_standing(id, view.offer),
            Some(bytes) if blake3::hash(bytes).as_bytes() == id => self
                .fs
                .store_replica(bytes, view.offer)
                .unwrap_or(Replica::Full),
            Some(_) => Replica::Full,
        }
    }

    /// A peer answered one of our pushes.
    pub fn on_ack(&self, view: &View<'_>, from: NodeId, id: &ChunkId, standing: Replica) {
        let Some(member) = view.member_at(from) else {
            return;
        };
        self.activity.acknowledged(from);
        let mut st = self.state.lock().unwrap();
        st.silent.remove(&member);
        if standing == Replica::Full {
            st.full.insert(member);
        }
        let Some(track) = st.tracks.get_mut(id) else {
            return;
        };
        track.heard = true;
        track.wake = 0;
        match standing {
            Replica::Stored => {
                track.sent.remove(&member);
                track.answered.remove(&member);
                track.holders.insert(member);
                track.missing.remove(&member);
            }
            Replica::Missing => {
                track.sent.insert(member, view.now);
                track.answered.insert(member);
                track.missing.insert(member);
            }
            Replica::Full => {
                track.sent.remove(&member);
            }
        }
    }

    /// The queue as it is now: chunks being worked on, asks nobody has
    /// answered per member, and the age in seconds of the chunk that has been
    /// waiting longest.
    pub fn queue(&self, now: Nanos) -> (u64, HashMap<MemberId, u64>, Option<u64>) {
        let st = self.state.lock().unwrap();
        let mut in_flight: HashMap<MemberId, u64> = HashMap::new();
        let mut oldest: Option<Nanos> = None;
        for t in st.tracks.values().filter(|t| !t.settled) {
            for member in t.sent.keys() {
                *in_flight.entry(*member).or_default() += 1;
            }
            if let Some(at) = t.queued_at {
                oldest = Some(oldest.map_or(at, |o| o.min(at)));
            }
        }
        let chunks = st.tracks.values().filter(|t| !t.settled).count() as u64;
        let age = oldest.map(|at| now.saturating_sub(at) / 1_000_000_000);
        (chunks, in_flight, age)
    }

    /// Chunks the replicator tracks, settled or not.
    pub fn tracked(&self) -> u64 {
        self.state.lock().unwrap().tracks.len() as u64
    }

    /// Members known to be out of room.
    pub fn full_members(&self) -> HashSet<MemberId> {
        self.state.lock().unwrap().full.clone()
    }

    /// Chunks held here that this node must re-replicate, by how safe they are.
    pub fn repair_progress(&self, view: &View<'_>) -> Option<Repair> {
        let st = self.state.lock().unwrap();
        let mut progress = Repair {
            at_risk: 0,
            degraded: 0,
        };
        for (id, t) in &st.tracks {
            let replicas = t.holders.len() as u32 + 1;
            let mine = is_steward(id, view.me, t.holders.iter().copied());
            if t.heard
                && !t.settled
                && mine
                && replicas < view.copies
                && self.fs.has_replica(id)
                && !awaiting(t, view, &st.silent)
            {
                match tier(replicas) {
                    Tier::AtRisk => progress.at_risk += 1,
                    _ => progress.degraded += 1,
                }
            }
        }
        (progress.at_risk + progress.degraded > 0).then_some(progress)
    }

    /// Chunks held here that are only waiting for answers about who else holds them.
    pub fn rechecking(&self, view: &View<'_>) -> u64 {
        let st = self.state.lock().unwrap();
        st.tracks
            .iter()
            .filter(|(id, t)| {
                !t.settled
                    && !self.fs.has_staged(id)
                    && self.fs.has_replica(id)
                    && awaiting(t, view, &st.silent)
            })
            .count() as u64
    }

    /// The member most staged chunks are waiting on (asked, not yet holding the chunk), and how
    /// many, so a backlog can be blamed on the slow member behind it.
    pub fn staged_waiting_on(&self) -> Option<(MemberId, u64)> {
        let st = self.state.lock().unwrap();
        let mut by_member: HashMap<MemberId, u64> = HashMap::new();
        for (id, t) in st.tracks.iter().filter(|(_, t)| !t.settled) {
            if t.sent.is_empty() || !self.fs.has_staged(id) {
                continue;
            }
            for m in t.sent.keys().filter(|m| !t.holders.contains(m)) {
                *by_member.entry(*m).or_default() += 1;
            }
        }
        by_member.into_iter().max_by_key(|(m, n)| (*n, *m))
    }

    /// Chunks on fewer than `view.copies` nodes, as far as this node knows,
    /// counted by their copies. Each chunk is counted by one member (once its
    /// holders agree who), so the counts of all members add up.
    pub fn unreplicated(&self, view: &View<'_>) -> Vec<Shortfall> {
        let mut st = self.state.lock().unwrap();
        self.track_staged(&mut st);
        let mut by_copies: BTreeMap<u32, u32> = BTreeMap::new();
        for (id, t) in &st.tracks {
            let held = self.fs.has_replica(id);
            // A held chunk is only reported once a peer has said how many copies exist.
            if !(t.heard || self.fs.has_staged(id)) {
                continue;
            }
            if !self.fs.has_staged(id) && awaiting(t, view, &st.silent) {
                continue;
            }
            let replicas = t.holders.len() as u32 + u32::from(held);
            // A staged copy only counts while it is the sole copy.
            let copies = if replicas == 0 && self.fs.has_staged(id) {
                1
            } else {
                replicas
            };
            // Holders share the counting by hash. A node that wrote the chunk
            // and does not hold it counts it alone, since the holders it
            // reached may not be tracking it. Around a membership change both
            // can count it, for a moment.
            let mine = !held || is_steward(id, view.me, t.holders.iter().copied());
            if copies < view.copies && mine {
                *by_copies.entry(copies).or_default() += 1;
            }
        }
        by_copies
            .into_iter()
            .map(|(copies, chunks)| Shortfall { copies, chunks })
            .collect()
    }
}
