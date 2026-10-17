//! Log exchange with peers over a [`Transport`].
//!
//! Each node tells its peers what it holds (`Hello`) and pushes whatever a
//! peer lacks (`Entries`). Hellos repeat every [`HELLO_INTERVAL`], so after a
//! partition heals or a node restarts the peers catch each other up without
//! any leader or quorum.

use crate::clock::Clock;
use crate::fs::{CatchUp, LocalFs};
use crate::net::{NodeId, Transport};
use crate::oplog::Frontier;
use crate::ops::Nanos;
use crate::protocol::Message;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

pub const HELLO_INTERVAL: Duration = Duration::from_secs(5);
const BATCH: usize = 256;

#[derive(Default)]
struct Peer {
    /// What the peer is believed to hold, once it has said so.
    known: Option<Frontier>,
    /// What the peer itself last said it holds (never a guess).
    confirmed: Option<Frontier>,
    /// When `confirmed` arrived.
    confirmed_at: Option<Nanos>,
    /// When the peer last said what it holds, if we have since applied
    /// everything up to that (its own word, not a second-hand one).
    synced: Option<Nanos>,
    last_hello: Option<Nanos>,
}

pub struct Syncer {
    fs: Arc<LocalFs>,
    transport: Arc<dyn Transport>,
    clock: Arc<dyn Clock>,
    peers: BTreeMap<NodeId, Peer>,
}

impl Syncer {
    pub fn new(fs: Arc<LocalFs>, transport: Arc<dyn Transport>, clock: Arc<dyn Clock>) -> Self {
        Self {
            fs,
            transport,
            clock,
            peers: BTreeMap::new(),
        }
    }

    /// Exchange logs with exactly these peers.
    pub fn set_peers(&mut self, peers: &[NodeId]) {
        self.peers.retain(|id, _| peers.contains(id));
        for p in peers {
            self.peers.entry(*p).or_default();
        }
    }

    fn send(&self, to: NodeId, msg: &Message) {
        self.transport.send(to, msg.encode());
    }

    /// Tell `to` what we hold now, so it sends what we lack.
    fn reply_frontier(&self, to: NodeId) {
        if let Ok(f) = self.fs.frontier() {
            self.send(to, &Message::Hello(f));
        }
    }

    /// Handle a log-exchange message from `from`; ignored unless `from` is a peer.
    pub fn handle(&mut self, from: NodeId, msg: Message) {
        if !self.peers.contains_key(&from) {
            return;
        }
        match msg {
            Message::Hello(f) => {
                let peer = self.peers.get_mut(&from).expect("known peer");
                let first = peer.known.is_none();
                peer.known = Some(f.clone());
                peer.confirmed = Some(f);
                peer.confirmed_at = Some(self.clock.now());
                self.note_synced(from);
                if first && let Ok(f) = self.fs.frontier() {
                    // The peer may not have heard from us yet: say what we hold.
                    self.send(from, &Message::Hello(f));
                }
            }
            Message::State(state) => {
                if let Err(e) = self.fs.apply_state(&state) {
                    eprintln!("startfs: cannot apply state from node {}: {e}", from.0);
                }
                self.note_synced(from);
                self.reply_frontier(from);
            }
            Message::Entries(entries) => {
                if let Err(e) = self.fs.apply_remote(&entries) {
                    eprintln!("startfs: cannot apply entries from node {}: {e}", from.0);
                }
                self.note_synced(from);
                self.reply_frontier(from);
            }
            _ => {}
        }
    }

    /// Record whether we now hold everything `peer` last said it holds.
    fn note_synced(&mut self, peer: NodeId) {
        let Ok(ours) = self.fs.frontier() else {
            return;
        };
        let Some(p) = self.peers.get_mut(&peer) else {
            return;
        };
        p.synced = match (&p.confirmed, p.confirmed_at) {
            (Some(theirs), Some(at))
                if theirs
                    .iter()
                    .all(|(o, seq)| ours.get(o).is_some_and(|s| s >= seq)) =>
            {
                Some(at)
            }
            _ => None,
        };
    }

