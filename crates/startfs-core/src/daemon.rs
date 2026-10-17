//! The daemon: owns the node's state and answers control requests.
//!
//! Peers talk to it through the [`Transport`]; [`Daemon::tick`] drains the
//! transport and sends whatever is due. A real daemon calls it from a
//! background thread, the test harness calls it to drive simulated time.

use crate::clock::Clock;
use crate::config::Config;
use crate::control::{FilesystemInfo, MemberStatus, NodeState, Request, Response, Status, Usage};
use crate::fs::{LocalFs, NodeConfig};
use crate::members::{Membership, Token};
use crate::net::{NodeId, Transport};
use crate::ops::{FsOps, FsResult};
use crate::protocol::Message;
use crate::ops::Nanos;
use crate::sync::{HELLO_INTERVAL, Syncer};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Called while a control request waits for a peer's answer. A real daemon
/// sleeps (its background thread makes progress meanwhile); the test harness
/// pumps the simulated network instead.
pub type WaitHook = Arc<dyn Fn() + Send + Sync>;

/// How many times a request waits (calling the hook each time) for an answer.
const WAIT_ROUNDS: usize = 300;

/// A member not heard from for this long is shown as down.
const DOWN_AFTER: std::time::Duration = std::time::Duration::from_secs(3 * HELLO_INTERVAL.as_secs());

pub struct Daemon {
    fs: Arc<LocalFs>,
    config: Config,
    /// Our address on the tailnet, as resolved by the caller.
    advertise_host: String,
    membership: Mutex<Membership>,
    clock: Arc<dyn Clock>,
    transport: Mutex<Option<Arc<dyn Transport>>>,
    /// Log exchange with peers; present once a transport is attached.
    syncer: Mutex<Option<Syncer>>,
    /// The answer to the join request in flight, if it has arrived.
    join_reply: Mutex<Option<Result<crate::members::Admission, String>>>,
    wait: Mutex<WaitHook>,
    /// When each peer endpoint was last heard from.
    heard: Mutex<HashMap<NodeId, Nanos>>,
}

impl Daemon {
    pub fn open(
        dir: &Path,
        config: Config,
        node: NodeConfig,
        clock: Arc<dyn Clock>,
        advertise_host: String,
    ) -> FsResult<Self> {
        let membership = Membership::load(&dir.join("membership.json"));
        let fs = Arc::new(LocalFs::open(dir, node, clock.clone())?);
        if let Some(me) = membership.me() {
            fs.set_node(NodeId(me));
        }
        Ok(Self {
            fs,
            clock,
            transport: Mutex::new(None),
            syncer: Mutex::new(None),
            join_reply: Mutex::new(None),
            heard: Mutex::new(HashMap::new()),
            wait: Mutex::new(Arc::new(|| {
                std::thread::sleep(std::time::Duration::from_millis(10))
            })),
            membership: Mutex::new(membership),
            config,
            advertise_host,
        })
    }

    pub fn fs(&self) -> Arc<LocalFs> {
        self.fs.clone()
    }

    pub fn set_wait_hook(&self, hook: WaitHook) {
        *self.wait.lock().unwrap() = hook;
    }

    fn addr(&self) -> String {
        format!("{}:{}", self.advertise_host, self.config.port)
    }

    fn transport(&self) -> Option<Arc<dyn Transport>> {
        self.transport.lock().unwrap().clone()
    }

    /// Connect the daemon to the network.
    pub fn attach(&self, transport: Arc<dyn Transport>) {
        let syncer = Syncer::new(self.fs.clone(), transport.clone(), self.clock.clone());
        *self.syncer.lock().unwrap() = Some(syncer);
        self.membership
            .lock()
            .unwrap()
            .refresh_self(transport.local(), self.config.offer_bytes);
        *self.transport.lock().unwrap() = Some(transport);
    }

    /// One round of background work: returns whether anything happened.
    pub fn tick(&self) -> bool {
        let Some(transport) = self.transport() else {
            return false;
        };
        let mut busy = false;
        while let Some((from, raw)) = transport.recv() {
            busy = true;
            self.heard.lock().unwrap().insert(from, self.clock.now());
            if let Some(msg) = Message::decode(&raw) {
                self.dispatch(&*transport, from, msg);
            }
        }
        let peers = self.membership.lock().unwrap().peers();
        let mut syncer = self.syncer.lock().unwrap();
        if let Some(s) = syncer.as_mut() {
            s.set_peers(&peers);
            busy |= s.poll();
        }
        busy
    }

    fn dispatch(&self, transport: &dyn Transport, from: NodeId, msg: Message) {
        match msg {
            Message::JoinRequest {
                fs_id,
                token_id,
                addr,
                offer_bytes,
            } => {
                let now = self.clock.now();
                let result = self
                    .membership
                    .lock()
                    .unwrap()
                    .admit(now, &fs_id, &token_id, from, addr, offer_bytes);
                if let Ok(adm) = &result {
                    let members = Message::Members(adm.members.clone()).encode();
                    for peer in self.membership.lock().unwrap().peers() {
                        transport.send(peer, members.clone());
                    }
                }
                transport.send(from, Message::JoinReply(result).encode());
            }
            Message::JoinReply(reply) => {
                *self.join_reply.lock().unwrap() = Some(reply);
            }
            Message::Members(list) => {
                let mut m = self.membership.lock().unwrap();
                if m.is_member_endpoint(from) {
                    m.merge(list);
                }
            }
            msg @ (Message::Hello(_) | Message::Entries(_)) => {
                if let Some(s) = self.syncer.lock().unwrap().as_mut() {
                    s.handle(from, msg);
                }
            }
        }
    }

