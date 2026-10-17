//! Background replication: gets every chunk to the nodes placement chose for
//! it, and tracks how many distinct nodes hold each chunk.
//!
//! A node works on the chunks in its staging area (written here, not yet on
//! their replica nodes). For each it asks the chosen nodes whether they hold
//! the chunk, pushes the bytes to those that lack it, and releases the staged
//! copy once every chosen node confirms. Nodes only learn who holds what from
//! these answers, so a node tracks the chunks it is working on and nothing else.

use crate::chunkstore::ChunkId;
use crate::control::{FileHealth, Tier};
use crate::fs::LocalFs;
use crate::members::Member;
use crate::net::{NodeId, Transport};
use crate::ops::Nanos;
use crate::placement::place;
use crate::protocol::{Message, Replica};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long to wait for a peer's answer before asking again.
pub const RETRY: Duration = Duration::from_secs(5);

/// What the replicator needs to know about the node and filesystem right now.
pub struct View<'a> {
    pub me: u32,
    /// N.
    pub copies: u32,
    pub members: &'a [Member],
    /// Space this node offers for replicas.
    pub offer: u64,
    pub now: Nanos,
}

impl View<'_> {
    fn endpoint(&self, member: u32) -> Option<NodeId> {
        self.members
            .iter()
            .find(|m| m.id == member)
            .map(|m| m.endpoint)
    }

    fn member_at(&self, endpoint: NodeId) -> Option<u32> {
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
    holders: BTreeSet<u32>,
    /// Members that said they lack it and would take it.
    missing: BTreeSet<u32>,
    /// When we last asked or pushed, per member.
    sent: HashMap<u32, Nanos>,
}

#[derive(Default)]
struct State {
    tracks: BTreeMap<ChunkId, Track>,
}

pub struct Replicator {
    fs: Arc<LocalFs>,
    state: Mutex<State>,
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
        }
    }

    /// Start working on every chunk written here that is not yet tracked.
    fn track_staged(&self, st: &mut State) {
        for id in self.fs.staged().unwrap_or_default() {
            st.tracks.entry(id).or_default();
        }
    }

    /// One round: do whatever is due. Returns whether anything was sent.
    pub fn poll(&self, view: &View<'_>, transport: &dyn Transport) -> bool {
        let mut busy = false;
        let mut st = self.state.lock().unwrap();
        self.track_staged(&mut st);
        let ids: Vec<ChunkId> = st.tracks.keys().copied().collect();
        for id in ids {
            busy |= self.work(&mut st, view, transport, &id);
        }
        busy
    }

    fn work(&self, st: &mut State, view: &View<'_>, transport: &dyn Transport, id: &ChunkId) -> bool {
        let mut busy = false;
        let staged = self.fs.has_staged(id);
        if !staged {
            st.tracks.remove(id);
            return false;
        }
        let targets = place(id, view.members, view.copies, &HashSet::new());
        let track = st.tracks.get_mut(id).expect("tracked");
        if targets.contains(&view.me)
            && !self.fs.has_replica(id)
            && self.fs.promote(id, view.offer).ok() != Some(Replica::Stored)
        {
            return false;
        }
        let have_here = self.fs.has_replica(id);
        for t in &targets {
            if *t == view.me || track.holders.contains(t) {
                continue;
            }
            let due = track
                .sent
                .get(t)
                .is_none_or(|at| view.now.saturating_sub(*at) >= RETRY.as_nanos() as u64);
            let Some(to) = view.endpoint(*t) else {
                continue;
            };
            if !due {
                continue;
            }
            let data = if track.missing.contains(t) {
                self.fs.local_chunk(id)
            } else {
                None
            };
            transport.send(to, Message::ChunkPush(*id, data).encode());
            track.sent.insert(*t, view.now);
            busy = true;
        }
        let complete = targets
            .iter()
            .all(|t| *t == view.me && have_here || track.holders.contains(t));
        if complete {
            self.fs.release(id);
            st.tracks.remove(id);
            busy = true;
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
        let Some(track) = st.tracks.get_mut(id) else {
            return;
        };
        track.sent.remove(&member);
        match standing {
            Replica::Stored => {
                track.holders.insert(member);
                track.missing.remove(&member);
            }
            Replica::Missing => {
                track.missing.insert(member);
            }
            Replica::Full => {}
        }
    }

    /// Files with a chunk on fewer than `view.copies` nodes, as far as this node knows.
    pub fn unreplicated(&self, view: &View<'_>) -> Vec<FileHealth> {
        let mut st = self.state.lock().unwrap();
        self.track_staged(&mut st);
        let copies: HashMap<ChunkId, u32> = st
            .tracks
            .iter()
            .map(|(id, t)| (*id, t.holders.len() as u32 + 1))
            .collect();
        let ids: HashSet<ChunkId> = copies.keys().copied().collect();
        let files = self.fs.files_using(&ids).unwrap_or_default();
        files
            .into_iter()
            .map(|(path, chunks)| {
                let least = chunks
                    .iter()
                    .filter_map(|c| copies.get(c).copied())
                    .min()
                    .unwrap_or(view.copies);
                FileHealth {
                    path,
                    copies: least,
                    wanted: view.copies,
                    tier: tier(least),
                }
            })
            .collect()
    }
}
