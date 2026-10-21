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
use startfs_core::protocol::{Message, NOTICE_BYTE, PROTOCOL_VERSION};
use startfs_core::wake::Waker;
use std::collections::HashMap;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};
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
const CLOSE_SUPERSEDED: u32 = 2;
/// Chunk bytes waiting in the inbox past which a stream stops being read, so QUIC flow control
/// slows the sender instead of this node's memory growing (#91). Metadata and gossip are exempt.
pub const INBOX_BULK_LIMIT: u64 = 64 << 20;
const INBOX_POLL: Duration = Duration::from_millis(5);
/// Flow-control window of one stream. Every chunk message has a stream of its own, so this is how
/// much of a chunk can be in flight in one round trip: quinn's default (about 1.25 MiB) holds a
/// 1 to 4 MiB chunk to a fraction of the link on any path with real latency.
const STREAM_WINDOW: u32 = 8 << 20;
/// Flow-control window of a connection, sized for the bulk bytes the replicator allows in flight
/// to one peer (`BACKLOG_LIMIT`) and the inbox holds back (`INBOX_BULK_LIMIT`).
const CONNECTION_WINDOW: u32 = 64 << 20;
/// Kernel buffer asked for each direction of the UDP socket. The default (about 200 KB) overflows
/// when a window of data arrives in a burst; the kernel caps the request at `net.core.rmem_max`.
const SOCKET_BUFFER: usize = 8 << 20;

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
    /// The live connection each peer dialed to us, with the instance the dialer named. A
    /// second one from a different instance means the peer restarted: the first is dead and so
    /// is our own link to it. One from the same instance is a redial, and our link is fine.
    dialed: Mutex<HashMap<NodeId, (quinn::Connection, Option<Instance>)>>,
    /// Names this run of the daemon to the peers it dials.
    instance: Instance,
    inbox: Mutex<Sender<(NodeId, Vec<u8>)>>,
    /// Woken when something arrives that the daemon should not leave waiting.
    waker: OnceLock<Arc<Waker>>,
    /// Connections this node has opened to peers, successful or not.
    connects: AtomicU64,
    /// Messages and bytes sent into the inbox and not yet taken out.
    inbox_messages: AtomicU64,
    inbox_bytes: AtomicU64,
    /// Chunk bytes queued or in flight to each peer.
    backlog: Mutex<HashMap<NodeId, u64>>,
    /// When a frame from each peer last arrived, handled by the daemon or not.
    heard: Mutex<HashMap<NodeId, Instant>>,
    /// The upgrade notice to hand to a peer refused for its protocol version.
    notice: Mutex<Option<Vec<u8>>>,
}

/// Random bytes naming one run of a daemon, sent in the handshake so an acceptor can tell a
/// peer that dials again from one that restarted.
type Instance = [u8; 8];

/// A message queued for a peer, with its claim on the backlog when it carries chunk bytes.
type Outgoing = (Vec<u8>, Option<Held>);

/// Counts a chunk message against a peer's backlog until it is dropped.
struct Held {
    shared: Arc<Shared>,
    to: NodeId,
    bytes: u64,
}

impl Held {
    fn new(shared: &Arc<Shared>, to: NodeId, bytes: usize) -> Self {
        let bytes = bytes as u64;
        *shared.backlog.lock().unwrap().entry(to).or_default() += bytes;
        Self {
            shared: shared.clone(),
            to,
            bytes,
        }
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        let mut backlog = self.shared.backlog.lock().unwrap();
        if let Some(n) = backlog.get_mut(&self.to) {
            *n = n.saturating_sub(self.bytes);
        }
    }
}

