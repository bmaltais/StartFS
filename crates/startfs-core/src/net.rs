//! Transport interface. Real QUIC and the simulated network both implement it.
//! Poll-based so a deterministic simulator can drive delivery.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub u32);

pub trait Transport: Send + Sync {
    /// Fire-and-forget; delivery is best effort.
    fn send(&self, to: NodeId, msg: Vec<u8>);
    /// Next message that has arrived, if any.
    fn recv(&self) -> Option<(NodeId, Vec<u8>)>;
}
