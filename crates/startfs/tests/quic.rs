//! Smoke tests of the QUIC transport over loopback, with a faked `tailscaled` whois.

#![allow(
    clippy::let_underscore_must_use,
    reason = "test teardown and best-effort replies"
)]

use startfs::identity::{PeerIdentity, StaticWhois};
use startfs::quic::QuicTransport;
use startfs_core::net::{NodeId, Transport};
use startfs_core::protocol::{Message, PROTOCOL_VERSION, UpgradeNotice};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn ip(last: u8) -> IpAddr {
    IpAddr::from([127, 0, 0, last])
}

fn peer(tags: &[&str]) -> PeerIdentity {
    PeerIdentity {
        name: "node".into(),
        tags: tags.iter().map(|t| t.to_string()).collect(),
    }
}

fn node(last: u8, whois: &Arc<StaticWhois>, allowed: &[&str]) -> QuicTransport {
    QuicTransport::start(
        SocketAddr::new(ip(last), 0),
        whois.clone(),
        allowed.iter().map(|t| t.to_string()).collect(),
    )
    .unwrap()
}

fn recv_within(t: &QuicTransport, wait: Duration) -> Option<(NodeId, Vec<u8>)> {
    let start = Instant::now();
    while start.elapsed() < wait {
        if let Some(m) = t.recv() {
            return Some(m);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    None
}

fn tailnet() -> Arc<StaticWhois> {
    let w = Arc::new(StaticWhois::default());
    for last in 1..=3 {
        w.add(ip(last), peer(&[]));
    }
    w
}

#[test]
fn a_message_travels_between_two_nodes_and_names_its_sender() {
    let whois = tailnet();
    let a = node(1, &whois, &[]);
    let b = node(2, &whois, &[]);
    let b_at_a = a.resolve(&b.local_addr().to_string()).unwrap();
    assert_eq!(b_at_a, b.local());

    a.send(b_at_a, b"hello".to_vec());
    let (from, msg) = recv_within(&b, Duration::from_secs(5)).expect("delivered");
    assert_eq!(from, a.local());
    assert_eq!(msg, b"hello");

    // The reply finds its way back without anyone resolving an address.
    b.send(from, b"hi".to_vec());
    let (from, msg) = recv_within(&a, Duration::from_secs(5)).expect("replied");
    assert_eq!((from, msg.as_slice()), (b.local(), &b"hi"[..]));
}

fn wait_for_error(t: &QuicTransport, about: NodeId, needle: &str) -> String {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if let Some(e) = t.last_error(about) {
            assert!(e.contains(needle), "unexpected error: {e}");
            return e;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("no error recorded, wanted one mentioning {needle:?}");
}

#[test]
fn a_peer_unknown_to_tailscale_is_rejected() {
    let a_view = Arc::new(StaticWhois::default());
    a_view.add(ip(1), peer(&[]));
    a_view.add(ip(2), peer(&[]));
    let a = node(1, &a_view, &[]);
    let b = node(2, &a_view, &[]);
    let c = node(3, &tailnet(), &[]); // c knows everyone; a does not know c

    let a_at_c = c.resolve(&a.local_addr().to_string()).unwrap();
    c.send(a_at_c, b"let me in".to_vec());
    let why = wait_for_error(&c, a_at_c, "not a known tailnet machine");
    assert!(why.contains("127.0.0.3"), "{why}");
    assert!(recv_within(&a, Duration::from_millis(300)).is_none());

    // Known peers are unaffected.
    let a_at_b = b.resolve(&a.local_addr().to_string()).unwrap();
    b.send(a_at_b, b"hi".to_vec());
    assert!(recv_within(&a, Duration::from_secs(5)).is_some());
}

#[test]
fn only_peers_with_an_allowed_tag_get_in() {
    let whois = Arc::new(StaticWhois::default());
    whois.add(ip(1), peer(&["tag:startfs"]));
    whois.add(ip(2), peer(&["tag:startfs", "tag:other"]));
    whois.add(ip(3), peer(&["tag:other"]));
    let a = node(1, &whois, &["tag:startfs"]);
    let b = node(2, &whois, &[]);
    let c = node(3, &whois, &[]);

    let a_at_c = c.resolve(&a.local_addr().to_string()).unwrap();
    c.send(a_at_c, b"hello".to_vec());
    wait_for_error(&c, a_at_c, "tag:startfs");
    assert!(recv_within(&a, Duration::from_millis(300)).is_none());

    let a_at_b = b.resolve(&a.local_addr().to_string()).unwrap();
    b.send(a_at_b, b"hello".to_vec());
    assert!(recv_within(&a, Duration::from_secs(5)).is_some());

    // Dialing out is held to the same policy.
    let d = node(2, &whois, &["tag:startfs"]);
    let c_at_d = d.resolve(&c.local_addr().to_string()).unwrap();
    d.send(c_at_d, b"hello".to_vec());
    wait_for_error(&d, c_at_d, "tag:startfs");
}

#[test]
fn a_peer_speaking_another_protocol_version_fails_with_a_clear_error() {
    let whois = tailnet();
    let a = node(1, &whois, &[]);
    let old = QuicTransport::start_versioned(
        SocketAddr::new(ip(2), 0),
        whois.clone(),
        vec![],
        startfs_core::protocol::PROTOCOL_VERSION + 1,
    )
    .unwrap();

    let a_at_old = old.resolve(&a.local_addr().to_string()).unwrap();
    old.send(a_at_old, b"hello".to_vec());
    let why = wait_for_error(&old, a_at_old, "protocol version mismatch");
    // Worded from the dialer's side: the peer (a) is older, so the dialer is told to upgrade the peer.
    let (ours, theirs) = (PROTOCOL_VERSION + 1, PROTOCOL_VERSION);
    assert!(
        why.contains(&format!(
            "the peer speaks version {theirs}, this node speaks version {ours}; upgrade the peer"
        )),
        "{why}"
    );
    assert!(recv_within(&a, Duration::from_millis(300)).is_none());
    // The accepting side says so too.
    wait_for_error(&a, old.local(), "protocol version mismatch");
}

#[test]
fn a_peer_refused_for_its_protocol_version_is_handed_the_upgrade_notice() {
    let whois = tailnet();
    let a = node(1, &whois, &[]);
    let old = QuicTransport::start_versioned(
        SocketAddr::new(ip(2), 0),
        whois.clone(),
        vec![],
        PROTOCOL_VERSION - 1,
    )
    .unwrap();
    let notice = Message::UpgradeNotice(UpgradeNotice {
        release: "v99.0.0".into(),
        protocol: PROTOCOL_VERSION,
    })
    .encode();
    a.set_notice(Some(notice.clone()));

    let a_at_old = old.resolve(&a.local_addr().to_string()).unwrap();
    old.send(a_at_old, b"hello".to_vec());
    // The refusal still says why, in plain words...
    let why = wait_for_error(&old, a_at_old, "protocol version mismatch");
    assert!(why.is_ascii(), "{why:?}");
    // ...and brings the notice, as if the refusing node had sent it.
    let (from, msg) = recv_within(&old, Duration::from_secs(5)).expect("the notice");
    assert_eq!((from, msg), (a.local(), notice));
    assert!(recv_within(&a, Duration::from_millis(300)).is_none());
}

#[test]
fn a_huge_message_arrives_whole() {
    let whois = tailnet();
    let a = node(1, &whois, &[]);
    let b = node(2, &whois, &[]);
    let b_at_a = a.resolve(&b.local_addr().to_string()).unwrap();
    let big: Vec<u8> = (0..48u32 << 20).map(|i| (i % 251) as u8).collect();
    a.send(b_at_a, big.clone());
    let (_, got) = recv_within(&b, Duration::from_secs(60)).expect("delivered");
    assert!(got == big);
}

#[test]
fn a_bulk_chunk_transfer_does_not_stall_metadata() {
    use startfs_core::protocol::Message;
    let whois = tailnet();
    let a = node(1, &whois, &[]);
    let b = node(2, &whois, &[]);
    let b_at_a = a.resolve(&b.local_addr().to_string()).unwrap();
    // Open the connection first so only the transfer itself is measured.
    a.send(b_at_a, Message::Members(vec![]).encode());
    recv_within(&b, Duration::from_secs(5)).expect("connected");

    let id = [9u8; 32];
    let bulk = Message::ChunkPush(id, Some(vec![0xAB; 96 << 20])).encode();
    let small = Message::Members(vec![]).encode();
    a.send(b_at_a, bulk.clone());
    a.send(b_at_a, small.clone());

    let (_, first) = recv_within(&b, Duration::from_secs(60)).expect("something arrives");
    assert_eq!(first, small, "metadata waited behind the chunk transfer");
    let (_, second) = recv_within(&b, Duration::from_secs(60)).expect("chunk arrives");
    assert!(second == bulk);
}

#[test]
fn a_receiver_that_falls_behind_holds_a_bounded_inbox_and_still_hears_metadata() {
    use startfs::quic::INBOX_BULK_LIMIT;
    use startfs_core::protocol::Message;
    let whois = tailnet();
    let a = node(1, &whois, &[]);
    let b = node(2, &whois, &[]);
    let b_at_a = a.resolve(&b.local_addr().to_string()).unwrap();
    let small = Message::Members(vec![]).encode();
    a.send(b_at_a, small.clone());
    recv_within(&b, Duration::from_secs(5)).expect("connected");

    // Twice the limit in chunk messages, none of which `b` takes off its inbox yet.
    let each = 16 << 20;
    let count = 2 * INBOX_BULK_LIMIT as usize / each;
    for i in 0..count {
        a.send(
            b_at_a,
            Message::ChunkPush([i as u8; 32], Some(vec![0xAB; each])).encode(),
        );
    }
    std::thread::sleep(Duration::from_secs(3));
    let held = b.inbox().unwrap().bytes;
    assert!(
        held <= INBOX_BULK_LIMIT + each as u64 + 1024,
        "{held} bytes in the inbox"
    );

    // Metadata is not held up behind the chunks.
    a.send(b_at_a, small.clone());
    let mut chunks = 0;
    let mut saw_small = false;
    let start = Instant::now();
    while chunks < count && start.elapsed() < Duration::from_secs(60) {
        match recv_within(&b, Duration::from_secs(5)) {
            Some((_, m)) if m == small => saw_small = true,
            Some(_) => chunks += 1,
            None => {}
        }
    }
    assert!(saw_small, "metadata never arrived");
    assert_eq!(chunks, count, "chunks were dropped, not slowed");
}

// ---- the real whois client, against a stand-in for tailscaled's local API ----

mod local_api {
    use startfs::identity::{TailscaleWhois, Whois};
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::path::Path;

    /// Answers one request per connection with `status` and `body`; returns
    /// the request lines it saw.
    fn serve(socket: &Path, status: &str, body: &str) -> std::sync::mpsc::Receiver<String> {
        let listener = UnixListener::bind(socket).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let reply = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                // The whole request first: closing with bytes unread resets the connection.
                let mut buf = [0u8; 4096];
                let mut n = 0;
                while !buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                    n += stream.read(&mut buf[n..]).unwrap();
                }
                let request = String::from_utf8_lossy(&buf[..n]).into_owned();
                let _ = tx.send(request.lines().next().unwrap().to_string());
                stream.write_all(reply.as_bytes()).unwrap();
            }
        });
        rx
    }

    #[test]
    fn asks_tailscaled_who_a_peer_is_and_reads_its_tags() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("tailscaled.sock");
        let seen = serve(
            &socket,
            "200 OK",
            r#"{"Node":{"ID":5,"Name":"laptop.tail1234.ts.net.","Tags":["tag:startfs","tag:other"]},"UserProfile":{"LoginName":"tagged-devices"},"CapMap":null}"#,
        );
        let peer = TailscaleWhois::new(&socket)
            .lookup("100.64.0.7:7600".parse().unwrap())
            .unwrap();
        assert_eq!(peer.name, "laptop.tail1234.ts.net.");
        assert_eq!(peer.tags, ["tag:startfs", "tag:other"]);
        assert_eq!(
            seen.recv().unwrap(),
            "GET /localapi/v0/whois?addr=100.64.0.7:7600 HTTP/1.0"
        );
    }

    /// Run by hand on a machine with Tailscale up:
    /// `cargo test -p startfs --test quic real_tailscaled -- --ignored`
    #[test]
    #[ignore = "needs a running tailscaled"]
    fn real_tailscaled_identifies_this_machine() {
        let me = std::process::Command::new("tailscale")
            .args(["ip", "-4"])
            .output()
            .expect("tailscale installed");
        let ip = String::from_utf8(me.stdout).unwrap();
        let addr = format!("{}:7600", ip.lines().next().unwrap().trim());
        let peer = TailscaleWhois::new(Path::new(TailscaleWhois::DEFAULT_SOCKET))
            .lookup(addr.parse().unwrap())
            .unwrap();
        assert!(peer.name.contains(".ts.net"), "{peer:?}");
    }

    #[test]
    fn an_untagged_machine_has_no_tags_and_an_unknown_address_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("a.sock");
        serve(
            &socket,
            "200 OK",
            r#"{"Node":{"Name":"desk.ts.net.","Tags":null},"UserProfile":{}}"#,
        );
        let peer = TailscaleWhois::new(&socket)
            .lookup("100.64.0.8:1".parse().unwrap())
            .unwrap();
        assert!(peer.tags.is_empty());

        let socket = dir.path().join("b.sock");
        serve(
            &socket,
            "404 Not Found",
            r#"{"error":"no match for \"8.8.8.8\""}"#,
        );
        let err = TailscaleWhois::new(&socket)
            .lookup("8.8.8.8:1".parse().unwrap())
            .unwrap_err();
        assert!(err.to_string().contains("8.8.8.8"), "{err}");

        let err = TailscaleWhois::new(&dir.path().join("missing.sock"))
            .lookup("100.64.0.9:1".parse().unwrap())
            .unwrap_err();
        assert!(err.to_string().contains("tailscaled"), "{err}");
    }
}