impl Shared {
    /// Hand the daemon a message as if `from` had sent it over a working connection.
    fn deliver(&self, from: NodeId, body: Vec<u8>) {
        let background = Message::is_background(&body);
        self.inbox_messages.fetch_add(1, Ordering::Relaxed);
        self.inbox_bytes
            .fetch_add(body.len() as u64, Ordering::Relaxed);
        #[allow(
            clippy::let_underscore_must_use,
            reason = "best-effort: the daemon may have stopped receiving"
        )]
        let _ = self.inbox.lock().unwrap().send((from, body));
        if !background {
            self.wake();
        }
    }

    /// Cut the daemon's idle wait short: a message is waiting for it.
    fn wake(&self) {
        if let Some(w) = self.waker.get() {
            w.wake();
        }
    }

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
    senders: Mutex<HashMap<NodeId, UnboundedSender<Outgoing>>>,
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
            let mut endpoint = quinn::Endpoint::new(
                quinn::EndpointConfig::default(),
                Some(server),
                bound_socket(bind)?,
                Arc::new(quinn::TokioRuntime),
            )?;
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
            dialed: Mutex::new(HashMap::new()),
            instance: new_instance()?,
            inbox: Mutex::new(tx),
            waker: OnceLock::new(),
            connects: AtomicU64::new(0),
            inbox_messages: AtomicU64::new(0),
            inbox_bytes: AtomicU64::new(0),
            backlog: Mutex::new(HashMap::new()),
            heard: Mutex::new(HashMap::new()),
            notice: Mutex::new(None),
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

    /// How many connections this node has opened to peers, successful or not. Each costs a
    /// whois lookup and a TLS handshake, so a count that keeps climbing on a quiet node means
    /// links are being torn down and redialed.
    pub fn connects(&self) -> u64 {
        self.shared.connects.load(Ordering::Relaxed)
    }

    /// Drop the connection this node dialed to `peer`, as a network blip would, so the next
    /// message redials. For tests.
    #[doc(hidden)]
    pub fn sever_link(&self, peer: NodeId) {
        if let Some(link) = self.shared.links.lock().unwrap().get(&peer) {
            link.close(0u32.into(), b"severed");
        }
    }

    /// The address this node listens on (and advertises).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Wake `waker` when a message arrives that the daemon should not leave waiting.
    pub fn set_waker(&self, waker: Arc<Waker>) {
        // Set once, when the tick starts; a second call keeps the first.
        let _kept = self.shared.waker.set(waker);
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
        let held = Message::is_bulk(&msg).then(|| Held::new(&self.shared, to, msg.len()));
        #[allow(
            clippy::let_underscore_must_use,
            reason = "best-effort send to a peer that may be gone"
        )]
        let _ = senders[&to].send((msg, held));
    }

    fn recv(&self) -> Option<(NodeId, Vec<u8>)> {
        let (from, body) = self.inbox.lock().unwrap().try_recv().ok()?;
        self.shared.inbox_messages.fetch_sub(1, Ordering::Relaxed);
        self.shared
            .inbox_bytes
            .fetch_sub(body.len() as u64, Ordering::Relaxed);
        Some((from, body))
    }

    fn backlog(&self, to: NodeId) -> u64 {
        self.shared
            .backlog
            .lock()
            .unwrap()
            .get(&to)
            .copied()
            .unwrap_or(0)
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

    fn set_notice(&self, notice: Option<Vec<u8>>) {
        *self.shared.notice.lock().unwrap() = notice;
    }

    fn rtt(&self, node: NodeId) -> Option<Duration> {
        let links = self.shared.links.lock().unwrap();
        let conn = links.get(&node)?;
        conn.close_reason().is_none().then(|| conn.rtt())
    }
}

/// A UDP socket bound to `bind` with larger kernel buffers than the default.
fn bound_socket(bind: SocketAddr) -> std::io::Result<std::net::UdpSocket> {
    let socket = socket2::Socket::new(
        socket2::Domain::for_address(bind),
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )?;
    // A refused size only leaves the default in place.
    #[allow(
        clippy::let_underscore_must_use,
        reason = "best-effort: the kernel may cap or refuse the size"
    )]
    let _ = socket.set_recv_buffer_size(SOCKET_BUFFER);
    #[allow(
        clippy::let_underscore_must_use,
        reason = "best-effort: the kernel may cap or refuse the size"
    )]
    let _ = socket.set_send_buffer_size(SOCKET_BUFFER);
    socket.bind(&bind.into())?;
    Ok(socket.into())
}

fn transport_config() -> quinn::TransportConfig {
    let mut cfg = quinn::TransportConfig::default();
    cfg.keep_alive_interval(Some(Duration::from_secs(5)));
    cfg.max_idle_timeout(Some(
        Duration::from_secs(30).try_into().expect("idle timeout"),
    ));
    cfg.stream_receive_window(STREAM_WINDOW.into());
    cfg.receive_window(CONNECTION_WINDOW.into());
    cfg.send_window(u64::from(CONNECTION_WINDOW));
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
    let instance = match handshake_accept(&shared, &conn, remote).await {
        Ok(instance) => instance,
        Err(why) => {
            conn.close(CLOSE_REFUSED.into(), why.refusal_text().as_bytes());
            shared.fail(peer, why);
            return;
        }
    };
    shared.addrs.lock().unwrap().insert(peer, remote);
    let previous = shared
        .dialed
        .lock()
        .unwrap()
        .insert(peer, (conn.clone(), instance));
    if let Some((old, old_instance)) = previous {
        old.close(CLOSE_SUPERSEDED.into(), b"superseded");
        // A restarted peer leaves our link to it on a dead path until the idle timeout, so
        // its hellos would reach the new daemon half a minute late: drop the link. A peer
        // that merely dialed again (its own link broke) has the same instance and our link
        // is fine; closing it would make the peer redial in turn, and the two would keep
        // closing each other's links. A peer that does not name its instance cannot be told
        // apart, and is left alone.
        let restarted = instance != old_instance;
        if restarted
            && instance.or(old_instance).is_some()
            && let Some(link) = shared.links.lock().unwrap().remove(&peer)
        {
            link.close(CLOSE_SUPERSEDED.into(), b"peer restarted");
        }
    }
    while let Ok(stream) = conn.accept_uni().await {
        tokio::spawn(read_messages(shared.clone(), peer, stream));
    }
}

