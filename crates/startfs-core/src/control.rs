//! The control interface: how the CLI (and the test harness) talks to the
//! daemon over a local unix socket. One JSON request line, one JSON response line.

use crate::cache::CacheStats;
use crate::daemon::Daemon;
use crate::error::Error;
use crate::fs::ConflictCopy;
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
    /// Mint another join token (valid for the configured time).
    NewToken,
    /// Change settings of this node; unset fields stay as they are.
    Config {
        offer_bytes: Option<u64>,
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
    /// N: copies of every chunk, fixed at creation.
    pub copies: u32,
    /// How long a member may be unseen before repair treats it as lost.
    #[serde(default = "default_grace")]
    pub repair_grace_secs: u64,
    /// How long a chunk nothing references is kept before GC removes it.
    #[serde(default = "default_retention")]
    pub gc_retention_secs: u64,
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
    /// Whether this is the node answering.
    pub is_self: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub chunks: u64,
    pub stored_bytes: u64,
    /// Space this node offers to the filesystem.
    pub offer_bytes: u64,
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

/// A file whose chunks are not all on N distinct nodes yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileHealth {
    pub path: String,
    /// Copies of the file's least replicated chunk.
    pub copies: u32,
    /// N.
    pub wanted: u32,
    pub tier: Tier,
}

/// Chunks this node is re-replicating after a member was lost, still to do.
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
    /// `None` until a filesystem is created or joined.
    pub verdict: Option<Verdict>,
    /// `None` until a filesystem is created or joined.
    pub filesystem: Option<FilesystemInfo>,
    pub node_state: NodeState,
    /// Replica storage on this node.
    pub usage: Usage,
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
    /// Files this node wrote that are not yet on N nodes, worst first.
    pub unreplicated: Vec<FileHealth>,
    /// Re-replication under way on this node, if any.
    pub repair: Option<Repair>,
    pub scrub: ScrubStatus,
    pub log: LogStatus,
    /// Things the user should know: redundancy that cannot be met, full nodes.
    pub warnings: Vec<String>,
    /// Conflict copies awaiting the user's attention (delete one to resolve it).
    pub conflicts: Vec<ConflictCopy>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Created { token: String },
    Joined,
    Done,
    Status(Box<Status>),
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
        })? {
            Response::Done => Ok(()),
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

    pub fn status(&self) -> Result<Status, Error> {
        match self.call(&Request::Status)? {
            Response::Status(s) => Ok(*s),
            other => Err(unexpected(other)),
        }
    }
}