#[test]
fn a_name_with_both_ipv4_and_ipv6_addresses_resolves_to_the_ipv4_one() {
    // Nodes listen on their tailnet IPv4 only, and MagicDNS names carry both families.
    let addr = startfs::quic::resolve_addr("localhost:7600").unwrap();
    assert!(addr.is_ipv4(), "{addr}");
    assert_eq!(addr.port(), 7600);
}

/// Both nodes send to each other every `step` for `span`, as hellos do.
fn chat(a: &QuicTransport, b: &QuicTransport, span: Duration, step: Duration) {
    let (b_at_a, a_at_b) = (b.local(), a.local());
    let start = Instant::now();
    while start.elapsed() < span {
        a.send(b_at_a, b"a".to_vec());
        b.send(a_at_b, b"b".to_vec());
        std::thread::sleep(step);
    }
    while a.recv().is_some() || b.recv().is_some() {}
}

#[test]
fn two_nodes_sending_to_each_other_at_once_settle_on_one_link() {
    let whois = tailnet();
    let a = node(1, &whois, &[]);
    let b = node(2, &whois, &[]);
    a.resolve(&b.local_addr().to_string()).unwrap();
    b.resolve(&a.local_addr().to_string()).unwrap();

    chat(&a, &b, Duration::from_secs(8), Duration::from_millis(250));

    assert!(
        a.connects() + b.connects() <= 4,
        "links kept being redialed: a opened {}, b opened {}",
        a.connects(),
        b.connects()
    );
}