/// Check the dialer's version and identity; tell it the verdict.
async fn handshake_accept(
    shared: &Arc<Shared>,
    conn: &quinn::Connection,
    remote: SocketAddr,
) -> Result<Option<Instance>, Error> {
    let (mut reply, mut hello) = conn.accept_bi().await?;
    let mut raw = [0u8; 4];
    tokio::time::timeout(CONNECT_TIMEOUT, hello.read_exact(&mut raw))
        .await
        .map_err(|_| Error::NoHandshake)??;
    // What follows the version, if anything, names the dialer's run. Dialers from before it
    // was added send nothing more, and ones from after it send nothing a refusal needs.
    let rest = tokio::time::timeout(CONNECT_TIMEOUT, hello.read_to_end(64))
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    let instance = Instance::try_from(rest.as_slice()).ok();
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
        Err(why) => {
            let mut answer = [&[1][..], why.refusal_text().as_bytes()].concat();
            // A peer on another protocol cannot hear any message of ours, so the notice of
            // which release the cluster is on (ADR 0006) rides the refusal, after the text.
            if matches!(why, Error::VersionMismatch { .. })
                && let Some(notice) = shared.notice.lock().unwrap().clone()
            {
                answer.extend(notice);
            }
            answer
        }
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
    verdict.map(|()| instance)
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
        // The variant is in the first two bytes: read them, and hold a chunk message back while
        // the inbox is over its limit, before its body takes any memory. Each chunk is a stream
        // of its own, so the check cannot wait until after the read.
        let mut head = [0u8; 2];
        let head = &mut head[..len.min(2)];
        if stream.read_exact(head).await.is_err() {
            return;
        }
        let bulk = Message::is_bulk(head);
        while bulk && shared.inbox_bytes.load(Ordering::Relaxed) > INBOX_BULK_LIMIT {
            tokio::time::sleep(INBOX_POLL).await;
        }
        let mut body = head.to_vec();
        body.resize(len, 0);
        if stream.read_exact(&mut body[head.len()..]).await.is_err() {
            return;
        }
        let background = Message::is_background(&body);
        shared.inbox_messages.fetch_add(1, Ordering::Relaxed);
        shared
            .inbox_bytes
            .fetch_add(body.len() as u64, Ordering::Relaxed);
        #[allow(
            clippy::let_underscore_must_use,
            reason = "best-effort: the daemon may have stopped receiving"
        )]
        let _ = shared.inbox.lock().unwrap().send((from, body));
        if !background {
            shared.wake();
        }
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
    peer: NodeId,
    addr: SocketAddr,
) -> Result<Link, Error> {
    shared.connects.fetch_add(1, Ordering::Relaxed);
    shared.authorize(addr).await?;
    let attempt = async {
        let conn = endpoint.connect(addr, "startfs")?.await?;
        let (mut hello, mut reply) = conn.open_bi().await?;
        hello
            .write_all(&[&MAGIC[..], &[shared.version], &shared.instance[..]].concat())
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
                // The text may be followed by an upgrade notice, which starts with a byte
                // no text holds.
                let (text, notice) = match why.iter().position(|b| *b == NOTICE_BYTE) {
                    Some(at) => (&why[..at], Some(why[at..].to_vec())),
                    None => (why, None),
                };
                if let Some(notice) = notice {
                    shared.deliver(peer, notice);
                }
                return Err(Error::Refused(String::from_utf8_lossy(text).into_owned()));
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
    mut queue: tokio::sync::mpsc::UnboundedReceiver<Outgoing>,
) {
    let mut link: Option<Link> = None;
    let mut failed_at: Option<Instant> = None;
    while let Some((msg, held)) = queue.recv().await {
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
            match connect(&shared, &endpoint, peer, addr).await {
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
                let _held = held;
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

fn new_instance() -> std::io::Result<Instance> {
    let mut instance = Instance::default();
    ring::default_provider()
        .secure_random
        .fill(&mut instance)
        .map_err(|_| std::io::Error::other("no random bytes for the instance"))?;
    Ok(instance)
}

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
