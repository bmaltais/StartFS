//! In-process cluster harness: N real nodes, a simulated network, crash and
//! restart, and an injectable clock. Tests drive nodes only through
//! [`FsOps`](startfs_core::ops::FsOps).

mod cluster;
mod fixtures;
mod network;

pub use cluster::{Cluster, NodeFs};
pub use fixtures::noise;
pub use network::{SimNetwork, SimTransport};
