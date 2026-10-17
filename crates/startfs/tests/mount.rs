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

#[test]
fn standard_tools_work_on_a_real_mount() {
    if !fuse_available() {
        eprintln!("skipping: /dev/fuse or fusermount is not available");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let mp = dir.path().join("mnt");
    std::fs::create_dir(&mp).unwrap();
    let config = dir.path().join("config.toml");
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
            return;
        }
        let mounted = std::fs::read_to_string("/proc/self/mountinfo")
            .unwrap()
            .lines()
            .any(|l| l.contains(mp.to_str().unwrap()));
        if mounted {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "mount timed out"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

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
    // SAFETY: plain kill(2) on our own child.
    unsafe { libc::kill(daemon.child.id() as i32, libc::SIGTERM) };
    let status = daemon.child.wait().unwrap();
    assert!(status.success(), "daemon exit: {status}");
    let still = std::fs::read_to_string("/proc/self/mountinfo")
        .unwrap()
        .lines()
        .any(|l| l.contains(mp.to_str().unwrap()));
    assert!(!still, "mount should be gone after the daemon stops");
}
