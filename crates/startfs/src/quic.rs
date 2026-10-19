//! The real [`Transport`]: QUIC between peers' tailnet addresses.
//!
//! The core is poll-based and free of async, so this adapter runs quinn on a
//! background runtime and talks to the core through channels: `send` queues a
//! message for the peer's sender task, `recv` pops what the receiver tasks
//! collected.
//!
//! Layout on the wire, per peer and direction: one QUIC connection. The
//! dialer first opens a bidirectional stream for a handshake (protocol
//! version; the acceptor looks the dialer up with `tailscaled` and answers
//! "ok" or the reason it refuses). After that, metadata messages go over one
//! long-lived unidirectional stream, which keeps them in order, while every
//! message that carries chunk bytes gets a unidirectional stream of its own at
//! lower priority, so bulk transfers never hold up the metadata exchange.
//! Each message is a 4-byte big-endian length followed by its bytes.

use crate::error::Error;
use crate::identity::{Whois, check_allowed};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, ring};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use startfs_core::net::{Inbox, NodeId, Transport};
use startfs_core::protocol::{Message, PROTOCOL_VERSION};
use std::collections::HashMap;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

/// Largest single message accepted or sent (full-state transfers are the big ones).
pub const MAX_MESSAGE: usize = 256 << 20;

const ALPN: &[u8] = b"startfs";
const MAGIC: &[u8; 3] = b"SFS";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// After a failed connection attempt, messages for that peer are dropped for this long.
const RETRY_AFTER: Duration = Duration::from_secs(2);
const CLOSE_REFUSED: u32 = 1;

/// Resolve a `host:port` string, preferring IPv4: nodes listen on their tailnet
/// IPv4 only, while a MagicDNS name also carries an IPv6 address.
pub fn resolve_addr(addr: &str) -> Option<SocketAddr> {
    let all: Vec<SocketAddr> = addr.to_socket_addrs().ok()?.collect();
    all.iter().find(|a| a.is_ipv4()).or(all.first()).copied()
}

/// The transport endpoint for an address: every node derives the same id from
/// the address a peer listens on (FNV-1a, which is stable across releases).
pub fn node_id_for(addr: SocketAddr) -> NodeId {
    let mut bytes = Vec::new();
    match addr.ip().to_canonical() {
        std::net::IpAddr::V4(v4) => {
            bytes.push(4);
            bytes.extend(v4.octets());
        }
        std::net::IpAddr::V6(v6) => {
            bytes.push(6);
            bytes.extend(v6.octets());
        }
    }
    bytes.extend(addr.port().to_be_bytes());
    let mut h: u32 = 0x811c_9dc5;
    for b in bytes {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    NodeId(h.max(1))
}

struct Shared {
    local: NodeId,
    /// The protocol version this node speaks.
    version: u8,
    whois: Arc<dyn Whois>,
    allowed_tags: Vec<String>,
    /// Where each known peer listens.
    addrs: Mutex<HashMap<NodeId, SocketAddr>>,
    /// Why the last attempt to talk to a peer failed.
    errors: Mutex<HashMap<NodeId, String>>,
    /// The live connection we dialed to each peer, for its round-trip time.
    links: Mutex<HashMap<NodeId, quinn::Connection>>,
    inbox: Mutex<Sender<(NodeId, Vec<u8>)>>,
    /// Messages and bytes sent into the inbox and not yet taken out.
    inbox_messages: AtomicU64,
    inbox_bytes: AtomicU64,
    /// When a frame from each peer last arrived, handled by the daemon or not.
    heard: Mutex<HashMap<NodeId, Instant>>,
}

impl Shared {
    /// Look a peer up with `tailscaled` and apply the tag policy.
    async fn authorize(self: &Arc<Self>, addr: SocketAddr) -> Result<(), Error> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            let peer = this.whois.lookup(addr)?;
            check_allowed(&peer, &this.allowed_tags)
        })
        .await?
    }

    fn fail(&self, peer: NodeId, why: Error) {
        eprintln!("startfs: peer {}: {why}", peer.0);
        self.errors.lock().unwrap().insert(peer, why.to_string());
    }
}

pub struct QuicTransport {
    shared: Arc<Shared>,
    local_addr: SocketAddr,
    inbox: Mutex<Receiver<(NodeId, Vec<u8>)>>,
    senders: Mutex<HashMap<NodeId, UnboundedSender<Vec<u8>>>>,
    endpoint: quinn::Endpoint,
    runtime: Option<tokio::runtime::Runtime>,
}

