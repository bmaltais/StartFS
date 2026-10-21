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
    start_daemon_in(data, tailnet_knows_peers, extra_config, &[])
}

/// A daemon with extra environment variables, as the installer's settings reach it.
fn start_daemon_in(
    data: &Path,
    tailnet_knows_peers: bool,
    extra_config: &str,
    env: &[(&str, &Path)],
) -> Daemon {
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
            .envs(env.iter().copied())
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

    // Every member's storage figures, asked for on demand.
    let text = stdout(&startfs(data, &["stats"]));
    assert!(
        text.contains("on disk") && text.contains("(this node)"),
        "{text}"
    );
    let rows: serde_json::Value =
        serde_json::from_str(&stdout(&startfs(data, &["stats", "--json"]))).unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["is_self"], true);
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

#[test]
fn cli_shows_the_nodes_activity_and_switches_its_counters_off() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path();
    let _daemon = start_daemon(data);
    stdout(&startfs(
        data,
        &["create", "--name", "home", "--copies", "1"],
    ));

    let json: serde_json::Value =
        serde_json::from_str(&stdout(&startfs(data, &["activity", "--json"]))).unwrap();
    assert_eq!(json["enabled"], true);
    // The real daemon ticks on the system clock, and reads its own memory
    // from /proc where there is one.
    let text = stdout(&startfs(data, &["activity"]));
    for needle in [
        "peer paths",
        "replication queue: 0 chunks",
        "in memory:",
        "resources:",
    ] {
        assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
    }
    if cfg!(target_os = "linux") {
        assert!(
            json["resources"]["rss_bytes"].as_u64().unwrap() > 0,
            "{json}"
        );
        assert!(json["resources"]["threads"].as_u64().unwrap() > 0);
    }
    // A source the platform lacks reads as unavailable, never as zero.
    if json["resources"]["cgroup"].is_null() {
        assert!(text.contains("cgroup memory: unavailable"), "{text}");
    }
    if json["resources"]["io_pressure"].is_null() {
        assert!(text.contains("io pressure: unavailable"), "{text}");
    } else {
        assert!(text.contains("io pressure: ") && !text.contains("io pressure: unavailable"));
    }
    if json["resources"]["written_bytes"].is_null() {
        assert!(
            text.contains("daemon storage writes: unavailable"),
            "{text}"
        );
    }

    stdout(&startfs(data, &["config", "--activity", "off"]));
    let text = stdout(&startfs(data, &["activity"]));
    assert!(text.contains("counters are off"), "{text}");
    assert!(text.contains("resources:"), "{text}");
    let json: serde_json::Value =
        serde_json::from_str(&stdout(&startfs(data, &["activity", "--json"]))).unwrap();
    assert_eq!(json["enabled"], false);
    assert!(json["tick"].is_null());

    assert!(
        !startfs(data, &["config", "--activity", "maybe"])
            .status
            .success()
    );
}

#[test]
fn the_activity_setting_in_the_config_file_turns_the_counters_off_from_the_start() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path();
    let _daemon = start_daemon_with(data, true, "activity = false\n");
    let json: serde_json::Value =
        serde_json::from_str(&stdout(&startfs(data, &["activity", "--json"]))).unwrap();
    assert_eq!(json["enabled"], false);
}

#[test]
fn cli_purges_deleted_data_without_waiting_for_the_retention() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path();
    let _daemon = start_daemon(data);
    stdout(&startfs(
        data,
        &["create", "--name", "home", "--copies", "1"],
    ));
    let text = stdout(&startfs(data, &["purge"]));
    assert!(text.contains("purge started on 1 member"), "{text}");
    assert!(!text.contains("not reached"), "{text}");
    // It is under way, which `status` says; the harness tests cover how it ends.
    let status = stdout(&startfs(data, &["status"]));
    assert!(status.contains("purge in progress"), "{status}");
}

#[test]
fn help_prints_usage_and_an_unknown_flag_runs_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path();
    let _daemon = start_daemon(data);
    stdout(&startfs(
        data,
        &["create", "--name", "home", "--copies", "1"],
    ));

    let help = stdout(&startfs(data, &["purge", "--help"]));
    assert!(help.contains("usage: startfs"), "{help}");
    let typo = startfs(data, &["purge", "--hlep"]);
    assert!(!typo.status.success());
    assert!(
        String::from_utf8_lossy(&typo.stderr).contains("unexpected argument --hlep"),
        "{}",
        String::from_utf8_lossy(&typo.stderr)
    );
    let stray = startfs(data, &["leave", "now"]);
    assert!(!stray.status.success());

    let status = stdout(&startfs(data, &["status"]));
    assert!(!status.contains("purge in progress"), "{status}");
    assert!(!status.contains("leaving"), "{status}");
}

