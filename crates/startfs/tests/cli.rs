//! Smoke test of the real binary: one process as daemon, others as CLI.

#![allow(
    clippy::let_underscore_must_use,
    reason = "test teardown and best-effort replies"
)]

use std::path::Path;
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};

fn startfs(data: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_startfs"))
        .arg("--data-dir")
        .arg(data)
        .args(args)
        .output()
        .unwrap()
}

fn stdout(out: &Output) -> String {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout.clone()).unwrap()
}

struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A stand-in for `tailscaled`'s local API: every address is a tailnet
/// machine carrying `tag:startfs`, unless `known` is false (then nobody is).
fn fake_tailscaled(socket: &Path, known: bool) {
    use std::io::{Read, Write};
    let listener = std::os::unix::net::UnixListener::bind(socket).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            // Read the whole request head before answering: closing with
            // unread bytes makes the kernel reset the connection under load.
            let mut head = Vec::new();
            let mut buf = [0u8; 4096];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => head.extend_from_slice(&buf[..n]),
                }
            }
            let (status, body) = if known {
                (
                    "200 OK",
                    r#"{"Node":{"Name":"peer.ts.net.","Tags":["tag:startfs"]}}"#,
                )
            } else {
                ("404 Not Found", r#"{"error":"no match"}"#)
            };
            let _ = write!(
                stream,
                "HTTP/1.0 {status}\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
        }
    });
}

fn start_daemon(data: &Path) -> Daemon {
    start_daemon_with(data, true, "")
}

fn start_daemon_with(data: &Path, tailnet_knows_peers: bool, extra_config: &str) -> Daemon {
    std::fs::create_dir_all(data).unwrap();
    let whois = data.join("whois.sock");
    fake_tailscaled(&whois, tailnet_knows_peers);
    let config = data.join("test-config.toml");
    std::fs::write(
        &config,
        format!(
            "port = 0\nwhois_socket = \"{}\"\n{extra_config}",
            whois.display()
        ),
    )
    .unwrap();
    let daemon = Daemon(
        Command::new(env!("CARGO_BIN_EXE_startfs"))
            .arg("--data-dir")
            .arg(data)
            .arg("--config")
            .arg(&config)
            .arg("daemon")
            .spawn()
            .unwrap(),
    );
    let started = Instant::now();
    while !data.join("control.sock").exists() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "daemon did not start"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    daemon
}

#[test]
fn version_flag_prints_the_crate_version_without_a_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let out = startfs(dir.path(), &["--version"]);
    assert!(out.status.success());
    assert_eq!(
        stdout(&out).trim(),
        format!(
            "startfs {} (protocol {})",
            env!("CARGO_PKG_VERSION"),
            startfs_core::protocol::PROTOCOL_VERSION
        )
    );
}