    fn wait_for<T>(&self, mut poll: impl FnMut() -> Option<T>) -> Option<T> {
        let hook = self.wait.lock().unwrap().clone();
        for _ in 0..WAIT_ROUNDS {
            if let Some(v) = poll() {
                return Some(v);
            }
            hook();
        }
        poll()
    }

    fn join(&self, token: &str, offer_bytes: Option<u64>) -> Result<(), String> {
        let token = Token::parse(token)?;
        if self.membership.lock().unwrap().filesystem().is_some() {
            return Err("this node already belongs to a filesystem".into());
        }
        if !self.fs.frontier().map_err(|e| e.to_string())?.is_empty() {
            return Err("this node holds files of its own; join needs a fresh node".into());
        }
        let transport = self.transport().ok_or("the daemon has no network")?;
        let bootstrap = transport
            .resolve(&token.addr)
            .ok_or_else(|| format!("cannot reach {}", token.addr))?;
        let offer = offer_bytes.unwrap_or(self.config.offer_bytes);
        *self.join_reply.lock().unwrap() = None;
        transport.send(
            bootstrap,
            Message::JoinRequest {
                fs_id: token.fs_id,
                token_id: token.token_id,
                addr: self.addr(),
                offer_bytes: offer,
            }
            .encode(),
        );
        let reply = self
            .wait_for(|| self.join_reply.lock().unwrap().take())
            .ok_or_else(|| format!("no answer from {}", token.addr))??;
        self.fs.set_node(NodeId(reply.id));
        self.membership.lock().unwrap().install(reply, offer)
    }

    fn mint(&self, membership: &mut Membership) -> Result<String, String> {
        let ttl = std::time::Duration::from_secs(self.config.token_ttl_secs);
        membership.mint_token(self.clock.now() + ttl.as_nanos() as u64)
    }

    pub fn handle(&self, req: Request) -> Response {
        match req {
            Request::Create { name, copies } => {
                let mut membership = self.membership.lock().unwrap();
                if membership.filesystem().is_some() {
                    return Response::Error("this node already belongs to a filesystem".into());
                }
                if name.is_empty() || copies == 0 {
                    return Response::Error("need a name and at least 1 copy".into());
                }
                let mut raw = [0u8; 16];
                getrandom::fill(&mut raw).expect("os randomness");
                let id: String = raw.iter().map(|b| format!("{b:02x}")).collect();
                let endpoint = self.transport().map_or(NodeId(0), |t| t.local());
                let created = membership
                    .create(
                        FilesystemInfo { id, name, copies },
                        endpoint,
                        self.addr(),
                        self.config.offer_bytes,
                    )
                    .and_then(|_| self.mint(&mut membership));
                match created {
                    Ok(token) => Response::Created { token },
                    Err(e) => Response::Error(e),
                }
            }
            Request::NewToken => match self.mint(&mut self.membership.lock().unwrap()) {
                Ok(token) => Response::Created { token },
                Err(e) => Response::Error(e),
            },
            Request::Revoke { token } => {
                match self.membership.lock().unwrap().revoke_token(&token) {
                    Ok(()) => Response::Done,
                    Err(e) => Response::Error(e),
                }
            }
            Request::Join { token, offer_bytes } => match self.join(&token, offer_bytes) {
                Ok(()) => Response::Joined,
                Err(e) => Response::Error(e),
            },
            Request::Status => {
                let stats = match self.fs.stats() {
                    Ok(s) => s,
                    Err(e) => return Response::Error(e.to_string()),
                };
                let membership = self.membership.lock().unwrap();
                let filesystem = membership.filesystem().cloned();
                let now = self.clock.now();
                let heard = self.heard.lock().unwrap();
                let members: Vec<MemberStatus> = membership
                    .members()
                    .into_iter()
                    .map(|m| {
                        let is_self = membership.me() == Some(m.id);
                        let up = is_self
                            || heard.get(&m.endpoint).is_some_and(|t| {
                                now.saturating_sub(*t) < DOWN_AFTER.as_nanos() as u64
                            });
                        MemberStatus {
                            id: m.id,
                            addr: m.addr,
                            state: if up { NodeState::Online } else { NodeState::Down },
                            offer_bytes: m.offer_bytes,
                            is_self,
                        }
                    })
                    .collect();
                let nodes = members.len() as u32;
                let copies_possible = filesystem.as_ref().map_or(0, |f| f.copies.min(nodes));
                Response::Status(Status {
                    filesystem,
                    node_state: NodeState::Online,
                    usage: Usage {
                        chunks: stats.chunks,
                        stored_bytes: stats.stored_bytes,
                        offer_bytes: membership.my_offer(self.config.offer_bytes),
                    },
                    members,
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
