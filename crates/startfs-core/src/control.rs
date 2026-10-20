//! The control interface: how the CLI (and the test harness) talks to the
//! daemon over a local unix socket. One JSON request line, one JSON response line.

use crate::activity::ActivityReport;
use crate::cache::CacheStats;
use crate::daemon::Daemon;
use crate::error::Error;
use crate::fs::ConflictCopy;
use crate::load::Load;
use crate::members::MemberId;
use crate::ops::Nanos;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    Create {
        name: String,
        copies: u32,
    },
    Status,
    /// This node's recent traffic, tick timings, queues and resource use.
    Activity,
    /// Ask every reachable member for its storage figures.
    NodeStats,
    /// Reclaim the chunks no file uses on every member now, instead of after
    /// the retention period. The reclaim gate still applies.
    Purge,
    /// Mint another join token (valid for the configured time).
    NewToken,
    /// Change settings of this node; unset fields stay as they are.
    Config {
        offer_bytes: Option<u64>,
        /// Change the share of new copies this node takes (0: follow the offer).
        #[serde(default)]
        weight_bytes: Option<u64>,
        /// Change the filesystem's number of copies (for every member).
        #[serde(default)]
        copies: Option<u32>,
        /// Switch the activity counters on or off, until the daemon restarts.
        #[serde(default)]
        activity: Option<bool>,
    },
    /// Make a token we issued stop working.
    Revoke {
        token: String,
    },
    /// Hand this node's data to the others, then leave the filesystem.
    Leave,
    /// Remove a member that is gone for good; its data is re-replicated.
    Remove {
        member: MemberId,
    },
    /// Join the filesystem named by a token, optionally setting our space offer.
    Join {
        token: String,
        offer_bytes: Option<u64>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilesystemInfo {
    pub id: String,
    pub name: String,
    /// N: copies of every chunk. Set at creation; `config --copies` changes it.
    pub copies: u32,
    /// Bumped each time `copies` is changed; the highest wins when members disagree.
    #[serde(default)]
    pub copies_rev: u64,
    /// How long a member may be unseen before repair treats it as lost.
    #[serde(default = "default_grace")]
    pub repair_grace_secs: u64,
    /// How long a chunk nothing references is kept before GC removes it.
    #[serde(default = "default_retention")]
    pub gc_retention_secs: u64,
    /// Days the extra copies made for a sleeping member are kept once it is
    /// back; 0 means no stand-in copies are made for members that are only asleep.
    #[serde(default = "default_standin_days")]
    pub standin_days: u64,
}

fn default_standin_days() -> u64 {
    crate::config::DEFAULT_STANDIN_DAYS
}

fn default_grace() -> u64 {
    30 * 60
}

fn default_retention() -> u64 {
    7 * 24 * 3600
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeState {
    Online,
    /// Not heard from recently.
    Down,
    /// Unseen past the repair grace period: its copies no longer count.
    Lost,
    /// Handing its data to the others before it goes.
    Leaving,
}

/// One member of the filesystem, as `status` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberStatus {
    pub id: MemberId,
    pub addr: String,
    pub state: NodeState,
    /// Space the member offers to the filesystem.
    pub offer_bytes: u64,
    /// The share of new copies it takes when that differs from the offer; 0
    /// means the offer.
    #[serde(default)]
    pub weight_bytes: u64,
    /// Whether this is the node answering.
    pub is_self: bool,
    /// Why this node cannot reach the member, when it is not up (for example a protocol
    /// version mismatch).
    #[serde(default)]
    pub problem: Option<String>,
    /// Seconds the extra copies made for this member are still kept, when it
    /// was seen asleep lately (its chunks stay at N reachable copies meanwhile).
    #[serde(default)]
    pub standin_secs_left: Option<u64>,
    /// When the cluster last saw the member, when it is not up.
    #[serde(default)]
    pub last_seen: Option<LastSeen>,
    /// Round-trip time from this node to the member, in microseconds, when
    /// it is up and a measurement exists. Never set for this node itself.
    #[serde(default)]
    pub rtt_micros: Option<u64>,
    /// How far behind the member is, when it is up but its tick or inbox is
    /// backed up. Up members that are behind get no new copies until they catch up.
    #[serde(default)]
    pub behind: Option<Load>,
}

/// How long ago any member last heard from a member that is not up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastSeen {
    pub secs_ago: u64,
    /// False when no member remembers a sighting, so the figure is only how
    /// long this daemon has been running: the real gap is at least that.
    pub exact: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub chunks: u64,
    pub stored_bytes: u64,
    /// Space this node offers to the filesystem.
    pub offer_bytes: u64,
}

