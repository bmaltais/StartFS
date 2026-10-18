//! Transport interface. Real QUIC and the simulated network both implement it.
//! Poll-based so a deterministic simulator can drive delivery.

use serde::{Deserialize, Serialize};

/// A node's transport endpoint on the wire. Not its member id: that is
/// [`MemberId`](crate::members::MemberId), assigned by the filesystem at join.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
pub struct NodeId(pub u32);

pub trait Transport: Send + Sync {
    /// Fire-and-forget; delivery is best effort.
    fn send(&self, to: NodeId, msg: Vec<u8>);
    /// Next message that has arrived, if any.
    fn recv(&self) -> Option<(NodeId, Vec<u8>)>;
    /// This node's own endpoint.
    fn local(&self) -> NodeId;
    /// The endpoint listening at a `host:port` address from a join token.
    fn resolve(&self, addr: &str) -> Option<NodeId>;
    /// Tell the transport where a member's endpoint listens (`host:port`), as
    /// the member list says, so it can be reached without a join token.
    /// A transport whose endpoints need no addresses ignores this.
    fn learn(&self, _node: NodeId, _addr: &str) {}
    /// Why the last attempt to reach `node` failed, in words for the user
    /// (peer refused, protocol version mismatch, unreachable).
    fn last_error(&self, _node: NodeId) -> Option<String> {
        None
    }
    /// How long ago a frame from `node` last arrived, whether or not the
    /// daemon has handled it yet; `None` when none has.
    fn last_heard(&self, _node: NodeId) -> Option<std::time::Duration> {
        None
    }
    /// The transport's own smoothed round-trip time to `node`, when it has a
    /// live path to it and can measure one.
    fn rtt(&self, _node: NodeId) -> Option<std::time::Duration> {
        None
    }
}
