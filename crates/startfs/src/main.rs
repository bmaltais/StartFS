//! The `startfs` binary: the daemon (`startfs daemon`) and the CLI that talks to it.

use startfs::mount;
use startfs::node::{self, Ticker};
use startfs_core::activity::{ActivityReport, Traffic};
use startfs_core::clock::SystemClock;
use startfs_core::config::Config;
use startfs_core::control::{self, Client, NodeState, ScrubState, Status, Tier};
use startfs_core::daemon::Daemon;
use startfs_core::fs::NodeConfig;
use startfs_core::members::MemberId;
use startfs_core::power::SystemPower;
use std::net::IpAddr;
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const USAGE: &str = "usage: startfs [--data-dir DIR] [--config FILE] <command>
       startfs --version

commands:
  daemon [--mount DIR]                run the daemon, mounting the filesystem at DIR
  create --name NAME --copies N [--example]
                                      create a filesystem and print its join token
  status [--json]                     show the state of this node's filesystem (--json: as JSON,
                                      for scripts and GUIs)
  activity [--json]                   show this node's recent traffic, tick timings, queues and
                                      memory over the last minute (--json: as JSON)
  stats [--json]                      ask every member for its storage figures (--json: as JSON)
  join TOKEN [--offer SIZE]           join the filesystem a token names
  token [--example]                   print a new join token (--example: the full join
                                      command to paste on the other machine)
  leave                               hand this node's data to the others and leave
  remove MEMBER                       remove a dead member (its data is re-replicated)
  revoke TOKEN                        make a join token stop working
  config [--offer SIZE] [--copies N] [--activity on|off]
                                      change this node's space offer (sizes like 500M, 2T), the
                                      filesystem's number of copies on every node, or whether this
                                      node keeps the counters behind `activity` (until restart)";

