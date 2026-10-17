//! What can go wrong in the mount layer and the network transport.
//!
//! The `Display` text is what the user sees. A handshake refusal travels to the
//! dialer as that text, so the wording here is part of what peers show each other.

use std::net::IpAddr;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("cannot reach tailscaled at {}: {source}", socket.display())]
    TailscaledUnreachable {
        socket: PathBuf,
        source: std::io::Error,
    },
    #[error("tailscaled sent a malformed answer")]
    TailscaledMalformed,
    #[error("tailscaled answered {status} with something unreadable")]
    TailscaledUnreadable { status: u16 },
    #[error("tailscaled does not know {addr}: {why}")]
    TailscaledUnknownPeer { addr: String, why: String },
    #[error("{name} is not allowed: it carries none of the tags {}", tags.join(", "))]
    PeerNotAllowed { name: String, tags: Vec<String> },
    #[error("{0} is not a known tailnet machine")]
    UnknownMachine(IpAddr),

    #[error("no handshake")]
    NoHandshake,
    #[error("not a StartFS peer")]
    NotAPeer,
    #[error(
        "protocol version mismatch: the peer speaks version {theirs}, this node speaks version {ours}"
    )]
    VersionMismatch { ours: u8, theirs: u8 },
    #[error("empty handshake answer")]
    EmptyHandshakeAnswer,
    #[error("handshake failed: {0:?}")]
    HandshakeFailed(Option<quinn::ConnectionError>),
    #[error("timed out connecting to {0}")]
    ConnectTimeout(std::net::SocketAddr),
    #[error("message of {0} bytes exceeds the limit")]
    MessageTooLarge(usize),
    /// The other side's refusal, as the text it sent over the handshake.
    #[error("{0}")]
    Refused(String),

    #[error(transparent)]
    Connect(#[from] quinn::ConnectError),
    #[error(transparent)]
    Connection(#[from] quinn::ConnectionError),
    #[error(transparent)]
    Write(#[from] quinn::WriteError),
    #[error(transparent)]
    Read(#[from] quinn::ReadExactError),
    #[error(transparent)]
    Closed(#[from] quinn::ClosedStream),
    #[error(transparent)]
    Task(#[from] tokio::task::JoinError),
    #[error(transparent)]
    Tls(#[from] rustls::Error),
    #[error(transparent)]
    CipherSuite(#[from] quinn::crypto::rustls::NoInitialCipherSuite),
    #[error(transparent)]
    Certificate(#[from] rcgen::Error),

    #[error("cannot mount at {}: {source}", mountpoint.display())]
    Mount {
        mountpoint: PathBuf,
        source: std::io::Error,
    },
    #[error(transparent)]
    Unmount(std::io::Error),
}
