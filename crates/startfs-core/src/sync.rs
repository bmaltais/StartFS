//! Log exchange with peers over a [`Transport`].
//!
//! Each node tells its peers what it holds (`Hello`) and pushes whatever a
//! peer lacks (`Entries`). Hellos repeat every [`HELLO_INTERVAL`], so after a
//! partition heals or a node restarts the peers catch each other up without
//! any leader or quorum.

use crate::clock::Clock;
use crate::fs::LocalFs;
use crate::net::{NodeId, Transport};
use crate::oplog::{Frontier, Message};
use crate::ops::Nanos;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

pub const HELLO_INTERVAL: Duration = Duration::from_secs(5);
const BATCH: usize = 256;

#[derive(Default)]
struct Peer {
    /// What the peer is believed to hold, once it has said so.
    known: Option<Frontier>,
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

    pub fn add_peer(&mut self, peer: NodeId) {
        self.peers.entry(peer).or_default();
    }

    fn send(&self, to: NodeId, msg: &Message) {
        self.transport.send(to, msg.encode());
    }

    /// Handle arrived messages and send what is due. Returns whether anything happened.
    pub fn tick(&mut self) -> bool {
        let mut busy = false;
        while let Some((from, raw)) = self.transport.recv() {
            busy = true;
            if !self.peers.contains_key(&from) {
                continue;
            }
            match Message::decode(&raw) {
                Some(Message::Hello(f)) => {
                    self.peers.get_mut(&from).expect("known peer").known = Some(f);
                }
                Some(Message::Entries(entries)) => {
                    let _ = self.fs.apply_remote(&entries);
                    if let Ok(f) = self.fs.frontier() {
                        self.send(from, &Message::Hello(f));
                    }
                }
                None => {}
            }
        }
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
            let Ok(entries) = self.fs.entries_since(&known, BATCH) else {
                continue;
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