#[derive(Debug, thiserror::Error)]
enum CliError {
    #[error("{USAGE}")]
    Usage,
    #[error("missing {0}\n{USAGE}")]
    MissingFlag(String),
    #[error("missing value for {0}")]
    MissingValue(String),
    #[error("cannot encode the status as JSON: {0}")]
    Json(serde_json::Error),
    #[error("--copies must be a number")]
    CopiesNotANumber,
    #[error("--activity must be on or off")]
    ActivityNotOnOff,
    #[error("remove needs a member number (see `startfs status`)")]
    RemoveNeedsMember,
    #[error("not a size: {0}")]
    BadSize(String),
    #[error("{}: {source}", path.display())]
    Config {
        path: PathBuf,
        source: startfs_core::Error,
    },
    #[error("{}: {source}", path.display())]
    Path {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot listen on {addr}: {source}")]
    Listen {
        addr: String,
        source: std::io::Error,
    },
    #[error("mounting is not supported here")]
    MountUnsupported,
    #[error(transparent)]
    DataDir(std::io::Error),
    #[error(transparent)]
    Fs(#[from] startfs_core::ops::FsError),
    #[error(transparent)]
    Core(#[from] startfs_core::Error),
    #[error(transparent)]
    Lib(#[from] startfs::Error),
}

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

fn run(mut args: Vec<String>) -> Result<(), CliError> {
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
    if matches!(args.first().map(String::as_str), Some("--version" | "-V")) {
        println!(
            "startfs {} (protocol {})",
            env!("CARGO_PKG_VERSION"),
            startfs_core::protocol::PROTOCOL_VERSION
        );
        return Ok(());
    }
    let data_dir = data_dir.unwrap_or_else(|| home_dir("XDG_DATA_HOME", ".local/share"));
    let socket = data_dir.join("control.sock");
    let Some(command) = args.first().cloned() else {
        return Err(CliError::Usage);
    };
    let rest = &args[1..];
    match command.as_str() {
        "daemon" => {
            let config_file = config_file
                .unwrap_or_else(|| home_dir("XDG_CONFIG_HOME", ".config").join("config.toml"));
            let mountpoint = flag_value(rest, "--mount").ok().map(PathBuf::from);
            daemon(data_dir, &config_file, &socket, mountpoint)
        }
        "create" => {
            let name = flag_value(rest, "--name")?;
            let copies = flag_value(rest, "--copies")?
                .parse::<u32>()
                .map_err(|_| CliError::CopiesNotANumber)?;
            print_token(&Client::new(socket).create(&name, copies)?, rest);
            Ok(())
        }
        "join" => {
            let token = rest.first().ok_or(CliError::Usage)?;
            let offer = optional_size(rest, "--offer")?;
            Client::new(socket).join(token, offer)?;
            println!("joined");
            Ok(())
        }
        "token" => {
            print_token(&Client::new(socket).new_token()?, rest);
            Ok(())
        }
        "leave" => {
            Client::new(socket).leave()?;
            println!("leaving: this node's data is being handed to the others");
            Ok(())
        }
        "remove" => {
            let member = rest
                .first()
                .and_then(|m| m.parse::<u32>().ok().map(MemberId))
                .ok_or(CliError::RemoveNeedsMember)?;
            Client::new(socket).remove(member)?;
            Ok(())
        }
        "revoke" => {
            Client::new(socket).revoke(rest.first().ok_or(CliError::Usage)?)?;
            Ok(())
        }
        "config" => {
            if let Some(bytes) = optional_size(rest, "--offer")? {
                Client::new(socket.clone()).set_offer(bytes)?;
            }
            if let Ok(value) = flag_value(rest, "--activity") {
                let on = match value.as_str() {
                    "on" => true,
                    "off" => false,
                    _ => return Err(CliError::ActivityNotOnOff),
                };
                Client::new(socket.clone()).set_activity(on)?;
            }
            if let Ok(copies) = flag_value(rest, "--copies") {
                let copies = copies
                    .parse::<u32>()
                    .map_err(|_| CliError::CopiesNotANumber)?;
                let client = Client::new(socket);
                client.set_copies(copies)?;
                if let Ok(s) = client.status()
                    && s.copies_possible < copies
                {
                    eprintln!(
                        "warning: {copies} copies wanted but only {} node{} can store copies, so {} will exist until more join",
                        s.copies_possible,
                        if s.copies_possible == 1 { "" } else { "s" },
                        s.copies_possible,
                    );
                }
            }
            Ok(())
        }
        "status" => {
            let status = Client::new(socket).status()?;
            if rest.iter().any(|a| a == "--json") {
                println!(
                    "{}",
                    serde_json::to_string(&status).map_err(CliError::Json)?
                );
            } else {
                print!("{}", render(&status));
            }
            Ok(())
        }
        "activity" => {
            let report = Client::new(socket).activity()?;
            if rest.iter().any(|a| a == "--json") {
                println!(
                    "{}",
                    serde_json::to_string(&report).map_err(CliError::Json)?
                );
            } else {
                print!("{}", render_activity(&report));
            }
            Ok(())
        }
        "stats" => {
            let client = Client::new(socket);
            let rows = client.node_stats()?;
            if rest.iter().any(|a| a == "--json") {
                println!("{}", serde_json::to_string(&rows).map_err(CliError::Json)?);
            } else {
                let copies = client.status()?.filesystem.map_or(1, |f| f.copies);
                print!("{}", node_stats_table(&rows, copies));
            }
            Ok(())
        }
        _ => Err(CliError::Usage),
    }
}

/// One line per member: what it stores, how well, and its share of the replicas.
fn node_stats_table(rows: &[control::NodeReport], copies: u32) -> String {
    // Every member has the full metadata, so any answering member knows the
    // cluster-wide size after dedup.
    let referenced = rows
        .iter()
        .find_map(|r| r.stats.map(|s| s.efficiency.referenced_bytes))
        .unwrap_or(0);
    let expected = referenced.saturating_mul(u64::from(copies));
    let cells: Vec<[String; 6]> = rows
        .iter()
        .map(|r| match &r.stats {
            Some(s) => [
                r.addr.clone(),
                format!("{} on disk", human(s.usage.stored_bytes)),
                format!("{} chunks", s.usage.chunks),
                ratio(
                    s.efficiency.local_plain_bytes,
                    s.efficiency.local_disk_bytes,
                )
                .map_or_else(|| "-".into(), |x| format!("{x} compression")),
                if expected > 0 {
                    format!(
                        "{:.1}% of replicas",
                        100.0 * s.efficiency.local_plain_bytes as f64 / expected as f64
                    )
                } else {
                    "-".into()
                },
                format!("{} offered", human(s.usage.offer_bytes)),
            ],
            None => [
                r.addr.clone(),
                "no answer".into(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
            ],
        })
        .collect();
    let width = |col: usize| cells.iter().map(|c| c[col].len()).max().unwrap_or(0);
    let w: Vec<usize> = (0..6).map(width).collect();
    let mut out = String::new();
    for (r, c) in rows.iter().zip(&cells) {
        let line = format!(
            "  {:<w0$}  {:>w1$}  {:>w2$}  {:>w3$}  {:>w4$}  {:>w5$}",
            c[0],
            c[1],
            c[2],
            c[3],
            c[4],
            c[5],
            w0 = w[0],
            w1 = w[1],
            w2 = w[2],
            w3 = w[3],
            w4 = w[4],
            w5 = w[5],
        );
        out.push_str(line.trim_end());
        out.push_str(if r.is_self { "  (this node)\n" } else { "\n" });
    }
    out
}

/// Prints the token alone, so scripts can capture it, or with `--example` the command to
/// paste on the other machine.
fn print_token(token: &str, rest: &[String]) {
    if rest.iter().any(|a| a == "--example") {
        println!("startfs join {token}");
    } else {
        println!("{token}");
    }
}

fn flag_value(args: &[String], name: &str) -> Result<String, CliError> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
        .ok_or_else(|| CliError::MissingFlag(name.to_string()))
}

/// A size such as `500M` or `2T` (powers of 1024), or plain bytes.
fn parse_size(text: &str) -> Result<u64, CliError> {
    let bad = || CliError::BadSize(text.to_string());
    let (digits, unit) = match text.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((i, _)) => text.split_at(i),
        None => (text, ""),
    };
    let shift = match unit.to_ascii_uppercase().as_str() {
        "" | "B" => 0,
        "K" | "KB" | "KIB" => 10,
        "M" | "MB" | "MIB" => 20,
        "G" | "GB" | "GIB" => 30,
        "T" | "TB" | "TIB" => 40,
        _ => return Err(bad()),
    };
    digits
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(1 << shift))
        .ok_or_else(bad)
}

fn optional_size(args: &[String], name: &str) -> Result<Option<u64>, CliError> {
    match args.iter().position(|a| a == name) {
        None => Ok(None),
        Some(i) => {
            let value = args
                .get(i + 1)
                .ok_or_else(|| CliError::MissingValue(name.to_string()))?;
            parse_size(value).map(Some)
        }
    }
}

fn daemon(
    data_dir: PathBuf,
    config_file: &std::path::Path,
    socket: &std::path::Path,
    mountpoint: Option<PathBuf>,
) -> Result<(), CliError> {
    // A missing config file is normal: every setting has a default.
    let mut config = match std::fs::read_to_string(config_file) {
        Ok(text) => Config::parse(&text).map_err(|source| CliError::Config {
            path: config_file.to_path_buf(),
            source,
        })?,
        Err(_) => Config::default(),
    };
    let config_mountpoint = config.mountpoint.clone();
    std::fs::create_dir_all(&data_dir).map_err(CliError::DataDir)?;
    // SAFETY: getuid/getgid have no preconditions and cannot fail.
    let node = NodeConfig {
        node: MemberId(0), // `create` keeps 0; `join` assigns the real one
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
    };
    let host = tailnet_address();
    let whois = node::tailscale_whois(&config);
    let transport = node::bind(&mut config, host, whois).map_err(|source| CliError::Listen {
        addr: format!("{host}:{}", config.port),
        source,
    })?;
    let d = Daemon::open(
        &data_dir,
        config,
        node,
        Arc::new(SystemClock),
        advertised_host(host),
    )?;
    let ticker = Ticker::start(d.clone(), transport);
    d.set_power_source(Arc::new(SystemPower));
    let mountpoint = mountpoint.or_else(|| config_mountpoint.map(PathBuf::from));
    let server = control::serve(d.clone(), socket).map_err(|source| CliError::Listen {
        addr: socket.display().to_string(),
        source,
    })?;
    let mount = match mountpoint {
        None => None,
        Some(mp) => {
            let mounter = mount::platform_mounter().ok_or(CliError::MountUnsupported)?;
            std::fs::create_dir_all(&mp).map_err(|source| CliError::Path {
                path: mp.clone(),
                source,
            })?;
            Some(mounter.mount(d.fs(), &mp)?)
        }
    };
    // Run until asked to stop, then unmount so no dead mount is left behind.
    // The control socket stays up meanwhile so `status` can say we are stopping.
    wait_for_stop();
    d.set_stopping();
    eprintln!("startfs: stopping");
    drop(ticker);
    if let Some(m) = mount {
        eprintln!("startfs: unmounting");
        m.unmount()?;
    }
    drop(server);
    Ok(())
}

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

/// Block until SIGTERM or SIGINT arrives.
fn wait_for_stop() {
    // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
    }
    while !STOP.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Run `tailscale` with `args`; its stdout when it succeeds.
fn tailscale(args: &[&str]) -> Option<String> {
    let out = Command::new("tailscale").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8(out.stdout).ok())?
}

/// What this node tells others to dial: its MagicDNS name, which survives a
/// change of tailnet IP, or the IP itself when there is no name (or no
/// tailnet at all, where a name would not be where we listen).
fn advertised_host(ip: IpAddr) -> String {
    if ip.is_loopback() {
        return ip.to_string();
    }
    let name = tailscale(&["status", "--json"]).and_then(|json| node::dns_name(&json));
    name.unwrap_or_else(|| {
        eprintln!("startfs: no MagicDNS name from tailscale; advertising {ip}");
        ip.to_string()
    })
}

/// This machine's tailnet address, or loopback when Tailscale is unavailable.
fn tailnet_address() -> IpAddr {
    tailscale(&["ip", "-4"])
        .and_then(|s| s.lines().next().and_then(|l| l.trim().parse().ok()))
        .unwrap_or(IpAddr::from([127, 0, 0, 1]))
}

/// One line per member, in columns sized to the longest entry.
fn members_table(members: &[control::MemberStatus]) -> String {
    let mut out = String::new();
    let rows: Vec<[String; 5]> = members
        .iter()
        .map(|m| {
            let state = match m.state {
                NodeState::Online => "up",
                NodeState::Down => "down",
                NodeState::Lost => "lost",
                NodeState::Leaving => "leaving",
            };
            [
                m.id.to_string(),
                m.addr.clone(),
                state.to_string(),
                format!("{} offered", human(m.offer_bytes)),
                m.rtt_micros.map_or_else(String::new, rtt_text),
            ]
        })
        .collect();
    let width = |col: usize| rows.iter().map(|r| r[col].len()).max().unwrap_or(0);
    let (wid, waddr, wstate, woffer, wrtt) = (width(0), width(1), width(2), width(3), width(4));
    for (m, r) in members.iter().zip(&rows) {
        out.push_str(&format!(
            "  {:<wid$}  {:<waddr$}  {:<wstate$}  {:>woffer$}{}{}\n",
            r[0],
            r[1],
            r[2],
            r[3],
            if wrtt > 0 {
                format!("  {:>wrtt$}", r[4])
            } else {
                String::new()
            },
            if m.is_self { "  (this node)" } else { "" }
        ));
        if let Some(why) = &m.problem {
            out.push_str(&format!("  {:<wid$}  cannot reach: {why}\n", ""));
        }
        if let Some(seen) = m.last_seen {
            out.push_str(&format!(
                "  {:<wid$}  last seen {}{} ago\n",
                "",
                if seen.exact { "" } else { "over " },
                time_left(seen.secs_ago)
            ));
        }
        if let Some(left) = m.standin_secs_left {
            out.push_str(&format!(
                "  {:<wid$}  extra copies kept for {} in case it sleeps again\n",
                "",
                time_left(left)
            ));
        }
    }
    out
}

/// "2d 4h", "5h 10m" or "12m": the two largest units of `secs`.
/// A round-trip time for the members table: `340µs`, `12ms`, `1.2s`.
fn rtt_text(micros: u64) -> String {
    match micros {
        0..1_000 => format!("{micros}µs"),
        1_000..100_000 => format!("{:.1}ms", micros as f64 / 1_000.0),
        100_000..1_000_000 => format!("{}ms", micros / 1_000),
        _ => format!("{:.1}s", micros as f64 / 1_000_000.0),
    }
}

fn time_left(secs: u64) -> String {
    let (d, h, m) = (secs / 86400, secs % 86400 / 3600, secs % 3600 / 60);
    match (d, h) {
        (0, 0) => format!("{m}m"),
        (0, _) => format!("{h}h {m}m"),
        _ => format!("{d}d {h}h"),
    }
}

/// "not fully replicated: N chunks (a at risk, b degraded)", or nothing.
fn unreplicated_line(unreplicated: &[control::Shortfall]) -> Option<String> {
    if unreplicated.is_empty() {
        return None;
    }
    let count = |risk: Tier| -> u32 {
        unreplicated
            .iter()
            .filter(|f| f.tier() == risk)
            .map(|f| f.chunks)
            .sum()
    };
    let n: u32 = unreplicated.iter().map(|f| f.chunks).sum();
    let tiers: Vec<String> = [(Tier::AtRisk, "at risk"), (Tier::Degraded, "degraded")]
        .into_iter()
        .filter(|(t, _)| count(*t) > 0)
        .map(|(t, name)| format!("{} {name}", count(t)))
        .collect();
    Some(format!(
        "not fully replicated: {n} chunk{} ({})\n",
        if n == 1 { "" } else { "s" },
        tiers.join(", ")
    ))
}

/// `a / b` as "1.4x", or nothing when there is no `b` to compare with.
fn ratio(a: u64, b: u64) -> Option<String> {
    (b > 0).then(|| format!("{:.1}x", a as f64 / b as f64))
}

/// Where the space went: the files' size, what dedup and compression leave
/// of it, and this node's share of the replicas.
fn efficiency_lines(s: &Status, copies: u32) -> String {
    let e = &s.efficiency;
    if e.logical_bytes == 0 {
        return "storage: no files yet\n".into();
    }
    let mut out = format!(
        "storage: {} in files, {} after dedup",
        human(e.logical_bytes),
        human(e.referenced_bytes + e.inline_bytes)
    );
    if let Some(r) = ratio(e.logical_bytes, e.referenced_bytes + e.inline_bytes) {
        out.push_str(&format!(" ({r})"));
    }
    out.push('\n');
    out.push_str(&format!(
        "  this node: {} on disk for {} of chunks",
        human(e.local_disk_bytes),
        human(e.local_plain_bytes)
    ));
    if let Some(r) = ratio(e.local_plain_bytes, e.local_disk_bytes) {
        out.push_str(&format!(" (compression {r})"));
    }
    out.push_str(&format!(
        ", {} compressed and {} stored as is\n",
        e.zstd_chunks, e.raw_chunks
    ));
    if e.zstd_chunks > 0 && e.local_plain_bytes > 0 {
        out.push_str(&format!(
            "  compression saved {} ({:.0}% of this node's chunk bytes were compressed)\n",
            human(e.local_plain_bytes.saturating_sub(e.local_disk_bytes)),
            100.0 * e.zstd_plain_bytes as f64 / e.local_plain_bytes as f64
        ));
    }
    let expected = e.referenced_bytes.saturating_mul(u64::from(copies));
    if expected > 0 {
        out.push_str(&format!(
            "  this node holds {:.1}% of the {} expected across the cluster ({} cop{})\n",
            100.0 * e.local_plain_bytes as f64 / expected as f64,
            human(expected),
            copies,
            if copies == 1 { "y" } else { "ies" }
        ));
    }
    if s.gc.pending_bytes > 0 {
        out.push_str(&format!(
            "  plus {} on this node that no file uses, waiting for garbage collection\n",
            human(s.gc.pending_bytes)
        ));
    }
    out
}

fn render(s: &Status) -> String {
    let stopping = if s.stopping {
        "stopping: the daemon is shutting down\n"
    } else {
        ""
    };
    let Some(fs) = &s.filesystem else {
        return format!(
            "{stopping}no filesystem on this node; run `startfs create --name NAME --copies N`\n"
        );
    };
    let state = match s.node_state {
        NodeState::Online => "online",
        NodeState::Down => "down",
        NodeState::Lost => "lost",
        NodeState::Leaving => "leaving",
    };
    let mut out = String::from(stopping);
    if let Some(v) = s.verdict {
        out.push_str(&format!("data: {}\n", v.describe().to_uppercase()));
    }
    out.push_str(&format!(
        "filesystem: {} ({} copies)\nnode: {state}\nusage: {} stored in {} chunks, {} offered\n",
        fs.name,
        fs.copies,
        human(s.usage.stored_bytes),
        s.usage.chunks,
        human(s.usage.offer_bytes),
    ));
    out.push_str(&format!(
        "redundancy: {} of {} copies possible ({} node{})\n",
        s.copies_possible,
        fs.copies,
        s.nodes,
        if s.nodes == 1 { "" } else { "s" }
    ));
    out.push_str(&format!("usable: about {}\n", human(s.usable_bytes)));
    out.push_str(&efficiency_lines(s, fs.copies));
    if s.staging.chunks > 0 {
        out.push_str(&format!(
            "staging: {} in {} chunks waiting for replication\n",
            human(s.staging.bytes),
            s.staging.chunks
        ));
    }
    out.push_str(&format!(
        "cache: {} used of {}\n",
        human(s.cache.used_bytes),
        human(s.cache.capacity_bytes)
    ));
    out.push_str("members:\n");
    out.push_str(&members_table(&s.members));
    if let Some(r) = &s.repair {
        out.push_str(&format!(
            "repair: {} at risk, {} degraded chunks left to re-replicate\n",
            r.at_risk, r.degraded
        ));
    }
    let scrub = match s.scrub.state {
        ScrubState::Idle => "idle".to_string(),
        ScrubState::Running => format!("running, {} replicas to check", s.scrub.remaining),
        ScrubState::PausedOnBattery => "paused while on battery power".to_string(),
    };
    out.push_str(&format!("scrub: {scrub}"));
    if s.scrub.healing > 0 {
        out.push_str(&format!(
            ", {} corrupt replicas being re-fetched",
            s.scrub.healing
        ));
    }
    out.push('\n');
    out.push_str(&format!(
        "metadata log: {} entries, {} objects\n",
        s.log.entries, s.log.objects
    ));
    if s.gc.pending_chunks > 0 || s.gc.reclaimed_chunks > 0 {
        out.push_str(&format!(
            "garbage collection: {} chunks ({} bytes) pending, {} chunks ({} bytes) reclaimed\n",
            s.gc.pending_chunks, s.gc.pending_bytes, s.gc.reclaimed_chunks, s.gc.reclaimed_bytes
        ));
    }
    if !s.gc.waiting_on.is_empty() {
        let names: Vec<&str> = s
            .members
            .iter()
            .filter(|m| s.gc.waiting_on.contains(&m.id))
            .map(|m| m.addr.as_str())
            .collect();
        out.push_str(&format!(
            "garbage collection is waiting to catch up with {} before reclaiming\n",
            names.join(", ")
        ));
    }
    if let Some(t) = &s.tick_busy {
        out.push_str(&format!(
            "busy: the background tick worked {}% of the last minute (average {} ms); see `startfs activity`\n",
            t.percent, t.avg_ms
        ));
    }
    for w in &s.warnings {
        out.push_str(&format!("warning: {w}\n"));
    }
    if !s.unheard.is_empty() {
        let names: Vec<&str> = s
            .members
            .iter()
            .filter(|m| s.unheard.contains(&m.id))
            .map(|m| m.addr.as_str())
            .collect();
        out.push_str(&format!(
            "partial view: no replication report from {}\n",
            names.join(", ")
        ));
    }
    if let Some(line) = unreplicated_line(&s.unreplicated) {
        out.push_str(&line);
    }
    for st in &s.stranded {
        let addr = s
            .members
            .iter()
            .find(|m| m.id == st.member)
            .map_or("a member", |m| m.addr.as_str());
        out.push_str(&format!(
            "at risk: {} chunk{} may exist only on {addr} (single copy in its report {} ago)\n",
            st.chunks,
            if st.chunks == 1 { "" } else { "s" },
            time_left(st.secs_ago)
        ));
    }
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

fn render_activity(a: &ActivityReport) -> String {
    let mut out = String::new();
    if a.enabled {
        out.push_str(&format!(
            "activity over the last {:.0} s (counters reset when the daemon restarts)\n",
            a.window_secs
        ));
    } else {
        out.push_str(
            "activity counters are off (`startfs config --activity on` switches them on); only resources and queues are shown\n",
        );
    }
    let secs = a.window_secs.max(1.0);
    let rate = |t: &Traffic| {
        format!(
            "{}/s {:.1} msg/s",
            human((t.bytes as f64 / secs) as u64),
            t.messages as f64 / secs
        )
    };
    if a.enabled {
        out.push_str("peer paths (out | in; chunk data, metadata, gossip, other):\n");
    } else {
        out.push_str("peer paths:\n");
    }
    for p in &a.peers {
        let state = match p.state {
            NodeState::Online => "online",
            NodeState::Down => "down",
            NodeState::Lost => "lost",
            NodeState::Leaving => "leaving",
        };
        out.push_str(&format!(
            "  {} ({state}), {} in flight\n",
            p.addr, p.in_flight
        ));
        let Some(t) = &p.traffic else {
            continue;
        };
        for (name, c) in [("out", &t.sent), ("in ", &t.received)] {
            out.push_str(&format!(
                "    {name}: chunks {}, metadata {}, gossip {}, other {}\n",
                rate(&c.chunk),
                rate(&c.metadata),
                rate(&c.gossip),
                rate(&c.other)
            ));
        }
        out.push_str(&format!(
            "    sends: {} acknowledged, {} timed out, {} retried\n",
            t.acknowledged, t.timed_out, t.retried
        ));
    }
    if let Some(t) = &a.tick {
        let p = &t.avg_phase_ms;
        out.push_str(&format!(
            "tick: {} in the window, average {:.1} ms, longest {:.1} ms, busy {:.0}% of the time\n  phases (average ms): flush {:.2}, sync {:.2}, receive {:.2}, gossip {:.2}, replicate {:.2}, other {:.2}\n",
            t.ticks, t.avg_ms, t.max_ms, t.busy_fraction * 100.0, p.flush, p.sync, p.receive, p.gossip, p.replicate, p.other
        ));
    } else if a.enabled {
        out.push_str("tick: no ticks in the window\n");
    }
    out.push_str(&format!("replication queue: {} chunks", a.queue.chunks));
    if let Some(age) = a.queue.oldest_secs {
        out.push_str(&format!(", the oldest waiting {age} s"));
    }
    out.push('\n');
    if a.chunk_sizes.iter().any(|c| c.chunks > 0) {
        let mut low = 0;
        let classes: Vec<String> = a
            .chunk_sizes
            .iter()
            .map(|c| {
                let label = match c.up_to_bytes {
                    Some(up) => format!("{}-{}", human(low), human(up)),
                    None => format!("over {}", human(low)),
                };
                low = c.up_to_bytes.unwrap_or(0);
                format!("{label}: {}", c.chunks)
            })
            .collect();
        out.push_str(&format!(
            "chunks replicated by size: {}\n",
            classes.join(", ")
        ));
    }
    out.push_str("in memory:\n");
    let s = &a.structures;
    for (name, st) in [
        ("inbox", &s.inbox),
        ("unclaimed chunk replies", &s.chunk_replies),
        ("decoded-chunk cache", &s.decoded_cache),
        ("replication index", &s.replication_index),
        ("staging index", &s.staging_index),
    ] {
        out.push_str(&match st {
            None => format!("  {name}: unavailable\n"),
            Some(st) => format!(
                "  {name}: {} entries{}{}\n",
                st.entries,
                st.bytes
                    .map_or(String::new(), |b| format!(", {}", human(b))),
                st.limit_bytes
                    .map_or(", no limit".to_string(), |l| format!(" of {}", human(l)))
            ),
        });
    }
    out.push_str("resources:\n");
    let r = &a.resources;
    let bytes = |v: Option<u64>| v.map_or("unavailable".to_string(), human);
    let psi = |v: &Option<startfs_core::resources::Pressure>| {
        v.map_or("unavailable".to_string(), |p| {
            format!("{:.1}% (10 s), {:.1}% (60 s)", p.avg10, p.avg60)
        })
    };
    out.push_str(&format!(
        "  daemon: {} resident, cpu {}, {} threads\n",
        bytes(r.rss_bytes),
        r.cpu_secs
            .map_or("unavailable".to_string(), |c| format!("{c:.1} s")),
        r.threads
            .map_or("unavailable".to_string(), |t| t.to_string()),
    ));
    out.push_str(&format!(
        "  machine: {} memory available, {} swap used\n",
        bytes(r.mem_available_bytes),
        bytes(r.swap_used_bytes)
    ));
    out.push_str(&format!("  memory pressure: {}\n", psi(&r.memory_pressure)));
    out.push_str(&format!("  cpu pressure: {}\n", psi(&r.cpu_pressure)));
    out.push_str(&match &r.cgroup {
        None => "  cgroup memory: unavailable\n".to_string(),
        Some(c) => format!(
            "  cgroup memory: {} used of {}\n",
            human(c.memory_current_bytes),
            c.memory_limit_bytes.map_or("no limit".to_string(), human)
        ),
    });
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
