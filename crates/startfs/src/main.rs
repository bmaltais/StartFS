//! The `startfs` binary: the daemon (`startfs daemon`) and the CLI that talks to it.

use startfs::installer::ScriptInstaller;
use startfs::mount;
use startfs::node::{self, Ticker};
use startfs_core::activity::{ActivityReport, Traffic};
use startfs_core::clock::SystemClock;
use startfs_core::config::Config;
use startfs_core::control::{self, Client, NodeState};
use startfs_core::daemon::Daemon;
use startfs_core::fs::NodeConfig;
use startfs_core::members::MemberId;
use startfs_core::power::SystemPower;
use startfs_core::status_view::{human, ratio, render, state_word, watch_frame};
use std::net::IpAddr;
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::{Duration, Instant};

const USAGE: &str = "usage: startfs [--data-dir DIR] [--config FILE] <command>
       startfs --version

commands:
  daemon [--mount DIR]                run the daemon, mounting the filesystem at DIR
  create --name NAME --copies N [--encrypt] [--example]
                                      create a filesystem and print its join token. --encrypt:
                                      chunks and metadata are encrypted at rest; the recovery key is shown
                                      once (on stderr). A plain filesystem unless --encrypt
  status [--verbose] [--json] [--watch [--interval SECONDS]]
                                      show a summary of this node's filesystem: the verdict and its
                                      cause, the members, what needs attention (--verbose, -v: also
                                      storage, cache, scrub, log; --json: everything, as JSON, for
                                      scripts and GUIs; --watch: redraw it in place every
                                      2 seconds, or every SECONDS, until Ctrl-C)
  activity [--json]                   show this node's recent traffic, tick timings, queues and
                                      memory over the last minute (--json: as JSON)
  purge                               free the data of deleted files on every member now, instead
                                      of after the retention period (not undoable); a member that
                                      is away is told once it is back. Kept versions stay
  stats [--json]                      ask every member for its storage figures (--json: as JSON)
  history PATH [--json]               list the versions kept of the file at PATH (under the mount,
                                      or in the filesystem), each named by its restore point
  restore PATH --at POINT|MARK        bring the file at PATH, or every file under the directory at
                                      PATH (deleted or not), back to what it held at POINT
                                      (YYYYMMDD-HHMMSS[.fff], UTC, as `history` shows it) or at
                                      the instant of a mark; what it replaces is kept as a
                                      version, so a restore is undoable
  mark create NAME | list [--json] | delete NAME
                                      name this instant so that every version needed to restore
                                      to it is kept past the version window (`restore PATH --at
                                      NAME`), on every member, until the mark is deleted. Disk
                                      held only by marks is shown by `status`. A file renamed
                                      since the mark is not brought back to its old place
  join TOKEN [--key RECOVERY_KEY] [--offer SIZE] [--site NAME] [--storage-only]
                                      join the filesystem a token names, declaring where this
                                      node lives (home, cloud) so copies are spread across sites;
                                      --storage-only: hold replicas but never mount or serve the
                                      filesystem. An encrypted filesystem needs --key
  add [USER@]HOST [--offer SIZE] [--site NAME] [--storage-only] [--via ssh|tailscale] [--version vX.Y.Z]
                                      install StartFS on a Linux machine over SSH and join it to this
                                      filesystem, run from your terminal with your own credentials.
                                      Uses `tailscale ssh` for a tailnet machine, else ssh (--via
                                      picks). It installs the release this node runs (--version
                                      another), FUSE unless --storage-only, and revokes its short-lived
                                      token at the end. An encrypted filesystem is refused
  recover RECOVERY_KEY TOKEN [--offer SIZE] [--site NAME]
                                      bring a rebuilt machine back into an encrypted filesystem:
                                      the key is checked by a member before it says anything
  recovery-key                        LAST RESORT: make another recovery key from a running
                                      member, when none is left to add a node with. Existing keys
                                      keep working; anyone who gets the new one can read the data
  token [--example]                   print a new join token (--example: the full join
                                      command to paste on the other machine)
  update [vX.Y.Z] [--force]           move every member to a release (the latest stable one when
                                      none is named): each stages it, then they restart one at a
                                      time, or all together when the protocol changes; follow it
                                      with `status`. --force: every member restarts although
                                      its mount is in use
  leave                               hand this node's data to the others and leave
  remove MEMBER                       remove a dead member (its data is re-replicated)
  revoke TOKEN                        make a join token stop working
  copies PATH N | PATH --clear       give the directory at PATH its own number of copies on every
                                      node (scratch 1, photos 3), for the files below it down to a
                                      directory with its own; a chunk files in two places use gets
                                      the most. --clear: follow the filesystem's again. Lowering
                                      waits for a healthy cluster, like `config --copies`;
                                      `status` lists them
  keep PATH [--release]               keep the file or directory at PATH in this node's cache: its
                                      chunks are fetched now if missing and never evicted for other
                                      reads (--release: let it go). Only this node; no other
                                      member is affected
  free                                free this node's cache: drop every cached chunk that is not
                                      kept (replicas this node is responsible for stay)
  config [--offer SIZE] [--weight SIZE] [--copies N] [--activity on|off] [--site NAME]
         [--storage-only | --no-storage-only] [--keep-versions DURATION]
                                      change this node's space offer (sizes like 500M, 2T), its
                                      weight (the share of new copies it takes, in the same
                                      units; 0: follow the offer), the filesystem's number of
                                      copies on every node, or whether this node keeps the
                                      counters behind `activity` (until restart), or the site
                                      this node declares (empty: none, so it is its own site),
                                      whether this node is storage-only (it needs an offer, and
                                      must not have the filesystem mounted),
                                      or how long every node keeps a replaced or deleted file's
                                      earlier version (like 7d, 36h; 0: none)";