/// Where the space went: what the files add up to, what dedup and
/// compression make of it, and this node's share of the replicas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Efficiency {
    /// Bytes the files in the tree add up to (what the user stored).
    pub logical_bytes: u64,
    /// Plaintext bytes of the distinct chunks the tree references (inline files not included).
    pub referenced_bytes: u64,
    /// Bytes of small files kept in the metadata rather than in chunks.
    #[serde(default)]
    pub inline_bytes: u64,
    /// Plaintext bytes of the referenced chunks this node holds.
    pub local_plain_bytes: u64,
    /// What those chunks take on this node's disk.
    pub local_disk_bytes: u64,
    /// Of those, stored as is because compression did not help.
    pub raw_chunks: u64,
    /// Compressed with zstd.
    pub zstd_chunks: u64,
    /// Plaintext bytes of the chunks compressed with zstd.
    #[serde(default)]
    pub zstd_plain_bytes: u64,
}

/// One node's storage figures: what it holds and how well it is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeStats {
    pub usage: Usage,
    pub efficiency: Efficiency,
    /// Bytes on the node's disk that no file uses, waiting for garbage collection.
    pub pending_gc_bytes: u64,
}

/// A member's answer to a node-stats query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeReport {
    pub member: MemberId,
    pub addr: String,
    /// Whether this is the node that was asked.
    pub is_self: bool,
    /// `None` when the member did not answer: it is down or unreachable.
    pub stats: Option<NodeStats>,
}

/// Chunks written here that are waiting for replication to finish.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Staging {
    pub chunks: u64,
    pub bytes: u64,
}

/// How safe a chunk (or the worst chunk of a file) is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tier {
    /// All N copies exist on distinct nodes.
    Healthy,
    /// Fewer than N copies.
    Degraded,
    /// A single copy.
    AtRisk,
}

/// Chunks held on `copies` distinct nodes, fewer than N.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shortfall {
    pub copies: u32,
    pub chunks: u32,
}

/// Chunks a member that is not up last reported as a single copy (zero when it
/// reported none). Others may have been given a copy since, but nobody else
/// knows of these chunks, so they may exist only on that member.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stranded {
    pub member: MemberId,
    pub chunks: u32,
    /// How old the report is.
    pub secs_ago: u64,
}

impl Shortfall {
    pub fn tier(&self) -> Tier {
        crate::replication::tier(self.copies)
    }
}

/// Chunks being re-replicated after a member was lost, still to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repair {
    /// Chunks down to a single copy: repaired first.
    pub at_risk: u32,
    /// Chunks with more than one but fewer than N copies.
    pub degraded: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScrubState {
    /// Nothing to do until the next pass is due.
    Idle,
    /// A pass is under way.
    Running,
    /// A pass is due or under way, but the machine is on battery power.
    PausedOnBattery,
}

/// The background scrub that re-reads every replica to catch silent corruption.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScrubStatus {
    pub state: ScrubState,
    /// When the last full pass finished.
    pub last_pass: Option<Nanos>,
    /// When the next full pass starts.
    pub next_pass: Option<Nanos>,
    /// Replicas the pass under way has yet to check.
    pub remaining: u64,
    /// Replicas found corrupt and still being re-fetched from other copies.
    pub healing: u32,
}

/// How much metadata history this node keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogStatus {
    /// Operation log entries not yet compacted away.
    pub entries: u64,
    /// Objects (files, directories, symlinks) whose merge state is kept,
    /// deleted ones included until every member has seen the delete.
    pub objects: u64,
}

/// Who a purge was sent to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PurgeReport {
    /// Members told to purge, this one included: all of them.
    pub asked: Vec<MemberId>,
}

/// Garbage collection of chunks no file references any more.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GcStatus {
    /// Orphans on this node not yet reclaimed (retention or the reclaim gate).
    pub pending_chunks: u64,
    pub pending_bytes: u64,
    /// Reclaimed by this daemon since it started.
    pub reclaimed_chunks: u64,
    pub reclaimed_bytes: u64,
    /// Members the reclaim gate is waiting to catch up with before it lets
    /// eligible orphans go. Empty when it holds nothing back.
    pub waiting_on: Vec<MemberId>,
    /// A purge is under way on this node: orphans are reclaimed as soon as
    /// the gate allows, whatever the retention period.
    #[serde(default)]
    pub purging: bool,
}

/// The overall answer to "is my data safe right now?", worst condition first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    /// Every file has all the copies that can exist and every member is reachable.
    Safe,
    /// Redundancy is reduced: a member is unreachable or copies are still being made.
    Degraded,
    /// Some data exists as a single copy.
    AtRisk,
    /// Fewer storage nodes than N: everything possible is done, but N copies cannot exist.
    BestEffort,
}