impl QuicTransport {
    /// Listen on `bind` (port 0 picks one). Peers are identified with `whois`
    /// and must carry one of `allowed_tags` when any are given.
    pub fn start(
        bind: SocketAddr,
        whois: Arc<dyn Whois>,
        allowed_tags: Vec<String>,
    ) -> std::io::Result<Self> {
        Self::start_versioned(bind, whois, allowed_tags, PROTOCOL_VERSION)
    }

    /// Like [`start`](Self::start) but speaking another protocol version, to
    /// exercise version mismatches.
    #[doc(hidden)]
    pub fn start_versioned(
        bind: SocketAddr,
        whois: Arc<dyn Whois>,
        allowed_tags: Vec<String>,
        version: u8,
    ) -> std::io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("startfs-quic")
            .enable_all()
            .build()?;
        let (server, client) = crypto().map_err(std::io::Error::other)?;
        let mut server = quinn::ServerConfig::with_crypto(Arc::new(server));
        server.transport_config(Arc::new(transport_config()));
        let mut client = quinn::ClientConfig::new(Arc::new(client));
        client.transport_config(Arc::new(transport_config()));
        let endpoint = {
            let _guard = runtime.enter();
            let mut endpoint = quinn::Endpoint::server(server, bind)?;
            endpoint.set_default_client_config(client);
            endpoint
        };
        let local_addr = endpoint.local_addr()?;
        let (tx, rx) = channel();
        let shared = Arc::new(Shared {
            local: node_id_for(local_addr),
            version,
            whois,
            allowed_tags,
            addrs: Mutex::new(HashMap::new()),
            errors: Mutex::new(HashMap::new()),
            links: Mutex::new(HashMap::new()),
            inbox: Mutex::new(tx),
            inbox_messages: AtomicU64::new(0),
            inbox_bytes: AtomicU64::new(0),
            heard: Mutex::new(HashMap::new()),
        });
        runtime.spawn(accept_loop(shared.clone(), endpoint.clone()));
        Ok(Self {
            shared,
            local_addr,
            inbox: Mutex::new(rx),
            senders: Mutex::new(HashMap::new()),
            endpoint,
            runtime: Some(runtime),
        })
    }

    /// The address this node listens on (and advertises).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    fn runtime(&self) -> &tokio::runtime::Runtime {
        self.runtime.as_ref().expect("runtime lives until drop")
    }
}

impl Drop for QuicTransport {
    fn drop(&mut self) {
        self.endpoint.close(0u32.into(), b"shutdown");
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_background();
        }
    }
}

impl Transport for QuicTransport {
    fn send(&self, to: NodeId, msg: Vec<u8>) {
        let mut senders = self.senders.lock().unwrap();
        if senders.get(&to).is_none_or(|s| s.is_closed()) {
            let Some(addr) = self.shared.addrs.lock().unwrap().get(&to).copied() else {
                return;
            };
            let (tx, rx) = unbounded_channel();
            self.runtime().spawn(peer_sender(
                self.shared.clone(),
                self.endpoint.clone(),
                to,
                addr,
                rx,
            ));
            senders.insert(to, tx);
        }
        #[allow(
            clippy::let_underscore_must_use,
            reason = "best-effort send to a peer that may be gone"
        )]
        let _ = senders[&to].send(msg);
    }

    fn recv(&self) -> Option<(NodeId, Vec<u8>)> {
        let (from, body) = self.inbox.lock().unwrap().try_recv().ok()?;
        self.shared.inbox_messages.fetch_sub(1, Ordering::Relaxed);
        self.shared
            .inbox_bytes
            .fetch_sub(body.len() as u64, Ordering::Relaxed);
        Some((from, body))
    }

    fn inbox(&self) -> Option<Inbox> {
        Some(Inbox {
            messages: self.shared.inbox_messages.load(Ordering::Relaxed),
            bytes: self.shared.inbox_bytes.load(Ordering::Relaxed),
        })
    }

    fn local(&self) -> NodeId {
        self.shared.local
    }

    fn last_heard(&self, node: NodeId) -> Option<Duration> {
        self.shared
            .heard
            .lock()
            .unwrap()
            .get(&node)
            .map(Instant::elapsed)
    }

    fn resolve(&self, addr: &str) -> Option<NodeId> {
        let addr = resolve_addr(addr)?;
        let id = node_id_for(addr);
        self.shared.addrs.lock().unwrap().insert(id, addr);
        Some(id)
    }

    fn learn(&self, node: NodeId, addr: &str) {
        if node == self.shared.local {
            return;
        }
        if let Some(addr) = resolve_addr(addr) {
            self.shared.addrs.lock().unwrap().insert(node, addr);
        }
    }

    fn last_error(&self, node: NodeId) -> Option<String> {
        self.shared.errors.lock().unwrap().get(&node).cloned()
    }

    fn rtt(&self, node: NodeId) -> Option<Duration> {
        let links = self.shared.links.lock().unwrap();
        let conn = links.get(&node)?;
        conn.close_reason().is_none().then(|| conn.rtt())
    }
}

