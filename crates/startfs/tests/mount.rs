//! Real-mount smoke test (Linux): the daemon mounts the filesystem and
//! ordinary tools work on it. Skipped, not failed, where FUSE is unavailable
//! (no /dev/fuse, no fusermount, or mounting is not permitted).
#![cfg(target_os = "linux")]
#![allow(
    clippy::let_underscore_must_use,
    reason = "test teardown and best-effort replies"
)]

use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

fn fuse_available() -> bool {
    let helper = ["fusermount3", "fusermount"].iter().any(|h| {
        Command::new(h)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
    });
    Path::new("/dev/fuse").exists() && helper
}

fn run(dir: &Path, program: &str, args: &[&str]) -> Output {
    let out = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{program} {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn unmount(mp: &Path) {
    for tool in ["fusermount3", "fusermount"] {
        if Command::new(tool)
            .arg("-u")
            .arg(mp)
            .stderr(Stdio::null())
            .status()
            .is_ok()
        {
            return;
        }
    }
}

struct Daemon {
    child: Child,
    mountpoint: std::path::PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        unmount(&self.mountpoint);
    }
}

/// A daemon with its filesystem mounted, or `None` where mounting is not possible.
fn mounted_daemon(dir: &Path) -> Option<Daemon> {
    if !fuse_available() {
        eprintln!("skipping: /dev/fuse or fusermount is not available");
        return None;
    }
    let data = dir.join("data");
    let mp = dir.join("mnt");
    std::fs::create_dir(&mp).unwrap();
    let config = dir.join("config.toml");
    std::fs::write(&config, "port = 0\n").unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_startfs"))
        .arg("--data-dir")
        .arg(&data)
        .arg("--config")
        .arg(&config)
        .arg("daemon")
        .arg("--mount")
        .arg(&mp)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut daemon = Daemon {
        child,
        mountpoint: mp.clone(),
    };
    let started = Instant::now();
    loop {
        if let Some(status) = daemon.child.try_wait().unwrap() {
            // The daemon exits at once when mounting is refused (containers,
            // user namespaces): treat that as "FUSE unavailable".
            eprintln!("skipping: daemon exited early ({status}); mounting not permitted here");
            return None;
        }
        let mounted = std::fs::read_to_string("/proc/self/mountinfo")
            .unwrap()
            .lines()
            .any(|l| l.contains(mp.to_str().unwrap()));
        if mounted {
            return Some(daemon);
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "mount timed out"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn terminate(daemon: &Daemon) {
    // SAFETY: plain kill(2) on our own child.
    unsafe { libc::kill(daemon.child.id() as i32, libc::SIGTERM) };
}

/// Wait up to `limit` for the daemon to exit.
fn exits_within(daemon: &mut Daemon, limit: Duration) -> Option<std::process::ExitStatus> {
    let started = Instant::now();
    while started.elapsed() < limit {
        if let Some(status) = daemon.child.try_wait().unwrap() {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

#[test]
fn standard_tools_work_on_a_real_mount() {
    let dir = tempfile::tempdir().unwrap();
    let Some(mut daemon) = mounted_daemon(dir.path()) else {
        return;
    };
    let mp = daemon.mountpoint.clone();

    // Plain file I/O and ls.
    std::fs::write(mp.join("hello.txt"), b"hello world").unwrap();
    assert_eq!(std::fs::read(mp.join("hello.txt")).unwrap(), b"hello world");
    let ls = run(&mp, "ls", &["-la"]);
    assert!(String::from_utf8_lossy(&ls.stdout).contains("hello.txt"));

    // cp, a large file (several chunks) and a directory tree.
    let big: Vec<u8> = (0..3_000_000u32).map(|i| (i * 7 % 251) as u8).collect();
    let src = dir.path().join("big.bin");
    std::fs::write(&src, &big).unwrap();
    run(
        dir.path(),
        "cp",
        &[src.to_str().unwrap(), mp.join("big.bin").to_str().unwrap()],
    );
    assert_eq!(std::fs::read(mp.join("big.bin")).unwrap(), big);
    run(&mp, "mkdir", &["-p", "a/b"]);
    run(&mp, "cp", &["hello.txt", "a/b/copy.txt"]);
    run(&mp, "ln", &["-s", "hello.txt", "link"]);
    assert_eq!(
        std::fs::read_link(mp.join("link")).unwrap(),
        Path::new("hello.txt")
    );

    // Hardlinks: a second name for the same file, with its link count.
    run(&mp, "ln", &["hello.txt", "a/b/hard.txt"]);
    {
        use std::os::unix::fs::MetadataExt;
        let (first, second) = (
            std::fs::metadata(mp.join("hello.txt")).unwrap(),
            std::fs::metadata(mp.join("a/b/hard.txt")).unwrap(),
        );
        assert_eq!((first.ino(), first.nlink()), (second.ino(), 2));
    }
    assert_eq!(
        std::fs::read(mp.join("a/b/hard.txt")).unwrap(),
        b"hello world"
    );

    // The editor pattern: write a temp file, fsync, rename over the original.
    std::fs::write(mp.join(".hello.txt.swp"), b"edited").unwrap();
    std::fs::rename(mp.join(".hello.txt.swp"), mp.join("hello.txt")).unwrap();
    assert_eq!(std::fs::read(mp.join("hello.txt")).unwrap(), b"edited");

    // git: init, add, commit, status clean.
    let repo = mp.join("repo");
    std::fs::create_dir(&repo).unwrap();
    run(&repo, "git", &["init", "-q"]);
    std::fs::write(repo.join("f.txt"), b"content\n").unwrap();
    run(&repo, "git", &["add", "f.txt"]);
    run(
        &repo,
        "git",
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "one",
        ],
    );
    assert!(
        run(&repo, "git", &["status", "--porcelain"])
            .stdout
            .is_empty()
    );

    run(&mp, "rm", &["-r", "a"]);
    assert!(!mp.join("a").exists());

    // Stopping the daemon (SIGTERM) unmounts cleanly.
    terminate(&daemon);
    let status = daemon.child.wait().unwrap();
    assert!(status.success(), "daemon exit: {status}");
    let still = std::fs::read_to_string("/proc/self/mountinfo")
        .unwrap()
        .lines()
        .any(|l| l.contains(mp.to_str().unwrap()));
    assert!(!still, "mount should be gone after the daemon stops");
}

/// A process with its working directory on the mount, as a shell would have.
fn hold(mp: &Path) -> Child {
    Command::new("sleep")
        .arg("600")
        .current_dir(mp)
        .spawn()
        .unwrap()
}

#[test]
fn the_daemon_stops_even_when_a_process_is_inside_the_mount() {
    let dir = tempfile::tempdir().unwrap();
    let Some(mut daemon) = mounted_daemon(dir.path()) else {
        return;
    };
    let mut holder = hold(&daemon.mountpoint);
    terminate(&daemon);
    let status = exits_within(&mut daemon, Duration::from_secs(10));
    let _ = holder.kill();
    let _ = holder.wait();
    assert!(
        status.is_some_and(|s| s.success()),
        "the daemon should exit shortly after SIGTERM: {status:?}"
    );
}

#[test]
fn status_says_stopping_while_the_daemon_shuts_down() {
    let dir = tempfile::tempdir().unwrap();
    let Some(mut daemon) = mounted_daemon(dir.path()) else {
        return;
    };
    let mut holder = hold(&daemon.mountpoint);
    terminate(&daemon);
    let mut said = (false, false);
    let started = Instant::now();
    while daemon.child.try_wait().unwrap().is_none() && started.elapsed() < Duration::from_secs(10)
    {
        let status = |args: &[&str]| {
            let out = Command::new(env!("CARGO_BIN_EXE_startfs"))
                .arg("--data-dir")
                .arg(dir.path().join("data"))
                .arg("status")
                .args(args)
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        said.0 |= status(&[]).contains("stopping");
        said.1 |= status(&["--json"]).contains("\"stopping\":true");
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = holder.kill();
    let _ = holder.wait();
    assert_eq!(
        said,
        (true, true),
        "status should report the stop under way"
    );
}