/// A release of a fake `startfs` that reports the version and the protocol the real one
/// speaks, as the installer script would find it under `base`.
fn publish_fake_release(base: &Path, tag: &str) {
    let target = format!(
        "{}-{}",
        std::env::consts::ARCH,
        if std::env::consts::OS == "macos" {
            "macos"
        } else {
            "linux"
        }
    );
    let name = format!("startfs-{tag}-{target}");
    let pkg = base.join("pkg").join(&name);
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(
        pkg.join("startfs"),
        format!(
            "#!/bin/sh\necho \"startfs {} (protocol {})\"\n",
            tag.trim_start_matches('v'),
            startfs_core::protocol::PROTOCOL_VERSION
        ),
    )
    .unwrap();
    std::fs::write(pkg.join("startfs.service"), "[Service]\n").unwrap();
    let dir = base.join("releases").join(tag);
    std::fs::create_dir_all(&dir).unwrap();
    let script = format!(
        "chmod +x pkg/{name}/startfs && tar czf releases/{tag}/{name}.tar.gz -C pkg {name} \
         && cd releases/{tag} && (sha256sum {name}.tar.gz || shasum -a 256 {name}.tar.gz) > {name}.tar.gz.sha256"
    );
    let out = Command::new("sh")
        .args(["-c", &script])
        .current_dir(base)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn cli_update_stages_the_release_and_has_the_installer_swap_it_in() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let install = dir.path().join("bin");
    let releases = dir.path().join("fake-releases");
    publish_fake_release(&releases, "v99.0.0");
    let base_url =
        std::ffi::OsString::from(format!("file://{}", releases.join("releases").display()));
    let base_url = Path::new(&base_url).to_path_buf();
    let _daemon = start_daemon_in(
        &data,
        true,
        "",
        &[
            ("INSTALL_DIR", &install),
            ("STARTFS_BASE_URL", &base_url),
            // Files only: the test must never touch a service of the machine it runs on.
            ("STARTFS_NO_SERVICE", Path::new("1")),
            ("HOME", dir.path()),
            ("XDG_CONFIG_HOME", &dir.path().join("config")),
        ],
    );
    stdout(&startfs(
        &data,
        &["create", "--name", "home", "--copies", "1"],
    ));

    let said = stdout(&startfs(&data, &["update", "v99.0.0"]));
    assert!(said.contains("moving the cluster to v99.0.0"), "{said}");

    // One member, one protocol: it stages the release and swaps it in on its own.
    let started = Instant::now();
    let swapped = install.join("startfs");
    while !swapped.exists() {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the release was never swapped in"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // The installer finishes after the swap puts the binary in place.
    while install.join("startfs.staged").exists() {
        assert!(started.elapsed() < Duration::from_secs(30));
        std::thread::sleep(Duration::from_millis(100));
    }
    let version = Command::new(&swapped).arg("--version").output().unwrap();
    assert!(String::from_utf8_lossy(&version.stdout).starts_with("startfs 99.0.0"));
    let text = stdout(&startfs(&data, &["status"]));
    assert!(text.contains("restarting on a new release"), "{text}");
    assert!(text.contains("upgrade: moving to v99.0.0"), "{text}");
}

#[test]
fn cli_update_says_when_the_release_is_not_a_tag() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path();
    let _daemon = start_daemon(data);
    stdout(&startfs(
        data,
        &["create", "--name", "home", "--copies", "1"],
    ));
    let out = startfs(data, &["update", "1.2.3"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a release tag"));
    let out = startfs(data, &["update", "--forced"]);
    assert!(!out.status.success(), "an unknown flag runs nothing");
}

/// A directory to use as the daemon's whole `PATH`, holding a `tailscale` that runs `script`
/// (or no `tailscale` at all when `script` is `None`).
fn path_with_tailscale(dir: &Path, script: Option<&str>) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    if let Some(script) = script {
        let stub = bin.join("tailscale");
        std::fs::write(&stub, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

fn daemon_with_path(data: &Path, path: &Path, wait_secs: &str) -> Child {
    std::fs::create_dir_all(data).unwrap();
    let config = data.join("test-config.toml");
    std::fs::write(&config, "port = 0\n").unwrap();
    Command::new(env!("CARGO_BIN_EXE_startfs"))
        .arg("--data-dir")
        .arg(data)
        .arg("--config")
        .arg(&config)
        .arg("daemon")
        .env("PATH", path)
        .env("STARTFS_TAILNET_WAIT", wait_secs)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap()
}

#[test]
fn a_daemon_whose_tailscale_never_answers_exits_instead_of_listening_on_loopback() {
    let dir = tempfile::tempdir().unwrap();
    let path = path_with_tailscale(dir.path(), Some("exit 1"));
    let out = daemon_with_path(&dir.path().join("data"), &path, "1")
        .wait_with_output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no tailnet address"), "{err}");
}

#[test]
fn a_daemon_waits_for_tailscale_to_come_up() {
    let dir = tempfile::tempdir().unwrap();
    let up = dir.path().join("up");
    // 127.0.0.2 stands in for the tailnet address: bindable anywhere on Linux.
    let script = format!(
        "[ -f {} ] && [ \"$1\" = ip ] && echo 127.0.0.2 || exit 1",
        up.display()
    );
    let path = path_with_tailscale(dir.path(), Some(&script));
    let data = dir.path().join("data");
    let daemon = Daemon(daemon_with_path(&data, &path, "30"));
    std::thread::sleep(Duration::from_millis(1200));
    assert!(
        !data.join("control.sock").exists(),
        "started before tailscale was up"
    );
    std::fs::write(&up, "").unwrap();
    let started = Instant::now();
    while !data.join("control.sock").exists() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "daemon did not start"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(daemon);
}

#[test]
fn a_daemon_without_tailscale_installed_runs_on_loopback() {
    let dir = tempfile::tempdir().unwrap();
    let path = path_with_tailscale(dir.path(), None);
    let data = dir.path().join("data");
    let daemon = Daemon(daemon_with_path(&data, &path, "1"));
    let started = Instant::now();
    while !data.join("control.sock").exists() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "daemon did not start"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(daemon);
}