fn transport_config() -> quinn::TransportConfig {
    let mut cfg = quinn::TransportConfig::default();
    cfg.keep_alive_interval(Some(Duration::from_secs(5)));
    cfg.max_idle_timeout(Some(
        Duration::from_secs(30).try_into().expect("idle timeout"),
    ));
    cfg
}

// ---- receiving ----

async fn accept_loop(shared: Arc<Shared>, endpoint: quinn::Endpoint) {
    while let Some(incoming) = endpoint.accept().await {
        tokio::spawn(serve_connection(shared.clone(), incoming));
    }
}

async fn serve_connection(shared: Arc<Shared>, incoming: quinn::Incoming) {
    let Ok(conn) = incoming.await else { return };
    let remote = conn.remote_address();
    let peer = node_id_for(remote);
    match handshake_accept(&shared, &conn, remote).await {
        Ok(()) => {}
        Err(why) => {
            conn.close(CLOSE_REFUSED.into(), why.refusal_text().as_bytes());
            shared.fail(peer, why);
            return;
        }
    }
    shared.addrs.lock().unwrap().insert(peer, remote);
    while let Ok(stream) = conn.accept_uni().await {
        tokio::spawn(read_messages(shared.clone(), peer, stream));
    }
}

/// Check the dialer's version and identity; tell it the verdict.
async fn handshake_accept(
    shared: &Arc<Shared>,
    conn: &quinn::Connection,
    remote: SocketAddr,
) -> Result<(), Error> {
    let (mut reply, mut hello) = conn.accept_bi().await?;
    let mut raw = [0u8; 4];
    tokio::time::timeout(CONNECT_TIMEOUT, hello.read_exact(&mut raw))
        .await
        .map_err(|_| Error::NoHandshake)??;
    let verdict = if &raw[..3] != MAGIC {
        Err(Error::NotAPeer)
    } else if raw[3] != shared.version {
        Err(Error::VersionMismatch {
            ours: shared.version,
            theirs: raw[3],
        })
    } else {
        shared.authorize(remote).await
    };
    let answer = match &verdict {
        Ok(()) => vec![0],
        Err(why) => [&[1][..], why.refusal_text().as_bytes()].concat(),
    };
    reply.write_all(&answer).await?;
    reply.finish()?;
    // Let the verdict reach the dialer before the connection is torn down.
    if verdict.is_err() {
        #[allow(
            clippy::let_underscore_must_use,
            reason = "best-effort: the peer may be gone"
        )]
        let _ = tokio::time::timeout(Duration::from_secs(1), reply.stopped()).await;
    }
    verdict
}

async fn read_messages(shared: Arc<Shared>, from: NodeId, mut stream: quinn::RecvStream) {
    loop {
        let mut len = [0u8; 4];
        if stream.read_exact(&mut len).await.is_err() {
            return;
        }
        let len = u32::from_be_bytes(len) as usize;
        if len > MAX_MESSAGE {
            shared.fail(from, Error::MessageTooLarge(len));
            return;
        }
        let mut body = vec![0u8; len];
        if stream.read_exact(&mut body).await.is_err() {
            return;
        }
        shared.inbox_messages.fetch_add(1, Ordering::Relaxed);
        shared
            .inbox_bytes
            .fetch_add(body.len() as u64, Ordering::Relaxed);
        #[allow(
            clippy::let_underscore_must_use,
            reason = "best-effort: the daemon may have stopped receiving"
        )]
        let _ = shared.inbox.lock().unwrap().send((from, body));
        shared.heard.lock().unwrap().insert(from, Instant::now());
    }
}

// ---- sending ----

struct Link {
    conn: quinn::Connection,
    control: quinn::SendStream,
}

