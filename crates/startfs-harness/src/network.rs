use startfs_core::clock::Clock;
use startfs_core::net::{NodeId, Transport};
use startfs_core::ops::Nanos;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct InFlight {
    deliver_at: Nanos,
    from: NodeId,
    to: NodeId,
    msg: Vec<u8>,
}

#[derive(Default)]
struct State {
    in_flight: Vec<InFlight>,
    /// Nodes in different groups cannot talk. Unlisted nodes are in group 0.
    groups: HashMap<NodeId, u32>,
    /// Directed links that silently drop everything.
    dropped: HashSet<(NodeId, NodeId)>,
    delays: HashMap<(NodeId, NodeId), Duration>,
    down: HashSet<NodeId>,
    /// `host:port` addresses nodes listen on.
    addrs: HashMap<String, NodeId>,
}

impl State {
    fn reachable(&self, from: NodeId, to: NodeId) -> bool {
        !self.down.contains(&from)
            && !self.down.contains(&to)
            && !self.dropped.contains(&(from, to))
            && self.groups.get(&from).copied().unwrap_or(0)
                == self.groups.get(&to).copied().unwrap_or(0)
    }
}

/// Simulated network with partitions, delay and loss, driven by the cluster clock.
#[derive(Clone)]
pub struct SimNetwork {
    clock: Arc<dyn Clock>,
    state: Arc<Mutex<State>>,
}

impl SimNetwork {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            state: Default::default(),
        }
    }

    pub fn transport(&self, node: NodeId) -> SimTransport {
        SimTransport {
            node,
            net: self.clone(),
        }
    }

    /// Split the network: each listed group can only talk within itself;
    /// nodes not listed form their own implicit group.
    pub fn partition(&self, groups: &[&[NodeId]]) {
        let mut s = self.state.lock().unwrap();
        s.groups.clear();
        for (i, g) in groups.iter().enumerate() {
            for n in *g {
                s.groups.insert(*n, i as u32 + 1);
            }
        }
    }

    /// Remove all partitions, drops and delays.
    pub fn heal(&self) {
        let mut s = self.state.lock().unwrap();
        s.groups.clear();
        s.dropped.clear();
        s.delays.clear();
    }

    /// Drop (or stop dropping) all traffic in both directions between two nodes.
    pub fn set_drop(&self, a: NodeId, b: NodeId, drop: bool) {
        let mut s = self.state.lock().unwrap();
        for link in [(a, b), (b, a)] {
            if drop {
                s.dropped.insert(link);
            } else {
                s.dropped.remove(&link);
            }
        }
    }

    /// Delay all traffic in both directions between two nodes.
    pub fn set_delay(&self, a: NodeId, b: NodeId, delay: Duration) {
        let mut s = self.state.lock().unwrap();
        s.delays.insert((a, b), delay);
        s.delays.insert((b, a), delay);
    }

    /// Make `node` reachable at a `host:port` address, as join tokens name it.
    pub fn register(&self, addr: &str, node: NodeId) {
        self.state.lock().unwrap().addrs.insert(addr.into(), node);
    }

    pub(crate) fn set_down(&self, node: NodeId, down: bool) {
        let mut s = self.state.lock().unwrap();
        if down {
            s.down.insert(node);
            s.in_flight.retain(|m| m.to != node && m.from != node);
        } else {
            s.down.remove(&node);
        }
    }
}

pub struct SimTransport {
    node: NodeId,
    net: SimNetwork,
}

impl Transport for SimTransport {
    fn send(&self, to: NodeId, msg: Vec<u8>) {
        let mut s = self.net.state.lock().unwrap();
        if !s.reachable(self.node, to) {
            return;
        }
        let delay = s.delays.get(&(self.node, to)).copied().unwrap_or_default();
        let deliver_at = self.net.clock.now() + delay.as_nanos() as u64;
        s.in_flight.push(InFlight {
            deliver_at,
            from: self.node,
            to,
            msg,
        });
    }

    fn local(&self) -> NodeId {
        self.node
    }

    fn resolve(&self, addr: &str) -> Option<NodeId> {
        self.net.state.lock().unwrap().addrs.get(addr).copied()
    }

    fn recv(&self) -> Option<(NodeId, Vec<u8>)> {
        let now = self.net.clock.now();
        let mut s = self.net.state.lock().unwrap();
        // Oldest deliverable message first; re-check reachability so a
        // partition raised while a message was in flight still cuts it off.
        let pos = s
            .in_flight
            .iter()
            .position(|m| m.to == self.node && m.deliver_at <= now)?;
        let m = s.in_flight.remove(pos);
        if s.reachable(m.from, m.to) {
            Some((m.from, m.msg))
        } else {
            None
        }
    }
}
