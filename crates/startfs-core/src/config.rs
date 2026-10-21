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
    /// Unset: derived from the machine's memory (see [`read_cache_default`]).
    pub read_cache_bytes: Option<u64>,
    /// Whether chunks this node stores are compressed: `auto` (try zstd on
    /// chunks a sample shows to be compressible, keep it only when it saves
    /// at least an eighth), `off`, or `always` (every chunk, kept whenever
    /// smaller). Only affects chunks stored from now on.
    pub compression: Compression,
    /// Most bytes of not-yet-replicated writes this node holds; writes are
    /// refused beyond it. Unset: only the free space of the staging partition
    /// bounds it (see `daemon::staging_limit`).
    pub staging_bytes: Option<u64>,
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
    /// Messages waiting in a peer's inbox from which this node sends it nothing
    /// new until it drains (it is let off below half). 0 turns that off.
    pub behind_inbox_messages: u64,
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
            read_cache_bytes: None,
            compression: Compression::Auto,
            staging_bytes: None,
            write_buffer_bytes: crate::fs::DEFAULT_WRITE_BUFFER,
            write_commit_delay_ms: 1000,
            sync_quiet_ms: 1000,
            token_ttl_secs: 24 * 3600,
            repair_grace_secs: 30 * 60,
            gc_retention_secs: 7 * 24 * 3600,
            standin_days: DEFAULT_STANDIN_DAYS,
            repair_bytes_per_sec: 64 << 20,
            behind_inbox_messages: 5000,
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

    /// The read cache size in effect, and whether it was derived rather than
    /// configured. `total_ram` is the machine's memory, when it could be read.
    pub fn read_cache_limit(&self, total_ram: Option<u64>) -> (u64, bool) {
        match self.read_cache_bytes {
            Some(bytes) => (bytes, false),
            None => (
                total_ram.map_or(DEFAULT_DECODED_BYTES, read_cache_default),
                true,
            ),
        }
    }
}

/// The read cache size for a machine with `total_ram` bytes of memory: 1/64 of
/// it, at least 16 MiB (a small board's idle budget) and at most 128 MiB.
pub fn read_cache_default(total_ram: u64) -> u64 {
    (total_ram / 64).clamp(16 << 20, 128 << 20)
}

/// The machine's total memory in bytes (the host's, not a container's limit),
/// or `None` when it cannot be read or the platform is not supported.
pub fn total_ram_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
        let kib: u64 = meminfo
            .lines()
            .find_map(|l| l.strip_prefix("MemTotal:"))?
            .split_whitespace()
            .next()?
            .parse()
            .ok()?;
        Some(kib * 1024)
    }
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;
    const MIB: u64 = 1 << 20;

    #[test]
    fn the_derived_size_is_a_64th_of_memory_between_16_and_128_mib() {
        assert_eq!(read_cache_default(0), 16 * MIB);
        assert_eq!(read_cache_default(GIB), 16 * MIB);
        assert_eq!(read_cache_default(4 * GIB), 64 * MIB);
        assert_eq!(read_cache_default(8 * GIB), 128 * MIB);
        assert_eq!(read_cache_default(64 * GIB), 128 * MIB);
    }

    #[test]
    fn a_configured_size_wins_whatever_the_memory() {
        for ram in [None, Some(GIB), Some(64 * GIB)] {
            let off = Config::parse("read_cache_bytes = 0").unwrap();
            assert_eq!(off.read_cache_limit(ram), (0, false));
            let set = Config::parse("read_cache_bytes = 1000").unwrap();
            assert_eq!(set.read_cache_limit(ram), (1000, false));
        }
    }

    #[test]
    fn unset_it_is_derived_from_memory_or_32_mib_when_memory_is_unknown() {
        let config = Config::default();
        assert_eq!(config.read_cache_limit(Some(4 * GIB)), (64 * MIB, true));
        assert_eq!(config.read_cache_limit(None), (32 * MIB, true));
    }
}
