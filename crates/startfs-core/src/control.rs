//! The control interface: how the CLI (and the test harness) talks to the
//! daemon over a local unix socket. One JSON request line, one JSON response line.

use crate::daemon::Daemon;
use crate::fs::ConflictCopy;
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeState {
    Online,
    /// Not heard from recently.
    Down,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// `None` until a filesystem is created or joined.
    pub filesystem: Option<FilesystemInfo>,
    pub node_state: NodeState,
    pub usage: Usage,
    /// Members of the filesystem, by id.
    pub members: Vec<MemberStatus>,
    pub nodes: u32,
    /// Best-effort redundancy: min(N, nodes) copies are actually possible.
    pub copies_possible: u32,
    /// Conflict copies awaiting the user's attention (delete one to resolve it).
    pub conflicts: Vec<ConflictCopy>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Created { token: String },
    Joined,
    Done,
    Status(Status),
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

    pub fn status(&self) -> Result<Status, String> {
        match self.call(&Request::Status)? {
            Response::Status(s) => Ok(s),
            Response::Error(e) => Err(e),
            other => Err(format!("unexpected response: {other:?}")),
        }
    }
}