#[test]
fn cli_creates_a_filesystem_and_reports_status_through_the_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path();

    // No daemon yet: a clear failure, not a hang or panic.
    let out = startfs(data, &["status"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot reach daemon"));

    let _daemon = start_daemon(data);
    let out = startfs(data, &["create", "--name", "home", "--copies", "3"]);
    let token = stdout(&out);
    assert!(token.trim().starts_with("startfs1-"), "{token}");

    let text = stdout(&startfs(data, &["status"]));
    for needle in [
        "home",
        "3 copies",
        "online",
        "1 of 3",
        "3 wanted, 1 possible",
        "usable",
        "storage: no files yet",
    ] {
        assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
    }

    // The same answer as JSON, for scripts and GUIs.
    let json: serde_json::Value =
        serde_json::from_str(&stdout(&startfs(data, &["status", "--json"]))).unwrap();
    assert_eq!(json["filesystem"]["name"], "home");
    assert_eq!(json["filesystem"]["copies"], 3);
    assert_eq!(json["copies_possible"], 1);
    assert_eq!(json["efficiency"]["logical_bytes"], 0);
    assert_eq!(json["members"].as_array().unwrap().len(), 1);
}

#[test]
fn cli_issues_and_revokes_tokens_sets_the_offer_and_lists_members() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path();
    let _daemon = start_daemon(data);

    // Nothing to issue before a filesystem exists.
    assert!(!startfs(data, &["token"]).status.success());
    stdout(&startfs(
        data,
        &["create", "--name", "home", "--copies", "2"],
    ));
    let token = stdout(&startfs(data, &["token"])).trim().to_string();
    assert!(token.starts_with("startfs1-"), "{token}");
    let example = stdout(&startfs(data, &["token", "--example"]));
    assert!(
        example.trim().starts_with("startfs join startfs1-"),
        "{example}"
    );
    stdout(&startfs(data, &["revoke", &token]));

    stdout(&startfs(data, &["config", "--offer", "50G"]));
    let text = stdout(&startfs(data, &["status"]));
    assert!(text.contains("members:"), "{text}");
    assert!(
        text.contains("this node") && text.contains("50.0 GiB"),
        "{text}"
    );

    // A bad token is refused with a clear message.
    let out = startfs(data, &["join", "not-a-token"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a StartFS join token"));
}

#[test]
fn cli_leaves_and_removes_members() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path();
    let _daemon = start_daemon(data);
    assert!(!startfs(data, &["leave"]).status.success());
    stdout(&startfs(
        data,
        &["create", "--name", "home", "--copies", "2"],
    ));

    // Members are named by their number in `status`; this node cannot remove itself.
    assert!(!startfs(data, &["remove", "0"]).status.success());
    assert!(!startfs(data, &["remove", "9"]).status.success());
    assert!(!startfs(data, &["remove", "bob"]).status.success());

    stdout(&startfs(data, &["leave"]));
    // The daemon now runs its background work, so a lone member, which has
    // nothing to hand over, finishes leaving at once.
    let started = Instant::now();
    loop {
        let text = stdout(&startfs(data, &["status"]));
        if text.contains("leaving") || text.contains("no filesystem on this node") {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(10), "{text}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn two_daemon_processes_create_join_and_see_each_other_over_quic() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let _a = start_daemon_with(da.path(), true, "allowed_tags = [\"tag:startfs\"]");
    let _b = start_daemon(db.path());

    let token = stdout(&startfs(
        da.path(),
        &["create", "--name", "home", "--copies", "2"],
    ));
    stdout(&startfs(
        db.path(),
        &["join", token.trim(), "--offer", "100M"],
    ));

    // Both ends list both members, and each sees the other as up.
    let started = Instant::now();
    loop {
        let (a, b) = (
            stdout(&startfs(da.path(), &["status"])),
            stdout(&startfs(db.path(), &["status"])),
        );
        let both_up = |t: &str| t.matches(" up ").count() + t.matches("(this node)").count() >= 2;
        if a.contains("(2 nodes)") && b.contains("(2 nodes)") && both_up(&a) && both_up(&b) {
            assert!(b.contains("100.0 MiB"), "{b}");
            // The member rows line up in columns.
            for text in [&a, &b] {
                let rows: Vec<&str> = text.lines().filter(|l| l.starts_with("  ")).collect();
                assert_eq!(rows.len(), 2, "{text}");
                for needle in ["offered", " up "] {
                    let at = |l: &str| l.find(needle);
                    assert!(at(rows[0]).is_some(), "{text}");
                    assert_eq!(at(rows[0]), at(rows[1]), "{text}");
                }
            }
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "members never converged:\n{a}\n{b}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_join_to_an_unidentifiable_peer_fails_with_the_reason() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let _a = start_daemon(da.path());
    let _b = start_daemon_with(db.path(), false, "");
    let token = stdout(&startfs(
        da.path(),
        &["create", "--name", "home", "--copies", "2"],
    ));
    let out = startfs(db.path(), &["join", token.trim()]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("tailscaled does not know"), "{err}");
}

#[test]
fn cli_changes_the_number_of_copies_and_rejects_nonsense() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path();
    let _daemon = start_daemon(data);
    stdout(&startfs(
        data,
        &["create", "--name", "home", "--copies", "2"],
    ));

    // One node cannot hold three copies: the command says so rather than succeeding quietly.
    let out = startfs(data, &["config", "--copies", "3"]);
    assert!(out.status.success());
    let warning = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        warning.contains("warning") && warning.contains("only 1 node"),
        "{warning}"
    );
    let text = stdout(&startfs(data, &["status"]));
    assert!(text.contains("home (3 copies)"), "{text}");

    let out = startfs(data, &["config", "--copies", "three"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--copies must be a number"));
    let out = startfs(data, &["config", "--copies", "0"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("at least 1 copy"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = stdout(&startfs(data, &["status"]));
    assert!(text.contains("home (3 copies)"), "{text}");
}
