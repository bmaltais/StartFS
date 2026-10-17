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
use std::time::Duration;

/// Identify peers with the `tailscaled` the config names (or the default one).
pub fn tailscale_whois(config: &Config) -> Arc<dyn Whois> {
    let socket = config
        .whois_socket
        .as_deref()
        .unwrap_or(TailscaleWhois::DEFAULT_SOCKET);
    Arc::new(TailscaleWhois::new(Path::new(socket)))
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

/// Drives [`Daemon::tick`] on a background thread until dropped.
pub struct Ticker {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Ticker {
    /// Attach the transport to the daemon and start ticking.
    pub fn start(daemon: Arc<Daemon>, transport: Arc<QuicTransport>) -> Self {
        daemon.attach(transport);
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("startfs-tick".into())
                .spawn(move || {
                    while !stop.load(Ordering::SeqCst) {
                        let busy = daemon.tick();
                        std::thread::sleep(Duration::from_millis(if busy { 1 } else { 10 }));
                    }
                })
                .expect("spawn tick thread")
        };
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
