//! Smoke test of the real binary: one process as daemon, others as CLI.

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

fn start_daemon(data: &Path) -> Daemon {
    let daemon = Daemon(
        Command::new(env!("CARGO_BIN_EXE_startfs"))
            .arg("--data-dir")
            .arg(data)
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
    ] {
        assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
    }
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
    let text = stdout(&startfs(data, &["status"]));
    assert!(text.contains("leaving"), "{text}");
}
