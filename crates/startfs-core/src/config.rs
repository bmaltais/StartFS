//! Daemon configuration. Every field has a default so no file is needed.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Port other members connect to (carried in join tokens).
    pub port: u16,
    /// Space this node offers to the filesystem, in bytes.
    pub offer_bytes: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: 7600,
            offer_bytes: 10 << 30,
        }
    }
}

impl Config {
    pub fn parse(text: &str) -> Result<Self, String> {
        toml::from_str(text).map_err(|e| e.to_string())
    }
}