impl Verdict {
    pub fn describe(self) -> &'static str {
        match self {
            Verdict::Safe => "safe",
            Verdict::Degraded => "degraded",
            Verdict::AtRisk => "at risk",
            Verdict::BestEffort => "best-effort",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// The daemon was told to stop and is winding down.
    #[serde(default)]
    pub stopping: bool,
    /// `None` until a filesystem is created or joined.
    pub verdict: Option<Verdict>,
    /// `None` until a filesystem is created or joined.
    pub filesystem: Option<FilesystemInfo>,
    pub node_state: NodeState,
    /// Replica storage on this node.
    pub usage: Usage,
    /// Dedup, compression and this node's share of the replicas.
    #[serde(default)]
    pub efficiency: Efficiency,
    /// Local staging area: written here, not yet released after replication.
    pub staging: Staging,
    /// Chunks fetched from peers; separate from replica storage.
    pub cache: CacheStats,
    /// Members of the filesystem, by id.
    pub members: Vec<MemberStatus>,
    pub nodes: u32,
    /// Best-effort redundancy: min(N, nodes) copies are actually possible.
    pub copies_possible: u32,
    /// Roughly how much data fits: offered space divided by N.
    pub usable_bytes: u64,
    /// Chunks not yet on N nodes anywhere in the cluster, as far as the
    /// reachable members report, counted by how many copies they have, fewest first.
    pub unreplicated: Vec<Shortfall>,
    /// Re-replication under way, summed over this node and the reachable members.
    pub repair: Option<Repair>,
    /// Chunks this node holds that it is still asking the other members about, after a restart
    /// or a change of membership: not counted as short of copies until the answers are in.
    #[serde(default)]
    pub rechecking: u64,
    /// Members whose replication state is missing from `unreplicated` and
    /// `repair`: unreachable, or not heard from yet. Empty when the view is complete.
    pub unheard: Vec<MemberId>,
    /// Single-copy chunks that members not up last reported, which may now
    /// exist only on them.
    #[serde(default)]
    pub stranded: Vec<Stranded>,
    pub scrub: ScrubStatus,
    pub log: LogStatus,
    pub gc: GcStatus,
    /// Things the user should know: redundancy that cannot be met, full nodes.
    pub warnings: Vec<String>,
    /// Conflict copies awaiting the user's attention (delete one to resolve it).
    pub conflicts: Vec<ConflictCopy>,
    /// Set when the tick thread was busy more than 80% of the last minute;
    /// `startfs activity` has the detail.
    #[serde(default)]
    pub tick_busy: Option<TickBusy>,
}

/// The tick thread is the bottleneck, as far as the last minute shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TickBusy {
    pub percent: u32,
    pub avg_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Created { token: String },
    Joined,
    Done,
    Status(Box<Status>),
    Activity(Box<ActivityReport>),
    Purged(PurgeReport),
    NodeStats(Vec<NodeReport>),
    Error(String),
}

/// A running control server; the socket is removed when this is dropped.
pub struct Server {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

pub fn serve(daemon: Arc<Daemon>, path: &Path) -> std::io::Result<Server> {
    #[allow(
        clippy::let_underscore_must_use,
        reason = "a stale socket file may not exist"
    )]
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    listener.set_nonblocking(true)?;
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let thread = std::thread::spawn(move || {
        while !flag.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    #[allow(
                        clippy::let_underscore_must_use,
                        reason = "a failure here surfaces as a read error in answer"
                    )]
                    let _ = stream.set_nonblocking(false);
                    let d = daemon.clone();
                    std::thread::spawn(move || {
                        #[allow(
                            clippy::let_underscore_must_use,
                            reason = "a client that hung up cannot be told"
                        )]
                        let _ = answer(&d, stream);
                    });
                }
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    });
    Ok(Server {
        path: path.to_path_buf(),
        stop,
        thread: Some(thread),
    })
}

fn answer(daemon: &Daemon, stream: UnixStream) -> std::io::Result<()> {
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    let resp = match serde_json::from_str::<Request>(&line) {
        Ok(req) => daemon.handle_control_request(req),
        Err(e) => Response::Error(format!("bad request: {e}")),
    };
    let mut out = &stream;
    writeln!(out, "{}", serde_json::to_string(&resp).expect("serialises"))
}

impl Server {
    /// Block the calling thread for as long as the server runs.
    pub fn wait(mut self) {
        if let Some(t) = self.thread.take() {
            #[allow(
                clippy::let_underscore_must_use,
                reason = "teardown: a panicked server thread has nothing left to report"
            )]
            let _ = t.join();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            #[allow(
                clippy::let_underscore_must_use,
                reason = "teardown: a panicked server thread has nothing left to report"
            )]
            let _ = t.join();
        }
        #[allow(
            clippy::let_underscore_must_use,
            reason = "teardown: the socket file may already be gone"
        )]
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Why a reply was not the one the request expects: the daemon refused, or it
/// answered with something else.
fn unexpected(reply: Response) -> Error {
    match reply {
        Response::Error(e) => Error::Remote(e),
        other => Error::UnexpectedResponse(Box::new(other)),
    }
}

