//! In-process cluster harness: N real nodes, a simulated network, crash and
//! restart, and an injectable clock. Tests drive nodes only through
//! [`FsOps`](startfs_core::ops::FsOps).

mod cluster;
mod fixtures;
mod network;
mod releases;

pub use cluster::{Cluster, NodeFs, Restart};
pub use fixtures::noise;
pub use network::{SimNetwork, SimTransport};
