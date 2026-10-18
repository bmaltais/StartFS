//! Puts a daemon on the network: binds the QUIC transport and runs the tick loop.

use crate::identity::{TailscaleWhois, Whois};
use crate::quic::QuicTransport;
use startfs_core::config::Config;
use startfs_core::daemon::Daemon;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Identify peers with the `tailscaled` the config names (or the default one).
pub fn tailscale_whois(config: &Config) -> Arc<dyn Whois> {
    let socket = config
        .whois_socket
        .as_deref()
        .unwrap_or(TailscaleWhois::DEFAULT_SOCKET);
    Arc::new(TailscaleWhois::new(Path::new(socket)))
}

/// The MagicDNS name in `tailscale status --json` output, without its
/// trailing dot; `None` when MagicDNS is off or the output is unusable.
pub fn dns_name(status_json: &str) -> Option<String> {
    let status: serde_json::Value = serde_json::from_str(status_json).ok()?;
    let name = status.get("Self")?.get("DNSName")?.as_str()?;
    let name = name.trim_end_matches('.');
    (!name.is_empty()).then(|| name.to_string())
}

/// Listen on `host` at the configured port (0 picks a free one), and record
/// the port actually bound in `config`, since join tokens carry it.
pub fn bind(
    config: &mut Config,
    host: IpAddr,
    whois: Arc<dyn Whois>,
) -> std::io::Result<Arc<QuicTransport>> {
    let transport = QuicTransport::start(
        SocketAddr::new(host, config.port),
        whois,
        config.allowed_tags.clone(),
    )?;
    config.port = transport.local_addr().port();
    Ok(Arc::new(transport))
}

/// A tick longer than this is logged: it delays everything that waits on it.
const SLOW_TICK: Duration = Duration::from_secs(2);

/// Drives [`Daemon::tick`] on a background thread until dropped, and
/// [`Daemon::keepalive`] on another so a slow tick does not make the node look
/// down to its peers.
pub struct Ticker {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl Ticker {
    /// Attach the transport to the daemon and start ticking.
    pub fn start(daemon: Arc<Daemon>, transport: Arc<QuicTransport>) -> Self {
        daemon.attach(transport);
        let stop = Arc::new(AtomicBool::new(false));
        let tick = {
            let (stop, daemon) = (stop.clone(), daemon.clone());
            std::thread::Builder::new()
                .name("startfs-tick".into())
                .spawn(move || {
                    while !stop.load(Ordering::SeqCst) {
                        let started = Instant::now();
                        let busy = daemon.tick();
                        if started.elapsed() >= SLOW_TICK {
                            eprintln!(
                                "startfs: a background tick took {:.1}s",
                                started.elapsed().as_secs_f64()
                            );
                        }
                        std::thread::sleep(Duration::from_millis(if busy { 1 } else { 10 }));
                    }
                })
                .expect("spawn tick thread")
        };
        let keepalive = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("startfs-keepalive".into())
                .spawn(move || {
                    while !stop.load(Ordering::SeqCst) {
                        let _hello_sent = daemon.keepalive();
                        std::thread::sleep(Duration::from_millis(250));
                    }
                })
                .expect("spawn keepalive thread")
        };
        Self {
            stop,
            threads: vec![tick, keepalive],
        }
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        for t in self.threads.drain(..) {
            #[allow(
                clippy::let_underscore_must_use,
                reason = "teardown: a panicked thread has nothing left to report"
            )]
            let _ = t.join();
        }
    }
}
