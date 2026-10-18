//! How long each peer takes to answer, so reads can ask the nearest first.

use crate::net::NodeId;
use crate::ops::Nanos;
use std::collections::HashMap;
use std::time::Duration;

/// An estimate older than this is forgotten: the peer or the path to it may
/// have changed, and a peer last seen slow must get the chance to prove it is not.
const STALE_AFTER: Nanos = Duration::from_secs(10 * 60).as_nanos() as Nanos;

/// A smoothed round-trip time per peer, learned from the answers it gives.
#[derive(Default)]
pub(super) struct PeerLatency {
    seen: HashMap<NodeId, (Nanos, Nanos)>,
}

impl PeerLatency {
    /// Fold one measured round trip (or the time waited in vain) into the estimate.
    pub(super) fn record(&mut self, peer: NodeId, sample: Nanos, now: Nanos) {
        let rtt = match self.estimate(peer, now) {
            Some(old) => (old * 3 + sample) / 4,
            None => sample,
        };
        self.seen.insert(peer, (rtt, now));
    }

    /// The current estimate for `peer`, if it has one that is not stale.
    pub(super) fn estimate(&self, peer: NodeId, now: Nanos) -> Option<Nanos> {
        let (rtt, at) = *self.seen.get(&peer)?;
        (now.saturating_sub(at) < STALE_AFTER).then_some(rtt)
    }
}
