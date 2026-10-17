//! Daemon configuration. Every field has a default so no file is needed.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Port other members connect to (carried in join tokens).
    pub port: u16,
    /// Space this node offers to the filesystem, in bytes.
    pub offer_bytes: u64,
    /// Most bytes of fetched chunks kept on this node (the LRU cache).
    pub cache_bytes: u64,
    /// How long a join token stays valid, in seconds.
    pub token_ttl_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: 7600,
            offer_bytes: 10 << 30,
            cache_bytes: 1 << 30,
            token_ttl_secs: 24 * 3600,
        }
    }
}

impl Config {
    pub fn parse(text: &str) -> Result<Self, String> {
        toml::from_str(text).map_err(|e| e.to_string())
    }
}
