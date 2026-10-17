//! The control interface: how the CLI (and the test harness) talks to the
//! daemon over a local unix socket. One JSON request line, one JSON response line.

use crate::cache::CacheStats;
use crate::daemon::Daemon;
use crate::fs::ConflictCopy;
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
        member: u32,
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
    pub id: u32,
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
    /// Replicas found corrupt and still being re-fetched from other copies.
    pub healing: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
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
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    listener.set_nonblocking(true)?;
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let thread = std::thread::spawn(move || {
        while !flag.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    let d = daemon.clone();
                    std::thread::spawn(move || {
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
        Ok(req) => daemon.handle(req),
        Err(e) => Response::Error(format!("bad request: {e}")),
    };
    let mut out = &stream;
    writeln!(out, "{}", serde_json::to_string(&resp).expect("serialises"))
}

impl Server {
    /// Block the calling thread for as long as the server runs.
    pub fn wait(mut self) {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let _ = std::fs::remove_file(&self.path);
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

    pub fn call(&self, req: &Request) -> Result<Response, String> {
        let io =
            |e: std::io::Error| format!("cannot reach daemon at {}: {e}", self.socket.display());
        let mut stream = UnixStream::connect(&self.socket).map_err(io)?;
        writeln!(
            stream,
            "{}",
            serde_json::to_string(req).expect("serialises")
        )
        .map_err(io)?;
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).map_err(io)?;
        serde_json::from_str(&line).map_err(|e| format!("bad response: {e}"))
    }

    /// Create a filesystem; returns the join token.
    pub fn create(&self, name: &str, copies: u32) -> Result<String, String> {
        match self.call(&Request::Create {
            name: name.into(),
            copies,
        })? {
            Response::Created { token } => Ok(token),
            Response::Error(e) => Err(e),
            other => Err(format!("unexpected response: {other:?}")),
        }
    }

    /// Join the filesystem a token names.
    pub fn join(&self, token: &str, offer_bytes: Option<u64>) -> Result<(), String> {
        match self.call(&Request::Join {
            token: token.into(),
            offer_bytes,
        })? {
            Response::Joined => Ok(()),
            Response::Error(e) => Err(e),
            other => Err(format!("unexpected response: {other:?}")),
        }
    }

    /// Mint another join token.
    pub fn new_token(&self) -> Result<String, String> {
        match self.call(&Request::NewToken)? {
            Response::Created { token } => Ok(token),
            Response::Error(e) => Err(e),
            other => Err(format!("unexpected response: {other:?}")),
        }
    }

    /// Revoke a token this node issued.
    pub fn revoke(&self, token: &str) -> Result<(), String> {
        match self.call(&Request::Revoke {
            token: token.into(),
        })? {
            Response::Done => Ok(()),
            Response::Error(e) => Err(e),
            other => Err(format!("unexpected response: {other:?}")),
        }
    }

    /// Change the space this node offers to the filesystem.
    pub fn set_offer(&self, bytes: u64) -> Result<(), String> {
        match self.call(&Request::Config {
            offer_bytes: Some(bytes),
        })? {
            Response::Done => Ok(()),
            Response::Error(e) => Err(e),
            other => Err(format!("unexpected response: {other:?}")),
        }
    }

    /// Leave the filesystem once this node's data is safe elsewhere.
    pub fn leave(&self) -> Result<(), String> {
        match self.call(&Request::Leave)? {
            Response::Done => Ok(()),
            Response::Error(e) => Err(e),
            other => Err(format!("unexpected response: {other:?}")),
        }
    }

    /// Remove a dead member.
    pub fn remove(&self, member: u32) -> Result<(), String> {
        match self.call(&Request::Remove { member })? {
            Response::Done => Ok(()),
            Response::Error(e) => Err(e),
            other => Err(format!("unexpected response: {other:?}")),
        }
    }

    pub fn status(&self) -> Result<Status, String> {
        match self.call(&Request::Status)? {
            Response::Status(s) => Ok(*s),
            Response::Error(e) => Err(e),
            other => Err(format!("unexpected response: {other:?}")),
        }
    }
}