#[derive(Debug, thiserror::Error)]
enum CliError {
    #[error("{USAGE}")]
    Usage,
    #[error("unexpected argument {0}\n{USAGE}")]
    UnexpectedArgument(String),
    #[error("missing {0}\n{USAGE}")]
    MissingFlag(String),
    #[error("missing value for {0}")]
    MissingValue(String),
    #[error("cannot encode the status as JSON: {0}")]
    Json(serde_json::Error),
    #[error("--watch redraws the text view; it cannot be combined with --json")]
    WatchWithJson,
    #[error("--watch needs a terminal to draw on")]
    WatchNeedsTerminal,
    #[error("not an interval: {0} (expected seconds, 0.2 or more)")]
    BadInterval(String),
    #[error("--storage-only and --no-storage-only contradict each other")]
    StorageOnlyBoth,
    #[error(
        "this member is storage-only, so it does not mount the filesystem; drop --mount and the mountpoint setting, or make it an ordinary member first (`startfs config --no-storage-only`)"
    )]
    StorageOnlyCannotMount,
    #[error("--via must be ssh or tailscale")]
    BadVia,
    #[error("a storage-only member needs --offer: how much space it gives")]
    StorageOnlyNeedsOffer,
    #[error(transparent)]
    Add(#[from] startfs::add::AddError),
    #[error("--copies must be a number")]
    CopiesNotANumber,
    #[error("--activity must be on or off")]
    ActivityNotOnOff,
    #[error("remove needs a member number (see `startfs status`)")]
    RemoveNeedsMember,
    #[error(
        "not a restore point or a mark: {0} (expected YYYYMMDD-HHMMSS[.fff] in UTC, as `startfs history` shows, or a name from `startfs mark list`)"
    )]
    BadRestorePoint(String),
    #[error("not a duration: {0} (expected a number and a unit, like 7d, 36h, 90m or 30s; or 0)")]
    BadDuration(String),
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
    #[error("tailscale gave no tailnet address within {0:?} (is tailscaled up and logged in?)")]
    NoTailnet(Duration),
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
    if rest.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return Ok(());
    }
    check_arguments(&command, rest)?;
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
            let encrypt = rest.iter().any(|a| a == "--encrypt");
            let (token, key) = Client::new(socket).create_filesystem(&name, copies, encrypt)?;
            print_token(&token, rest);
            if let Some(key) = key {
                eprintln!(
                    "\nRECOVERY KEY (shown once, keep it somewhere safe):\n\n    {key}\n\n\
                     Chunks and metadata are encrypted. A machine joining, or rebuilt, needs this key:\n\
                     startfs join TOKEN --key KEY. Lost it? A running member can make another:\n\
                     startfs recovery-key (a last resort)."
                );
            }
            Ok(())
        }
        "join" => {
            let token = rest.first().ok_or(CliError::Usage)?;
            let offer = optional_size(rest, "--offer")?;
            let site = flag_value(rest, "--site").unwrap_or_default();
            if let Ok(key) = flag_value(rest, "--key") {
                Client::new(socket).join_with_key(token, offer, &site, &key)?;
            } else if rest.iter().any(|a| a == "--storage-only") {
                Client::new(socket).join_storage_only(token, offer, &site)?;
            } else {
                Client::new(socket).join_at(token, offer, &site)?;
            }
            println!("joined");
            Ok(())
        }
        "add" => add_member(rest, socket),
        "recover" => {
            let (key, token) = (
                rest.first().ok_or(CliError::Usage)?,
                rest.get(1).ok_or(CliError::Usage)?,
            );
            let offer = optional_size(rest, "--offer")?;
            let site = flag_value(rest, "--site").unwrap_or_default();
            Client::new(socket).join_with_key(token, offer, &site, key)?;
            println!("recovered: this node is back in the filesystem");
            Ok(())
        }
        "recovery-key" => {
            let key = Client::new(socket).recovery_key()?;
            println!("{key}");
            eprintln!(
                "\nA new recovery key. Anyone holding it can join and read this filesystem: keep it\n\
                 as safe as the first one. The older keys still work."
            );
            Ok(())
        }
        "token" => {
            print_token(&Client::new(socket).new_token()?, rest);
            Ok(())
        }
        "update" => {
            let release = rest
                .iter()
                .find(|a| !a.starts_with('-'))
                .map(String::as_str);
            let force = rest.iter().any(|a| a == "--force");
            let report = Client::new(socket).update_with(release, force)?;
            if report.already_on_it {
                println!(
                    "the cluster is already on {}; nothing to do",
                    report.release
                );
            } else {
                println!(
                    "moving the cluster to {}: the members stage it, then restart (see `startfs status`)",
                    report.release
                );
            }
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
        "config" => config(rest, socket),
        "keep" => {
            let path = absolute(positional(rest, &[]).ok_or(CliError::Usage)?)?;
            let keep = !rest.iter().any(|a| a == "--release");
            let report = Client::new(socket).keep_local(&path, keep)?;
            if keep {
                println!(
                    "keeping {path} local: {} chunk{}, {} fetched{}",
                    report.chunks,
                    if report.chunks == 1 { "" } else { "s" },
                    report.fetched,
                    if report.missing > 0 {
                        format!(
                            ", {} not available now (a read fetches them)",
                            report.missing
                        )
                    } else {
                        String::new()
                    }
                );
            }
            Ok(())
        }
        "free" => {
            let bytes = Client::new(socket).free_space()?;
            println!("freed {} from the cache", human(bytes));
            Ok(())
        }
        "copies" => {
            let path = absolute(positional(rest, &[]).ok_or(CliError::Usage)?)?;
            let copies = if rest.iter().any(|a| a == "--clear") {
                None
            } else {
                let n = rest
                    .iter()
                    .filter(|a| !a.starts_with('-'))
                    .nth(1)
                    .ok_or(CliError::Usage)?;
                Some(n.parse::<u32>().map_err(|_| CliError::CopiesNotANumber)?)
            };
            Client::new(socket).set_dir_copies(&path, copies)?;
            Ok(())
        }
        "status" => {
            let verbose = rest.iter().any(|a| a == "--verbose" || a == "-v");
            if rest.iter().any(|a| a == "--watch") {
                if rest.iter().any(|a| a == "--json") {
                    return Err(CliError::WatchWithJson);
                }
                let interval = match rest.iter().position(|a| a == "--interval") {
                    Some(i) => parse_interval(
                        rest.get(i + 1)
                            .ok_or_else(|| CliError::MissingValue("--interval".to_string()))?,
                    )?,
                    None => Duration::from_secs(2),
                };
                return watch_status(&Client::new(socket), verbose, interval);
            }
            let status = Client::new(socket).status()?;
            if rest.iter().any(|a| a == "--json") {
                println!(
                    "{}",
                    serde_json::to_string(&status).map_err(CliError::Json)?
                );
            } else {
                print!("{}", render(&status, verbose));
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
        "purge" => {
            let client = Client::new(socket);
            let report = client.purge()?;
            println!(
                "purge started on {} member{}; deleted files' data is freed as each member allows (see `startfs status`)",
                report.asked.len(),
                if report.asked.len() == 1 { "" } else { "s" }
            );
            if !report.deferred.is_empty() {
                let away: Vec<String> = report.deferred.iter().map(|m| m.0.to_string()).collect();
                println!(
                    "away: member{} {} will be told to purge once back and caught up, by this node",
                    if away.len() == 1 { "" } else { "s" },
                    away.join(", ")
                );
            }
            Ok(())
        }
        "history" => {
            let path = absolute(positional(rest, &[]).ok_or(CliError::Usage)?)?;
            let versions = Client::new(socket).history(&path)?;
            if rest.iter().any(|a| a == "--json") {
                println!(
                    "{}",
                    serde_json::to_string(&versions).map_err(CliError::Json)?
                );
            } else {
                print!("{}", history_table(&path, &versions));
            }
            Ok(())
        }
        "restore" => {
            let point = flag_value(rest, "--at")?;
            let path = absolute(positional(rest, &["--at"]).ok_or(CliError::Usage)?)?;
            let client = Client::new(socket);
            // A mark's name can never read as a restore point, so the two are told apart by form.
            let report = match startfs_core::fs::parse_restore_point(&point) {
                Some(at) => client.restore(&path, at)?,
                None => {
                    if !client.marks()?.iter().any(|m| m.name == point) {
                        return Err(CliError::BadRestorePoint(point));
                    }
                    client.restore_mark(&path, &point)?
                }
            };
            println!(
                "restored {} file{} under {path} to what {} held at {point}",
                report.restored,
                if report.restored == 1 { "" } else { "s" },
                if report.restored == 1 { "it" } else { "they" }
            );
            if report.blocked > 0 {
                eprintln!(
                    "{} left as they are: a directory now stands where they were",
                    report.blocked
                );
            }
            Ok(())
        }
        "mark" => mark_command(rest, socket),
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

/// The first argument that is neither a flag nor the value of one of `valued`.
fn positional<'a>(args: &'a [String], valued: &[&str]) -> Option<&'a str> {
    let mut args = args.iter();
    while let Some(a) = args.next() {
        if valued.contains(&a.as_str()) {
            args.next();
        } else if !a.starts_with('-') {
            return Some(a);
        }
    }
    None
}

/// A path as given, made absolute against the working directory (not resolved:
/// the file it names may be gone).
fn absolute(path: &str) -> Result<String, CliError> {
    if path.starts_with('/') {
        return Ok(path.to_string());
    }
    let cwd = std::env::current_dir().map_err(CliError::DataDir)?;
    Ok(cwd.join(path).display().to_string())
}

/// `startfs mark create NAME | list | delete NAME`.
fn mark_command(rest: &[String], socket: PathBuf) -> Result<(), CliError> {
    use startfs_core::fs::restore_point;
    let client = Client::new(socket);
    let mut words = rest.iter().filter(|a| !a.starts_with('-'));
    match (words.next().map(String::as_str), words.next()) {
        (Some("create"), Some(name)) => {
            let m = client.mark_create(name)?;
            println!("marked {} as {}", restore_point(m.at), m.name);
        }
        (Some("delete"), Some(name)) => client.mark_delete(name)?,
        (Some("list"), None) => {
            let marks = client.marks()?;
            if rest.iter().any(|a| a == "--json") {
                println!("{}", serde_json::to_string(&marks).map_err(CliError::Json)?);
            } else if marks.is_empty() {
                println!("no marks (`startfs mark create NAME` names this instant)");
            } else {
                for m in marks {
                    println!("{:<24} {}  member {}", m.name, restore_point(m.at), m.by.0);
                }
            }
        }
        _ => return Err(CliError::Usage),
    }
    Ok(())
}

/// One line per version, oldest first, each named by its restore point.
fn history_table(path: &str, versions: &[control::VersionInfo]) -> String {
    use startfs_core::fs::restore_point;
    let mut out = format!(
        "{path}: {} version{} (restore points in UTC; `startfs restore {path} --at POINT`)\n",
        versions.len(),
        if versions.len() == 1 { "" } else { "s" }
    );
    for v in versions {
        let ended = match (v.replaced, v.deleted) {
            (None, _) => "current".to_string(),
            (Some(t), true) => format!("deleted {}", restore_point(t)),
            (Some(t), false) => format!("replaced {}", restore_point(t)),
        };
        out.push_str(&format!(
            "  {}  {:>10}  {ended}\n",
            restore_point(v.written),
            human(v.size)
        ));
    }
    out
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

/// Seconds between two refreshes of `--watch`; at least 0.2, so it cannot spin.
fn parse_interval(text: &str) -> Result<Duration, CliError> {
    match text.parse::<f64>() {
        Ok(s) if s.is_finite() && (0.2..=3600.0).contains(&s) => Ok(Duration::from_secs_f64(s)),
        _ => Err(CliError::BadInterval(text.to_string())),
    }
}

static WATCH_STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_watch_signal(_: libc::c_int) {
    WATCH_STOP.store(true, Ordering::SeqCst);
}

/// The terminal's size as (rows, cols), or a plain 24x80 when it will not say.
fn terminal_size() -> (usize, usize) {
    let mut ws = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCGWINSZ fills the winsize it is given.
    let ok = unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) } == 0;
    if ok && ws.ws_row > 0 && ws.ws_col > 0 {
        (usize::from(ws.ws_row), usize::from(ws.ws_col))
    } else {
        (24, 80)
    }
}

