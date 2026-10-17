//! Whole daemons (real filesystems, real tick loops) talking over loopback QUIC.

use startfs::identity::{PeerIdentity, StaticWhois, Whois};
use startfs::node::{self, Ticker};
use startfs_core::clock::SystemClock;
use startfs_core::config::Config;
use startfs_core::control::{Request, Response};
use startfs_core::daemon::Daemon;
use startfs_core::fs::NodeConfig;
use startfs_core::members::MemberId;
use startfs_core::ops::*;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Node {
    daemon: Arc<Daemon>,
    _ticker: Ticker,
    _dir: tempfile::TempDir,
}

impl Node {
    fn start(last: u8, whois: Arc<dyn Whois>, offer_bytes: u64) -> Node {
        let host = IpAddr::from([127, 0, 0, last]);
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config {
            port: 0,
            offer_bytes,
            // These tests write without closing the file and expect the next tick to commit it.
            write_commit_delay_ms: 0,
            ..Config::default()
        };
        let transport = node::bind(&mut config, host, whois).unwrap();
        let daemon = Daemon::open(
            dir.path(),
            config,
            NodeConfig {
                node: MemberId(0),
                uid: 1000,
                gid: 1000,
            },
            Arc::new(SystemClock),
            host.to_string(),
        )
        .unwrap();
        let ticker = Ticker::start(daemon.clone(), transport);
        Node {
            daemon,
            _ticker: ticker,
            _dir: dir,
        }
    }

    fn call(&self, req: Request) -> Response {
        self.daemon.handle_control_request(req)
    }

    fn create(&self, copies: u32) -> String {
        match self.call(Request::Create {
            name: "home".into(),
            copies,
        }) {
            Response::Created { token } => token,
            other => panic!("create: {other:?}"),
        }
    }

    fn join(&self, token: &str) -> Response {
        self.call(Request::Join {
            token: token.into(),
            offer_bytes: None,
        })
    }

    fn status(&self) -> startfs_core::control::Status {
        match self.call(Request::Status) {
            Response::Status(s) => *s,
            other => panic!("status: {other:?}"),
        }
    }
}

fn tailnet() -> Arc<StaticWhois> {
    let w = Arc::new(StaticWhois::default());
    for last in 1..=3 {
        w.add(
            IpAddr::from([127, 0, 0, last]),
            PeerIdentity {
                name: format!("node{last}"),
                tags: vec![],
            },
        );
    }
    w
}

fn wait_until(what: &str, mut ok: impl FnMut() -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(60) {
        if ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}");
}

fn write_file(fs: &impl FsOps, name: &str, data: &[u8]) {
    let f = fs.create(ROOT_INO, name, 0o644).unwrap();
    fs.write(f.ino, 0, data).unwrap();
}

fn read_file(fs: &impl FsOps, name: &str) -> Option<Vec<u8>> {
    let f = fs.lookup(ROOT_INO, name).ok()?;
    fs.read(f.ino, 0, f.size as u32).ok()
}

#[test]
fn a_joined_node_sees_metadata_and_reads_chunks_from_its_peer() {
    let whois = tailnet();
    let a = Node::start(1, whois.clone(), 1 << 30);
    let token = a.create(1);

    // B is a pure client: it stores nothing, so every read is a remote fetch.
    let b = Node::start(2, whois, 0);
    let joined = b.call(Request::Join {
        token,
        offer_bytes: Some(0),
    });
    assert!(matches!(joined, Response::Joined), "{joined:?}");

    let body: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
    write_file(&*a.daemon.fs(), "report.bin", &body);

    wait_until("the file to appear on B", || {
        read_file(&*b.daemon.fs(), "report.bin").is_some()
    });
    assert!(read_file(&*b.daemon.fs(), "report.bin").unwrap() == body);
    assert_eq!(b.status().members.len(), 2);
}

#[test]
fn new_data_is_replicated_to_the_other_node_over_quic() {
    let whois = tailnet();
    let a = Node::start(1, whois.clone(), 1 << 30);
    let token = a.create(2);
    let b = Node::start(2, whois, 1 << 30);
    assert!(matches!(b.join(&token), Response::Joined));

    write_file(&*a.daemon.fs(), "data.bin", &vec![42u8; 200_000]);

    wait_until("B to hold a replica", || b.status().usage.chunks > 0);
    wait_until("A to see two copies", || a.status().unreplicated.is_empty());
}

#[test]
fn joining_through_a_peer_that_refuses_us_says_why() {
    let a_view = Arc::new(StaticWhois::default());
    a_view.add(
        IpAddr::from([127, 0, 0, 1]),
        PeerIdentity {
            name: "a".into(),
            tags: vec![],
        },
    );
    let a = Node::start(1, a_view, 1 << 30);
    let token = a.create(1);
    let b = Node::start(2, tailnet(), 1 << 30);
    let Response::Error(why) = b.join(&token) else {
        panic!("joined despite being unknown to A");
    };
    assert!(why.contains("not a known tailnet machine"), "{why}");
}