#[test]
fn a_restarted_peer_does_not_set_the_two_nodes_redialing_each_other() {
    let whois = tailnet();
    let a = node(1, &whois, &[]);
    let b = node(2, &whois, &[]);
    let b_addr = b.local_addr();
    a.resolve(&b_addr.to_string()).unwrap();
    b.resolve(&a.local_addr().to_string()).unwrap();
    chat(&a, &b, Duration::from_secs(2), Duration::from_millis(250));

    drop(b);
    // The old socket may take a moment to release the port.
    let mut tries = 0;
    let b = loop {
        match QuicTransport::start(b_addr, whois.clone(), vec![]) {
            Ok(b) => break b,
            Err(e) if tries < 50 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(100));
                drop(e);
            }
            Err(e) => panic!("cannot rebind {b_addr}: {e}"),
        }
    };
    b.resolve(&a.local_addr().to_string()).unwrap();
    let before = a.connects() + b.connects();
    // Hearing from the new b drops a's link to the old one, so a reaches b at once rather
    // than after the idle timeout.
    b.send(a.local(), b"back".to_vec());
    assert!(recv_within(&a, Duration::from_secs(5)).is_some());
    a.send(b.local(), b"welcome".to_vec());
    assert!(recv_within(&b, Duration::from_secs(5)).is_some());
    chat(&a, &b, Duration::from_secs(12), Duration::from_millis(250));

    assert!(
        a.connects() + b.connects() - before <= 4,
        "links kept being redialed after the restart: a opened {}, b opened {}",
        a.connects(),
        b.connects()
    );
}

#[test]
fn one_dropped_link_does_not_set_the_two_nodes_redialing_each_other() {
    let whois = tailnet();
    let a = node(1, &whois, &[]);
    let b = node(2, &whois, &[]);
    a.resolve(&b.local_addr().to_string()).unwrap();
    b.resolve(&a.local_addr().to_string()).unwrap();
    chat(&a, &b, Duration::from_secs(2), Duration::from_millis(250));

    a.sever_link(b.local());
    let before = a.connects() + b.connects();
    chat(&a, &b, Duration::from_secs(12), Duration::from_millis(250));

    assert!(
        a.connects() + b.connects() - before <= 2,
        "links kept being redialed after one dropped: a opened {}, b opened {}",
        a.connects(),
        b.connects()
    );
}
