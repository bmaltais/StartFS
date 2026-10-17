//! Transport interface. Real QUIC and the simulated network both implement it.
//! Poll-based so a deterministic simulator can drive delivery.

use serde::{Deserialize, Serialize};

/// A node's identity: the transport endpoint on the wire, and the member id
/// the filesystem assigned at join (which tags its log entries and inodes).
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
}
