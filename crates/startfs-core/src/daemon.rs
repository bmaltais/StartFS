//! The daemon: owns the node's state and answers control requests.

use crate::clock::Clock;
use crate::config::Config;
use crate::control::{FilesystemInfo, NodeState, Request, Response, Status, Usage};
use crate::fs::{LocalFs, NodeConfig};
use crate::net::{NodeId, Transport};
use crate::ops::FsOps;
use crate::ops::FsResult;
use crate::sync::Syncer;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub struct Daemon {
    fs: Arc<LocalFs>,
    identity_file: PathBuf,
    config: Config,
    /// Our address on the tailnet, as resolved by the caller.
    advertise_host: String,
    identity: Mutex<Option<FilesystemInfo>>,
    clock: Arc<dyn Clock>,
    /// Log exchange with peers; present once a transport is attached.
    syncer: Mutex<Option<Syncer>>,
}

impl Daemon {
    pub fn open(
        dir: &Path,
        config: Config,
        node: NodeConfig,
        clock: Arc<dyn Clock>,
        advertise_host: String,
    ) -> FsResult<Self> {
        let identity_file = dir.join("filesystem.toml");
        let identity = std::fs::read_to_string(&identity_file)
            .ok()
            .and_then(|t| toml::from_str(&t).ok());
        Ok(Self {
            fs: Arc::new(LocalFs::open(dir, node, clock.clone())?),
            clock,
            syncer: Mutex::new(None),
            identity_file,
            config,
            advertise_host,
            identity: Mutex::new(identity),
        })
    }

    pub fn fs(&self) -> Arc<LocalFs> {
        self.fs.clone()
    }

    /// Start exchanging logs with `peers` over `transport`.
    ///
    /// Membership does not exist yet (`join` arrives with ticket #5), so
    /// callers name the peers directly; the harness does this for its nodes.
    pub fn attach(&self, transport: Arc<dyn Transport>, peers: &[NodeId]) {
        let mut syncer = Syncer::new(self.fs.clone(), transport, self.clock.clone());
        for p in peers {
            syncer.add_peer(*p);
        }
        *self.syncer.lock().unwrap() = Some(syncer);
    }

    /// One round of background work: returns whether anything happened.
    pub fn tick(&self) -> bool {
        self.syncer
            .lock()
            .unwrap()
            .as_mut()
            .is_some_and(Syncer::tick)
    }

    pub fn handle(&self, req: Request) -> Response {
        match req {
            Request::Create { name, copies } => {
                let mut identity = self.identity.lock().unwrap();
                if identity.is_some() {
                    return Response::Error("this node already belongs to a filesystem".into());
                }
                if name.is_empty() || copies == 0 {
                    return Response::Error("need a name and at least 1 copy".into());
                }
                let mut raw = [0u8; 16];
                getrandom::fill(&mut raw).expect("os randomness");
                let id: String = raw.iter().map(|b| format!("{b:02x}")).collect();
                let info = FilesystemInfo { id, name, copies };
                let token = format!(
                    "startfs1-{}@{}:{}",
                    info.id, self.advertise_host, self.config.port
                );
                let text = toml::to_string(&info).expect("serialises");
                if let Err(e) = std::fs::write(&self.identity_file, text) {
                    return Response::Error(format!("cannot save filesystem identity: {e}"));
                }
                *identity = Some(info);
                Response::Created { token }
            }
            Request::Status => {
                let stats = match self.fs.stats() {
                    Ok(s) => s,
                    Err(e) => return Response::Error(e.to_string()),
                };
                let filesystem = self.identity.lock().unwrap().clone();
                let nodes = u32::from(filesystem.is_some());
                let copies_possible = filesystem.as_ref().map_or(0, |f| f.copies.min(nodes));
                Response::Status(Status {
                    filesystem,
                    node_state: NodeState::Online,
                    usage: Usage {
                        chunks: stats.chunks,
                        stored_bytes: stats.stored_bytes,
                        offer_bytes: self.config.offer_bytes,
                    },
                    nodes,
                    copies_possible,
                    conflicts: match self.fs.conflicts() {
                        Ok(c) => c,
                        Err(e) => return Response::Error(e.to_string()),
                    },
                })
            }
        }
    }
}
