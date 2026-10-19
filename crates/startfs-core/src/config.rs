//! Daemon configuration. Every field has a default so no file is needed.

use crate::cache::DEFAULT_DECODED_BYTES;
use crate::chunkstore::Compression;
use crate::error::Error;
use serde::{Deserialize, Serialize};

/// Days the extra copies made for a sleeping member are kept once it is back.
pub const DEFAULT_STANDIN_DAYS: u64 = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Port other members connect to (carried in join tokens).
    pub port: u16,
    /// Space this node offers to the filesystem, in bytes.
    pub offer_bytes: u64,
    /// Most bytes of fetched chunks kept on this node (the LRU cache).
    pub cache_bytes: u64,
    /// Most bytes of recently read chunks kept decoded in memory, so reading a
    /// file in small pieces does not decode a chunk for each piece. 0 turns it off.
    pub read_cache_bytes: u64,
    /// Whether chunks this node stores are compressed: `auto` (try zstd on
    /// chunks a sample shows to be compressible, keep it only when it saves
    /// at least an eighth), `off`, or `always` (every chunk, kept whenever
    /// smaller). Only affects chunks stored from now on.
    pub compression: Compression,
    /// Most bytes of not-yet-replicated writes this node holds; writes are
    /// refused beyond it.
    pub staging_bytes: u64,
    /// Most bytes of one file's writes held in memory before a batch is
    /// committed (at least 8 MiB is used). Buffered writes are lost if the
    /// daemon dies before the file is flushed, fsynced or closed.
    pub write_buffer_bytes: u64,
    /// How long a file must go unwritten before the background tick commits
    /// its buffered writes (closing or syncing the file commits at once).
    pub write_commit_delay_ms: u64,
    /// How long the node must go without a metadata commit before the tick
    /// makes recent ones durable (fsync, shutdown and talking to peers do so
    /// at once). A power cut inside this window can lose recent creates.
    pub sync_quiet_ms: u64,
    /// How long a join token stays valid, in seconds.
    pub token_ttl_secs: u64,
    /// When this node creates a filesystem: how long a member may be unseen
    /// before its copies are re-made elsewhere. Fixed for the filesystem.
    pub repair_grace_secs: u64,
    /// When this node creates a filesystem: how long chunks that no file
    /// references are kept (so deletes stay recoverable) before GC removes
    /// them. Fixed for the filesystem.
    pub gc_retention_secs: u64,
    /// When this node creates a filesystem: how many days the extra copies
    /// made to cover for a sleeping member are kept after it is back, in case
    /// it sleeps again. 0 turns stand-in copies off. Fixed for the filesystem.
    pub standin_days: u64,
    /// Most bytes per second this node sends when re-replicating chunks it
    /// already holds (repair and rebalancing; new writes are not limited).
    pub repair_bytes_per_sec: u64,
    /// How often the scrub reads back every replica this node holds, in seconds.
    pub scrub_period_secs: u64,
    /// Most bytes (encoded, roughly) in one piece when this node sends its
    /// whole merge state to a member that missed a log compaction.
    pub state_chunk_bytes: usize,
    /// Whether the daemon keeps the counters and timers behind `startfs
    /// activity` (a rolling minute, in memory). `startfs config --activity
    /// on|off` changes it until the daemon restarts.
    pub activity: bool,
    /// Where the daemon mounts the filesystem (`startfs daemon --mount DIR`
    /// overrides it). Unset: the daemon runs without a mount.
    pub mountpoint: Option<String>,
    /// Tailscale ACL tags (such as `tag:startfs`) a machine must carry, at
    /// least one of them, to talk to this node. Empty: any machine on the tailnet.
    pub allowed_tags: Vec<String>,
    /// The `tailscaled` unix socket used to identify peers. Unset: the
    /// platform default.
    pub whois_socket: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: 7600,
            offer_bytes: 10 << 30,
            cache_bytes: 1 << 30,
            read_cache_bytes: DEFAULT_DECODED_BYTES,
            compression: Compression::Auto,
            staging_bytes: 4 << 30,
            write_buffer_bytes: crate::fs::DEFAULT_WRITE_BUFFER,
            write_commit_delay_ms: 1000,
            sync_quiet_ms: 1000,
            token_ttl_secs: 24 * 3600,
            repair_grace_secs: 30 * 60,
            gc_retention_secs: 7 * 24 * 3600,
            standin_days: DEFAULT_STANDIN_DAYS,
            repair_bytes_per_sec: 64 << 20,
            scrub_period_secs: 30 * 24 * 3600,
            state_chunk_bytes: crate::sync::STATE_CHUNK_BYTES,
            activity: true,
            mountpoint: None,
            allowed_tags: Vec::new(),
            whois_socket: None,
        }
    }
}

impl Config {
    pub fn parse(text: &str) -> Result<Self, Error> {
        Ok(toml::from_str(text)?)
    }
}