/// `startfs status --watch`: redraw the summary in place on the terminal's alternate screen
/// until Ctrl-C (or SIGTERM), then give the screen and the cursor back. The daemon is asked
/// once per refresh and only while this runs; a daemon that cannot be reached shows as a
/// line and the watch carries on.
fn config(rest: &[String], socket: PathBuf) -> Result<(), CliError> {
    if let Some(bytes) = optional_size(rest, "--offer")? {
        Client::new(socket.clone()).set_offer(bytes)?;
    }
    if let Some(bytes) = optional_size(rest, "--weight")? {
        Client::new(socket.clone()).set_weight(bytes)?;
    }
    if let Ok(site) = flag_value(rest, "--site") {
        Client::new(socket.clone()).set_site(&site)?;
    }
    if let Ok(value) = flag_value(rest, "--activity") {
        let on = match value.as_str() {
            "on" => true,
            "off" => false,
            _ => return Err(CliError::ActivityNotOnOff),
        };
        Client::new(socket.clone()).set_activity(on)?;
    }
    let has = |flag: &str| rest.iter().any(|a| a == flag);
    if has("--storage-only") && has("--no-storage-only") {
        return Err(CliError::StorageOnlyBoth);
    }
    if has("--storage-only") || has("--no-storage-only") {
        Client::new(socket.clone()).set_storage_only(has("--storage-only"))?;
    }
    if let Ok(window) = flag_value(rest, "--keep-versions") {
        let secs = parse_duration(&window).ok_or_else(|| CliError::BadDuration(window.clone()))?;
        Client::new(socket.clone()).set_keep_versions(secs)?;
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

fn watch_status(client: &Client, verbose: bool, interval: Duration) -> Result<(), CliError> {
    use std::io::{IsTerminal, Write};
    if !std::io::stdout().is_terminal() {
        return Err(CliError::WatchNeedsTerminal);
    }
    // SAFETY: the handler only stores to an atomic flag, which is async-signal-safe.
    unsafe {
        libc::signal(
            libc::SIGINT,
            on_watch_signal as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGTERM,
            on_watch_signal as *const () as libc::sighandler_t,
        );
    }
    let mut out = std::io::stdout();
    let mut draw = |bytes: &[u8]| out.write_all(bytes).and_then(|()| out.flush()).is_ok();
    if !draw(b"\x1b[?1049h\x1b[?25l\x1b[2J") {
        return Ok(());
    }
    while !WATCH_STOP.load(Ordering::SeqCst) {
        let mut text = match client.status() {
            Ok(status) => render(&status, verbose),
            Err(e) => format!("cannot reach the daemon: {e}\n"),
        };
        text.push_str(&format!(
            "\nwatching every {}s, Ctrl-C to quit\n",
            interval.as_secs_f64()
        ));
        let (rows, cols) = terminal_size();
        if !draw(watch_frame(&text, rows, cols).as_bytes()) {
            break; // the terminal is gone
        }
        let until = Instant::now() + interval;
        while Instant::now() < until && !WATCH_STOP.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    draw(b"\x1b[?25h\x1b[?1049l");
    Ok(())
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

/// What a command accepts: flags that take a value, flags that stand alone, and
/// how many bare arguments. Anything else is an error, so a mistyped flag never
/// runs a command that ignores it.
fn check_arguments(command: &str, args: &[String]) -> Result<(), CliError> {
    let (valued, alone, positional): (&[&str], &[&str], usize) = match command {
        "daemon" => (&["--mount"], &[], 0),
        "create" => (&["--name", "--copies"], &["--example", "--encrypt"], 0),
        "status" => (
            &["--interval"],
            &["--json", "--verbose", "-v", "--watch"],
            0,
        ),
        "activity" | "stats" => (&[], &["--json"], 0),
        "history" => (&[], &["--json"], 1),
        "restore" => (&["--at"], &[], 1),
        "mark" => (&[], &["--json"], 2),
        "join" => (&["--offer", "--site", "--key"], &["--storage-only"], 1),
        "add" => (
            &["--offer", "--site", "--via", "--version"],
            &["--storage-only"],
            1,
        ),
        "recover" => (&["--offer", "--site"], &[], 2),
        "token" => (&[], &["--example"], 0),
        "update" => (&[], &["--force"], 1),
        "remove" | "revoke" => (&[], &[], 1),
        "copies" => (&[], &["--clear"], 2),
        "keep" => (&[], &["--release"], 1),
        "config" => (
            &[
                "--offer",
                "--weight",
                "--copies",
                "--activity",
                "--site",
                "--keep-versions",
            ],
            &["--storage-only", "--no-storage-only"],
            0,
        ),
        // purge, leave and unknown commands (reported as usage later)
        _ => (&[], &[], 0),
    };
    let mut bare = 0;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if valued.contains(&arg.as_str()) {
            args.next();
        } else if alone.contains(&arg.as_str()) {
            continue;
        } else {
            if !arg.starts_with('-') {
                bare += 1;
            }
            if arg.starts_with('-') || bare > positional {
                return Err(CliError::UnexpectedArgument(arg.clone()));
            }
        }
    }
    Ok(())
}

/// `startfs add`: see `startfs::add`.
fn add_member(rest: &[String], socket: PathBuf) -> Result<(), CliError> {
    use startfs::add::{self, Plan, SshRemote, Via};
    let host = rest
        .first()
        .filter(|h| !h.starts_with('-'))
        .ok_or(CliError::Usage)?;
    let offer = flag_value(rest, "--offer").ok();
    if let Some(offer) = &offer {
        parse_size(offer)?;
    }
    let storage_only = rest.iter().any(|a| a == "--storage-only");
    if storage_only && offer.is_none() {
        return Err(CliError::StorageOnlyNeedsOffer);
    }
    let via = match flag_value(rest, "--via").ok().as_deref() {
        None => None,
        Some("ssh") => Some(Via::Ssh),
        Some("tailscale") => Some(Via::Tailscale),
        Some(_) => return Err(CliError::BadVia),
    };
    let plan = Plan {
        version: flag_value(rest, "--version")
            .unwrap_or_else(|_| format!("v{}", env!("CARGO_PKG_VERSION"))),
        offer,
        site: flag_value(rest, "--site").ok(),
        storage_only,
    };
    let via = add::choose_transport(via, || add::tailscale_knows(host));
    println!(
        "adding {host} ({}) at {}",
        if matches!(via, Via::Ssh) {
            "ssh"
        } else {
            "tailscale ssh"
        },
        plan.version
    );
    let mut remote = SshRemote {
        via,
        host: host.clone(),
    };
    let report = add::add(&mut remote, &Client::new(socket), &plan)?;
    for warning in &report.warnings {
        eprintln!("warning: {warning}");
    }
    println!("added {host}: watch it arrive with `startfs status`");
    Ok(())
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

/// Seconds in a duration like `7d`, `36h`, `90m` or `30s`; a bare `0` is none.
fn parse_duration(text: &str) -> Option<u64> {
    if text == "0" {
        return Some(0);
    }
    let unit = match text.chars().last()? {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86400,
        _ => return None,
    };
    text[..text.len() - 1]
        .parse::<u64>()
        .ok()?
        .checked_mul(unit)
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
    let host = tailnet_address()?;
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
    let mountpoint = mountpoint.or_else(|| config_mountpoint.map(PathBuf::from));
    if mountpoint.is_some() && d.is_storage_only() {
        return Err(CliError::StorageOnlyCannotMount);
    }
    if let Some(mp) = &mountpoint {
        d.set_mountpoint(mp.clone());
    }
    // Before the first tick: what an earlier swap left open is settled here.
    d.set_installer(Arc::new(ScriptInstaller::new(
        &data_dir,
        mountpoint.clone(),
    )));
    let ticker = Ticker::start(d.clone(), transport);
    d.set_power_source(Arc::new(SystemPower));
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

/// The write end of the pipe the signal handler wakes `wait_for_stop` through.
static STOP_PIPE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(_: libc::c_int) {
    let fd = STOP_PIPE.load(Ordering::SeqCst);
    // SAFETY: write(2) is async-signal-safe; a failed or short write only loses a wake-up
    // that the pipe already holds.
    unsafe {
        libc::write(fd, [0u8].as_ptr().cast(), 1);
    }
}

/// Block until SIGTERM or SIGINT arrives.
fn wait_for_stop() {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` has room for the two descriptors pipe(2) writes.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        eprintln!("startfs: cannot wait for a signal without a pipe");
        return;
    }
    STOP_PIPE.store(fds[1], Ordering::SeqCst);
    // SAFETY: the handler only writes to a pipe, which is async-signal-safe.
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
    }
    let mut byte = [0u8; 1];
    loop {
        // SAFETY: reads one byte into a buffer of one; an interrupted read is retried.
        let n = unsafe { libc::read(fds[0], byte.as_mut_ptr().cast(), 1) };
        if n > 0 {
            return;
        }
        if n < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return;
        }
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

/// How long a daemon waits for Tailscale to give this machine an address before giving
/// up, so a boot where `tailscaled` starts alongside us does not leave us on loopback.
const TAILNET_WAIT: Duration = Duration::from_secs(60);

/// This machine's tailnet address; loopback when Tailscale is not installed at all.
///
/// With Tailscale installed but not answering (still starting, logged out) this waits,
/// then fails: a node bound to loopback advertises an address no peer can dial, and
/// nothing would ever correct it. Failing lets the service manager retry.
/// `STARTFS_TAILNET_WAIT` (seconds) overrides how long to wait.
fn tailnet_address() -> Result<IpAddr, CliError> {
    let wait = std::env::var("STARTFS_TAILNET_WAIT")
        .ok()
        .and_then(|s| s.parse().ok())
        .map_or(TAILNET_WAIT, Duration::from_secs);
    let started = Instant::now();
    loop {
        match Command::new("tailscale").args(["ip", "-4"]).output() {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(IpAddr::from([127, 0, 0, 1]));
            }
            Ok(out) if out.status.success() => {
                let text = String::from_utf8_lossy(&out.stdout);
                if let Some(ip) = text.lines().next().and_then(|l| l.trim().parse().ok()) {
                    return Ok(ip);
                }
            }
            _ => {}
        }
        if started.elapsed() >= wait {
            return Err(CliError::NoTailnet(wait));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
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
            other => state_word(other),
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
    out.push_str(&format!("  io pressure: {}\n", psi(&r.io_pressure)));
    out.push_str(&format!(
        "  daemon storage writes: {} so far\n",
        bytes(r.written_bytes)
    ));
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
