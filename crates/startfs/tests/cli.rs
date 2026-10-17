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

struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn cli_creates_a_filesystem_and_reports_status_through_the_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path();

    // No daemon yet: a clear failure, not a hang or panic.
    let out = startfs(data, &["status"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot reach daemon"));

    let _daemon = Daemon(
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

    let out = startfs(data, &["create", "--name", "home", "--copies", "3"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let token = String::from_utf8(out.stdout).unwrap();
    assert!(token.trim().starts_with("startfs1-"), "{token}");

    let out = startfs(data, &["status"]);
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    for needle in ["home", "3 copies", "online", "1 of 3"] {
        assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
    }
}
