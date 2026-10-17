//! Who a peer is, according to the local `tailscaled`.
//!
//! QUIC runs over throwaway self-signed certificates, so they prove nothing.
//! Instead each connection's remote address is looked up with `tailscaled`
//! (its `whois` API) and the answer decides whether the peer may talk to us.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;

/// What `tailscaled` knows about a peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerIdentity {
    /// The machine's name on the tailnet.
    pub name: String,
    /// Its Tailscale ACL tags, such as `tag:startfs`.
    pub tags: Vec<String>,
}

/// Looks peers up by their address. Behind a trait so tests can fake `tailscaled`.
pub trait Whois: Send + Sync {
    /// The peer at `addr`, or why it cannot be identified (unknown address,
    /// `tailscaled` unreachable).
    fn lookup(&self, addr: SocketAddr) -> Result<PeerIdentity, String>;
}

/// Whether a peer may join in: with `allowed_tags` empty, any tailnet machine;
/// otherwise only machines carrying at least one of those tags.
pub fn check_allowed(peer: &PeerIdentity, allowed_tags: &[String]) -> Result<(), String> {
    if allowed_tags.is_empty() || peer.tags.iter().any(|t| allowed_tags.contains(t)) {
        return Ok(());
    }
    Err(format!(
        "{} is not allowed: it carries none of the tags {}",
        peer.name,
        allowed_tags.join(", ")
    ))
}

/// A fixed table of peers by IP address; for tests and for running without Tailscale.
#[derive(Default)]
pub struct StaticWhois(Mutex<HashMap<IpAddr, PeerIdentity>>);

impl StaticWhois {
    pub fn add(&self, ip: IpAddr, peer: PeerIdentity) {
        self.0.lock().unwrap().insert(ip, peer);
    }
}

impl Whois for StaticWhois {
    fn lookup(&self, addr: SocketAddr) -> Result<PeerIdentity, String> {
        self.0
            .lock()
            .unwrap()
            .get(&addr.ip())
            .cloned()
            .ok_or_else(|| format!("{} is not a known tailnet machine", addr.ip()))
    }
}
