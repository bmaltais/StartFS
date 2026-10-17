//! The `startfs` binary: the daemon (`startfs daemon`) and the CLI that talks to it.

use startfs_core::clock::SystemClock;
use startfs_core::config::Config;
use startfs_core::control::{self, Client, NodeState, Status};
use startfs_core::daemon::Daemon;
use startfs_core::fs::NodeConfig;
use startfs_core::net::NodeId;
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::sync::Arc;

const USAGE: &str = "usage: startfs [--data-dir DIR] [--config FILE] <command>

commands:
  daemon                              run the daemon
  create --name NAME --copies N       create a filesystem and print its join token
  status                              show the state of this node's filesystem";

fn main() -> ExitCode {
    match run(std::env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("startfs: {e}");
            ExitCode::FAILURE
        }
    }
}

fn home_dir(var: &str, fallback: &str) -> PathBuf {
    if let Some(p) = std::env::var_os(var) {
        return PathBuf::from(p).join("startfs");
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(fallback).join("startfs")
}

fn run(mut args: Vec<String>) -> Result<(), String> {
    let mut data_dir = None;
    let mut config_file = None;
    while let Some(flag) = args.first().cloned() {
        match flag.as_str() {
            "--data-dir" | "--config" if args.len() >= 2 => {
                let v = PathBuf::from(args.remove(1));
                if flag == "--data-dir" {
                    data_dir = Some(v);
                } else {
                    config_file = Some(v);
                }
                args.remove(0);
            }
            _ => break,
        }
    }
    let data_dir = data_dir.unwrap_or_else(|| home_dir("XDG_DATA_HOME", ".local/share"));
    let socket = data_dir.join("control.sock");
    let Some(command) = args.first().cloned() else {
        return Err(USAGE.into());
    };
    let rest = &args[1..];
    match command.as_str() {
        "daemon" => {
            let config_file = config_file
                .unwrap_or_else(|| home_dir("XDG_CONFIG_HOME", ".config").join("config.toml"));
            daemon(data_dir, &config_file, &socket)
        }
        "create" => {
            let name = flag_value(rest, "--name")?;
            let copies = flag_value(rest, "--copies")?
                .parse::<u32>()
                .map_err(|_| "--copies must be a number".to_string())?;
            println!("{}", Client::new(socket).create(&name, copies)?);
            Ok(())
        }
        "status" => {
            print!("{}", render(&Client::new(socket).status()?));
            Ok(())
        }
        _ => Err(USAGE.into()),
    }
}

fn flag_value(args: &[String], name: &str) -> Result<String, String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
        .ok_or_else(|| format!("missing {name}\n{USAGE}"))
}

fn daemon(
    data_dir: PathBuf,
    config_file: &std::path::Path,
    socket: &std::path::Path,
) -> Result<(), String> {
    // A missing config file is normal: every setting has a default.
    let config = match std::fs::read_to_string(config_file) {
        Ok(text) => Config::parse(&text).map_err(|e| format!("{}: {e}", config_file.display()))?,
        Err(_) => Config::default(),
    };
    std::fs::create_dir_all(&data_dir).map_err(|e| e.to_string())?;
    // SAFETY: getuid/getgid have no preconditions and cannot fail.
    let node = NodeConfig {
        node: NodeId(0), // assigned at join once membership exists
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
    };
    let d = Daemon::open(
        &data_dir,
        config,
        node,
        Arc::new(SystemClock),
        tailnet_address(),
    )
    .map_err(|e| e.to_string())?;
    let server = control::serve(d, socket)
        .map_err(|e| format!("cannot listen on {}: {e}", socket.display()))?;
    server.wait();
    Ok(())
}

/// This machine's tailnet address, or loopback when Tailscale is unavailable.
fn tailnet_address() -> String {
    Command::new("tailscale")
        .args(["ip", "-4"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.lines().next().map(str::to_string))
        .unwrap_or_else(|| "127.0.0.1".into())
}

fn render(s: &Status) -> String {
    let Some(fs) = &s.filesystem else {
        return "no filesystem on this node; run `startfs create --name NAME --copies N`\n".into();
    };
    let state = match s.node_state {
        NodeState::Online => "online",
        NodeState::Down => "down",
    };
    let mut out = format!(
        "filesystem: {} ({} copies)\nnode: {state}\nusage: {} stored in {} chunks, {} offered\n",
        fs.name,
        fs.copies,
        human(s.usage.stored_bytes),
        s.usage.chunks,
        human(s.usage.offer_bytes),
    );
    out.push_str(&format!(
        "redundancy: {} of {} copies possible ({} node{})\n",
        s.copies_possible,
        fs.copies,
        s.nodes,
        if s.nodes == 1 { "" } else { "s" }
    ));
    if !s.conflicts.is_empty() {
        out.push_str(&format!(
            "conflicts: {} (delete a copy to resolve it)\n",
            s.conflicts.len()
        ));
        for c in &s.conflicts {
            out.push_str(&format!("  {}\n", c.path));
        }
    }
    out
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}
