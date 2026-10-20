//! Puts a daemon on the network: binds the QUIC transport and runs the tick loop.

use crate::identity::{TailscaleWhois, Whois};
use crate::quic::QuicTransport;
use startfs_core::config::Config;
use startfs_core::daemon::{BUSY_WAIT, Daemon};
use startfs_core::wake::Waker;
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
///
/// Neither polls. The tick sleeps until a message arrives, a write leaves work, or
/// [`Daemon::idle_wait`] is up, so a quiet node wakes a few times a second instead
/// of a hundred.
pub struct Ticker {
    stop: Arc<AtomicBool>,
    wakers: [Arc<Waker>; 2],
    threads: Vec<JoinHandle<()>>,
}

impl Ticker {
    /// Attach the transport to the daemon and start ticking.
    pub fn start(daemon: Arc<Daemon>, transport: Arc<QuicTransport>) -> Self {
        let tick_waker = Arc::new(Waker::default());
        let keepalive_waker = Arc::new(Waker::default());
        transport.set_waker(tick_waker.clone());
        daemon.fs().set_waker(tick_waker.clone());
        daemon.attach(transport);
        let stop = Arc::new(AtomicBool::new(false));
        let tick = {
            let (stop, daemon, waker) = (stop.clone(), daemon.clone(), tick_waker.clone());
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
                        if busy {
                            std::thread::sleep(Duration::from_millis(1));
                        } else if waker.wait(daemon.idle_wait()) {
                            // Woken: rounds still come no closer than 10 ms, so a stream of
                            // writes or messages is handled in batches, not one round each.
                            std::thread::sleep(BUSY_WAIT.saturating_sub(started.elapsed()));
                        }
                    }
                })
                .expect("spawn tick thread")
        };
        let keepalive = {
            let (stop, waker) = (stop.clone(), keepalive_waker.clone());
            std::thread::Builder::new()
                .name("startfs-keepalive".into())
                .spawn(move || {
                    while !stop.load(Ordering::SeqCst) {
                        let _hello_sent = daemon.keepalive();
                        waker.wait(daemon.keepalive_wait());
                    }
                })
                .expect("spawn keepalive thread")
        };
        Self {
            stop,
            wakers: [tick_waker, keepalive_waker],
            threads: vec![tick, keepalive],
        }
    }
}

/// How long a stop waits for a tick that is mid-work (it may be waiting on a
/// peer) before it gives up on the thread.
const TICK_GRACE: Duration = Duration::from_secs(3);

impl Drop for Ticker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        for w in &self.wakers {
            w.wake();
        }
        let started = Instant::now();
        for t in self.threads.drain(..) {
            while !t.is_finished() && started.elapsed() < TICK_GRACE {
                std::thread::sleep(Duration::from_millis(10));
            }
            if t.is_finished() {
                #[allow(
                    clippy::let_underscore_must_use,
                    reason = "teardown: a panicked thread has nothing left to report"
                )]
                let _ = t.join();
            } else {
                eprintln!(
                    "startfs: a background thread is still running after {TICK_GRACE:?}; not waiting for it"
                );
            }
        }
    }
}