async fn connect(
    shared: &Arc<Shared>,
    endpoint: &quinn::Endpoint,
    addr: SocketAddr,
) -> Result<Link, Error> {
    shared.authorize(addr).await?;
    let attempt = async {
        let conn = endpoint.connect(addr, "startfs")?.await?;
        let (mut hello, mut reply) = conn.open_bi().await?;
        hello
            .write_all(&[MAGIC[0], MAGIC[1], MAGIC[2], shared.version])
            .await?;
        hello.finish()?;
        let answer = reply.read_to_end(1024).await.map_err(|_| {
            // The acceptor closed the connection; its reason says why.
            match conn.close_reason() {
                Some(quinn::ConnectionError::ApplicationClosed(c)) => {
                    Error::Refused(String::from_utf8_lossy(&c.reason).into_owned())
                }
                other => Error::HandshakeFailed(other),
            }
        })?;
        match answer.split_first() {
            Some((0, _)) => {}
            Some((_, why)) => {
                return Err(Error::Refused(String::from_utf8_lossy(why).into_owned()));
            }
            None => return Err(Error::EmptyHandshakeAnswer),
        }
        let control = conn.open_uni().await?;
        Ok(Link { conn, control })
    };
    tokio::time::timeout(CONNECT_TIMEOUT, attempt)
        .await
        .map_err(|_| Error::ConnectTimeout(addr))?
}

async fn write_frame(stream: &mut quinn::SendStream, msg: &[u8]) -> Result<(), Error> {
    stream.write_all(&(msg.len() as u32).to_be_bytes()).await?;
    Ok(stream.write_all(msg).await?)
}

/// Owns the connection to one peer and sends what the core queues for it.
async fn peer_sender(
    shared: Arc<Shared>,
    endpoint: quinn::Endpoint,
    peer: NodeId,
    addr: SocketAddr,
    mut queue: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
) {
    let mut link: Option<Link> = None;
    let mut failed_at: Option<Instant> = None;
    while let Some(msg) = queue.recv().await {
        if msg.len() > MAX_MESSAGE {
            shared.fail(peer, Error::MessageTooLarge(msg.len()));
            continue;
        }
        if link
            .as_ref()
            .is_some_and(|l| l.conn.close_reason().is_some())
        {
            link = None;
        }
        if link.is_none() {
            if failed_at.is_some_and(|t| t.elapsed() < RETRY_AFTER) {
                continue;
            }
            match connect(&shared, &endpoint, addr).await {
                Ok(l) => {
                    shared.errors.lock().unwrap().remove(&peer);
                    failed_at = None;
                    shared.links.lock().unwrap().insert(peer, l.conn.clone());
                    link = Some(l);
                }
                Err(why) => {
                    shared.fail(peer, why);
                    failed_at = Some(Instant::now());
                    continue;
                }
            }
        }
        let l = link.as_mut().expect("connected above");
        if Message::is_bulk(&msg) {
            let conn = l.conn.clone();
            tokio::spawn(async move {
                if let Ok(mut stream) = conn.open_uni().await {
                    #[allow(
                        clippy::let_underscore_must_use,
                        reason = "best-effort: priority is only a hint"
                    )]
                    let _ = stream.set_priority(-1);
                    if write_frame(&mut stream, &msg).await.is_ok() {
                        #[allow(
                            clippy::let_underscore_must_use,
                            reason = "best-effort: the peer may be gone"
                        )]
                        let _ = stream.finish();
                        #[allow(
                            clippy::let_underscore_must_use,
                            reason = "best-effort: the peer may be gone"
                        )]
                        let _ = stream.stopped().await;
                    }
                }
            });
        } else if write_frame(&mut l.control, &msg).await.is_err() {
            link = None;
        }
    }
}

// ---- TLS: throwaway certificates, identity comes from whois ----

fn crypto() -> Result<(QuicServerConfig, QuicClientConfig), Error> {
    let provider = Arc::new(ring::default_provider());
    let key = rcgen::generate_simple_self_signed(vec!["startfs".to_string()])?;
    let cert = CertificateDer::from(key.cert.der().to_vec());
    let private = PrivateKeyDer::Pkcs8(key.signing_key.serialize_der().into());

    let mut server = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(vec![cert], private)?;
    server.alpn_protocols = vec![ALPN.to_vec()];

    let mut client = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyCertificate(provider)))
        .with_no_client_auth();
    client.alpn_protocols = vec![ALPN.to_vec()];

    Ok((
        QuicServerConfig::try_from(server)?,
        QuicClientConfig::try_from(client)?,
    ))
}

/// Accepts any certificate: the peer is vetted by `tailscaled`, not by TLS.
#[derive(Debug)]
struct AnyCertificate(Arc<CryptoProvider>);

impl ServerCertVerifier for AnyCertificate {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}
