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

/// Asks the local `tailscaled` over its unix-socket API.
pub struct TailscaleWhois {
    socket: std::path::PathBuf,
}

impl TailscaleWhois {
    /// Where `tailscaled` listens by default on Linux.
    pub const DEFAULT_SOCKET: &'static str = "/var/run/tailscale/tailscaled.sock";

    pub fn new(socket: &std::path::Path) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    fn get(&self, path: &str) -> Result<(u16, Vec<u8>), String> {
        use std::io::{Read, Write};
        let unreachable = |e: std::io::Error| {
            format!("cannot reach tailscaled at {}: {e}", self.socket.display())
        };
        let mut stream =
            std::os::unix::net::UnixStream::connect(&self.socket).map_err(unreachable)?;
        let timeout = Some(std::time::Duration::from_secs(5));
        #[allow(
            clippy::let_underscore_must_use,
            reason = "best-effort: timeouts are a safeguard, the read reports real failures"
        )]
        let _ = stream.set_read_timeout(timeout);
        #[allow(
            clippy::let_underscore_must_use,
            reason = "best-effort: timeouts are a safeguard, the read reports real failures"
        )]
        let _ = stream.set_write_timeout(timeout);
        write!(
            stream,
            "GET {path} HTTP/1.0\r\nHost: local-tailscaled.sock\r\n\r\n"
        )
        .map_err(unreachable)?;
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).map_err(unreachable)?;
        let split = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .ok_or("tailscaled sent a malformed answer")?;
        let status = String::from_utf8_lossy(&raw[..split])
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse().ok())
            .ok_or("tailscaled sent a malformed answer")?;
        Ok((status, raw[split + 4..].to_vec()))
    }
}

impl Whois for TailscaleWhois {
    fn lookup(&self, addr: SocketAddr) -> Result<PeerIdentity, String> {
        let (status, body) = self.get(&format!("/localapi/v0/whois?addr={addr}"))?;
        let json: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|_| format!("tailscaled answered {status} with something unreadable"))?;
        if status != 200 {
            let why = json["error"].as_str().unwrap_or("unknown");
            return Err(format!("tailscaled does not know {addr}: {why}"));
        }
        let node = &json["Node"];
        Ok(PeerIdentity {
            name: node["Name"].as_str().unwrap_or_default().to_string(),
            tags: node["Tags"]
                .as_array()
                .map(|t| {
                    t.iter()
                        .filter_map(|t| t.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
        })
    }
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