    /// For each peer, when it last reported its own frontier while we held
    /// everything up to it; `None` if we have not caught up with it since.
    pub fn synced_with(&self) -> BTreeMap<NodeId, Option<Nanos>> {
        self.peers.iter().map(|(id, p)| (*id, p.synced)).collect()
    }

    /// What every peer not in `except` is known to hold, per origin: the log
    /// below it can be dropped. `None` while any such peer has not said what
    /// it holds, or when every peer is excepted (this node is cut off).
    pub fn held_by_all(&self, except: &[NodeId]) -> Option<Frontier> {
        let mut all: Option<Frontier> = None;
        for (_, p) in self.peers.iter().filter(|(id, _)| !except.contains(id)) {
            let known = p.confirmed.as_ref()?;
            all = Some(match all {
                None => known.clone(),
                Some(mut so_far) => {
                    so_far.retain(|o, seq| {
                        *seq = (*seq).min(known.get(o).copied().unwrap_or(0));
                        *seq > 0
                    });
                    so_far
                }
            });
        }
        match (all, self.peers.is_empty()) {
            (Some(f), _) => Some(f),
            (None, true) => self.fs.frontier().ok(),
            (None, false) => None,
        }
    }

    /// Whether every peer, lost ones included, has itself said it holds exactly
    /// what we hold: nobody has anything that is yet to reach anybody.
    pub fn converged(&self) -> bool {
        let Ok(ours) = self.fs.frontier() else {
            return false;
        };
        let same = |a: &Frontier, b: &Frontier| {
            a.keys()
                .chain(b.keys())
                .all(|o| a.get(o).unwrap_or(&0) == b.get(o).unwrap_or(&0))
        };
        self.peers
            .values()
            .all(|p| p.confirmed.as_ref().is_some_and(|k| same(k, &ours)))
    }

    /// Whether every peer not in `except` is known to hold all of our log.
    pub fn peers_have_our_log(&self, except: &[NodeId]) -> bool {
        let Ok(ours) = self.fs.frontier() else {
            return false;
        };
        self.peers
            .iter()
            .filter(|(id, _)| !except.contains(id))
            .all(|(_, p)| {
                p.known.as_ref().is_some_and(|k| {
                    ours.iter()
                        .all(|(o, seq)| k.get(o).is_some_and(|s| s >= seq))
                })
            })
    }

    /// Send what is due. Returns whether anything happened.
    pub fn poll(&mut self) -> bool {
        let mut busy = false;
        let now = self.clock.now();
        let ids: Vec<NodeId> = self.peers.keys().copied().collect();
        for id in ids {
            let due = self.peers[&id]
                .last_hello
                .is_none_or(|t| now.saturating_sub(t) >= HELLO_INTERVAL.as_nanos() as u64);
            if due && let Ok(f) = self.fs.frontier() {
                self.send(id, &Message::Hello(f));
                let p = self.peers.get_mut(&id).expect("known peer");
                p.last_hello = Some(now);
                busy = true;
            }
            let Some(known) = self.peers[&id].known.clone() else {
                continue;
            };
            let entries = match self.fs.catch_up(&known, BATCH) {
                Ok(CatchUp::Entries(entries)) => entries,
                // What the peer misses was compacted away: send our state instead.
                Ok(CatchUp::State(state)) => {
                    let p = self.peers.get_mut(&id).expect("known peer");
                    p.known = Some(state.frontier.clone());
                    self.send(id, &Message::State(state));
                    busy = true;
                    continue;
                }
                Err(_) => continue,
            };
            if entries.is_empty() {
                continue;
            }
            let p = self.peers.get_mut(&id).expect("known peer");
            let known = p.known.as_mut().expect("checked above");
            for e in &entries {
                known.insert(e.origin, e.seq + 1);
            }
            self.send(id, &Message::Entries(entries));
            busy = true;
        }
        busy
    }
}
