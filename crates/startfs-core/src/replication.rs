//! Background replication: gets every chunk to the nodes placement chose for
//! it, and tracks how many distinct nodes hold each chunk.
//!
//! A node works on the chunks in its staging area (written here, not yet on
//! their replica nodes). For each it asks the chosen nodes whether they hold
//! the chunk, pushes the bytes to those that lack it, and releases the staged
//! copy once every chosen node confirms. Nodes only learn who holds what from
//! these answers, so a node tracks the chunks it is working on and nothing else.

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

/// Fewest reachable copies a chunk is left with: below this, repair does not
/// wait out the grace period for a member to return.
const MIN_COPIES: u32 = 2;

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
}

/// N and each member's (id, offer, revision): what placement depends on.
type Epoch = (u32, Vec<(MemberId, u64, u64)>, Vec<MemberId>);

#[derive(Default)]
struct State {
    tracks: BTreeMap<ChunkId, Track>,
    /// Members that turned a chunk away for lack of room; placement skips them.
    full: HashSet<MemberId>,
    /// The copies and membership (id, offer, revision) placement last used.
    epoch: Option<Epoch>,
    /// Bytes of re-replication allowed right now; goes negative after a send.
    allowance: i64,
    allowance_at: Option<Nanos>,
    /// The staging generation last scanned for new chunks to track.
    scanned: Option<u64>,
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
    state: Mutex<State>,
    heal: Mutex<BTreeMap<ChunkId, Heal>>,
}

pub fn tier(copies: u32) -> Tier {
    if copies <= 1 {
        Tier::AtRisk
    } else {
        Tier::Degraded
    }
}

impl Replicator {
    pub fn new(fs: Arc<LocalFs>) -> Self {
        Self {
            fs,
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
                transport.send(
                    peers[h.tried % peers.len()],
                    Message::ChunkRequest(*id).encode(),
                );
                h.tried += 1;
                h.asked = Some(view.now);
                busy = true;
            }
        }
        busy
    }

    /// Start working on every chunk written here that is not yet tracked.
    fn track_staged(&self, st: &mut State) {
        // Listing the staging area costs disk reads: only look again when it changed.
        let generation = self.fs.staging_generation();
        if st.scanned == Some(generation) {
            return;
        }
        st.scanned = Some(generation);
        for id in self.fs.staged().unwrap_or_default() {
            st.tracks.entry(id).or_default();
        }
    }

    /// One round: do whatever is due. Returns whether anything was sent.
    pub fn poll(&self, view: &View<'_>, transport: &dyn Transport) -> bool {
        let mut busy = false;
        let mut st = self.state.lock().unwrap();
        let epoch = (
            view.copies,
            view.members
                .iter()
                .map(|m| (m.id, m.offer_bytes, m.rev))
                .collect::<Vec<_>>(),
            view.lost
                .iter()
                .chain(view.down)
                .copied()
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect(),
        );
        if st.epoch.as_ref() != Some(&epoch) {
            // Membership or offers changed: placement may differ, so check
            // every chunk held here against it.
            st.epoch = Some(epoch);
            st.full.clear();
            st.tracks.clear();
            st.scanned = None;
            for id in self.fs.held().unwrap_or_default() {
                st.tracks.entry(id).or_default();
            }
        }
        self.track_staged(&mut st);
        // Earn this round's share of the repair rate (at most one second's worth).
        let earned = st.allowance_at.map_or(i64::MAX, |at| {
            (view.now.saturating_sub(at) as u128 * view.repair_rate as u128 / 1_000_000_000)
                .min(i64::MAX as u128) as i64
        });
        let cap = view.repair_rate.min(i64::MAX as u64) as i64;
        st.allowance = st.allowance.saturating_add(earned).min(cap);
        st.allowance_at = Some(view.now);
        // The least replicated chunks first, so the most at risk are made safe first.
        // Chunks that are settled have nothing left to do until placement changes.
        let mut ids: Vec<ChunkId> = st
            .tracks
            .iter()
            .filter(|(_, t)| !t.settled)
            .map(|(id, _)| *id)
            .collect();
        ids.sort_by_key(|id| {
            let t = &st.tracks[id];
            t.holders.len() + usize::from(self.fs.has_replica(id))
        });
        for id in ids {
            busy |= self.work(&mut st, view, transport, &id);
        }
        drop(st);
        busy | self.poll_heal(view, transport)
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
        let held = self.fs.has_replica(id);
        // Down members may return, so their copies are only replaced once the
        // grace period ends. Unless that leaves too few reachable copies: then
        // stand-ins on other members bring it back to the minimum now.
        let reachable = track
            .holders
            .iter()
            .filter(|h| !view.down.contains(*h))
            .count() as u32
            + u32::from(held);
        let coming = targets
            .iter()
            .filter(|t| **t != view.me && !view.down.contains(*t) && !track.holders.contains(*t))
            .count() as u32;
        let needed = view
            .copies
            .min(MIN_COPIES)
            .saturating_sub(reachable + coming);
        if needed > 0 {
            let out: HashSet<MemberId> = skip.union(view.down).copied().collect();
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
        for t in &targets {
            if *t == view.me || track.holders.contains(t) {
                continue;
            }
            // A target that just said it lacks the chunk is sent it at once by
            // the steward; everyone else asks again only after a retry delay.
            let due = (steward && track.answered.contains(t))
                || track
                    .sent
                    .get(t)
                    .is_none_or(|at| view.now.saturating_sub(*at) >= RETRY.as_nanos() as u64);
            let Some(to) = view.endpoint(*t) else {
                continue;
            };
            if !due {
                continue;
            }
            let mut data = None;
            if steward && track.missing.contains(t) {
                data = self.fs.local_chunk(id);
                if let Some(bytes) = &data
                    && !staged
                {
                    // Chunks already held are re-replicated within the rate limit.
                    if st.allowance < 0 {
                        continue;
                    }
                    st.allowance -= bytes.len() as i64;
                }
            }
            transport.send(to, Message::ChunkPush(*id, data).encode());
            track.sent.insert(*t, view.now);
            track.answered.remove(t);
            busy = true;
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
            // Placement no longer asks this node to hold it: hand-off done.
            if held && !targets.contains(&view.me) {
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
        let mut st = self.state.lock().unwrap();
        if standing == Replica::Full {
            st.full.insert(member);
        }
        let Some(track) = st.tracks.get_mut(id) else {
            return;
        };
        track.heard = true;
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
            if t.heard && !t.settled && mine && replicas < view.copies && self.fs.has_replica(id) {
                match tier(replicas) {
                    Tier::AtRisk => progress.at_risk += 1,
                    _ => progress.degraded += 1,
                }
            }
        }
        (progress.at_risk + progress.degraded > 0).then_some(progress)
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