/// Client side of the control interface, used by the CLI and the harness.
#[derive(Debug, Clone)]
pub struct Client {
    socket: PathBuf,
}

impl Client {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    pub fn call(&self, req: &Request) -> Result<Response, Error> {
        let io = |source: std::io::Error| Error::DaemonUnreachable {
            socket: self.socket.clone(),
            source,
        };
        let mut stream = UnixStream::connect(&self.socket).map_err(io)?;
        writeln!(
            stream,
            "{}",
            serde_json::to_string(req).expect("serialises")
        )
        .map_err(io)?;
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).map_err(io)?;
        serde_json::from_str(&line).map_err(Error::BadResponse)
    }

    /// Create a filesystem; returns the join token.
    pub fn create(&self, name: &str, copies: u32) -> Result<String, Error> {
        match self.call(&Request::Create {
            name: name.into(),
            copies,
        })? {
            Response::Created { token } => Ok(token),
            other => Err(unexpected(other)),
        }
    }

    /// Join the filesystem a token names.
    pub fn join(&self, token: &str, offer_bytes: Option<u64>) -> Result<(), Error> {
        match self.call(&Request::Join {
            token: token.into(),
            offer_bytes,
        })? {
            Response::Joined => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    /// Mint another join token.
    pub fn new_token(&self) -> Result<String, Error> {
        match self.call(&Request::NewToken)? {
            Response::Created { token } => Ok(token),
            other => Err(unexpected(other)),
        }
    }

    /// Revoke a token this node issued.
    pub fn revoke(&self, token: &str) -> Result<(), Error> {
        match self.call(&Request::Revoke {
            token: token.into(),
        })? {
            Response::Done => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    /// Change the space this node offers to the filesystem.
    pub fn set_offer(&self, bytes: u64) -> Result<(), Error> {
        match self.call(&Request::Config {
            offer_bytes: Some(bytes),
            weight_bytes: None,
            copies: None,
            activity: None,
        })? {
            Response::Done => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    /// Change the share of new copies this node takes, in the units of the offer;
    /// 0 makes it follow the offer again.
    pub fn set_weight(&self, bytes: u64) -> Result<(), Error> {
        match self.call(&Request::Config {
            offer_bytes: None,
            weight_bytes: Some(bytes),
            copies: None,
            activity: None,
        })? {
            Response::Done => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    /// Change the number of copies of every chunk, on every member.
    pub fn set_copies(&self, copies: u32) -> Result<(), Error> {
        match self.call(&Request::Config {
            offer_bytes: None,
            weight_bytes: None,
            copies: Some(copies),
            activity: None,
        })? {
            Response::Done => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    /// Switch the activity counters on or off (until the daemon restarts).
    pub fn set_activity(&self, on: bool) -> Result<(), Error> {
        match self.call(&Request::Config {
            offer_bytes: None,
            weight_bytes: None,
            copies: None,
            activity: Some(on),
        })? {
            Response::Done => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    /// Reclaim unused chunks on every member now.
    pub fn purge(&self) -> Result<PurgeReport, Error> {
        match self.call(&Request::Purge)? {
            Response::Purged(report) => Ok(report),
            other => Err(unexpected(other)),
        }
    }

    /// This node's activity: recent traffic, tick timings, queues, resources.
    pub fn activity(&self) -> Result<ActivityReport, Error> {
        match self.call(&Request::Activity)? {
            Response::Activity(a) => Ok(*a),
            other => Err(unexpected(other)),
        }
    }

    /// Leave the filesystem once this node's data is safe elsewhere.
    pub fn leave(&self) -> Result<(), Error> {
        match self.call(&Request::Leave)? {
            Response::Done => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    /// Remove a dead member.
    pub fn remove(&self, member: MemberId) -> Result<(), Error> {
        match self.call(&Request::Remove { member })? {
            Response::Done => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    /// Every member's storage figures, fetched from the members themselves now.
    pub fn node_stats(&self) -> Result<Vec<NodeReport>, Error> {
        match self.call(&Request::NodeStats)? {
            Response::NodeStats(rows) => Ok(rows),
            other => Err(unexpected(other)),
        }
    }

    pub fn status(&self) -> Result<Status, Error> {
        match self.call(&Request::Status)? {
            Response::Status(s) => Ok(*s),
            other => Err(unexpected(other)),
        }
    }
}
