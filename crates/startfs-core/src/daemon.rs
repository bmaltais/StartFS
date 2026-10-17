//! The daemon: owns the node's state and answers control requests.
//!
//! Peers talk to it through the [`Transport`]; [`Daemon::tick`] drains the
//! transport and sends whatever is due. A real daemon calls it from a
//! background thread, the test harness calls it to drive simulated time.

use crate::chunkstore::ChunkId;
use crate::clock::Clock;
use crate::config::Config;
use crate::control::{
    FilesystemInfo, LogStatus, MemberStatus, NodeState, Request, Response, Staging, Status, Usage,
};
use crate::fs::{ChunkSource, LocalFs, NodeConfig};
use crate::members::{Member, Membership, Token};
use crate::net::{NodeId, Transport};
use crate::power::{MainsPower, PowerSource};
use crate::ops::Nanos;
use crate::ops::{FsOps, FsResult};
use crate::protocol::{Message, Replica};
use crate::replication::{Replicator, View};
use crate::scrub::Scrubber;
use crate::sync::{HELLO_INTERVAL, Syncer};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

/// Called while a control request waits for a peer's answer. A real daemon
/// sleeps (its background thread makes progress meanwhile); the test harness
/// pumps the simulated network instead.
pub type WaitHook = Arc<dyn Fn() + Send + Sync>;

/// How many times a request waits (calling the hook each time) for an answer.
const WAIT_ROUNDS: usize = 300;

/// A member not heard from for this long is shown as down.
/// How often the operation log is compacted.
const COMPACT_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// How often the garbage collector looks for orphaned chunks.
const GC_INTERVAL: Duration = Duration::from_secs(10 * 60);

const DOWN_AFTER: std::time::Duration =
    std::time::Duration::from_secs(3 * HELLO_INTERVAL.as_secs());

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
    last_gossip: Mutex<Option<Nanos>>,
    last_gc: Mutex<Option<Nanos>>,
    last_compact: Mutex<Option<Nanos>>,
    /// When this daemon started: how long a never-heard member has been unseen.
    started: Nanos,
    /// Answers to chunk requests, by peer and chunk, for the reader waiting on them.
    chunk_replies: Mutex<HashMap<(NodeId, ChunkId), ChunkAnswer>>,
    /// Pushes new chunks to the nodes placement chose and tracks their copies.
    replicator: Replicator,
    scrubber: Scrubber,
    power: Mutex<Arc<dyn PowerSource>>,
}

/// A peer's answer to a chunk request: the bytes, or `None` if it lacks them.
type ChunkAnswer = Option<Vec<u8>>;

/// Fetches chunks from peers on behalf of the filesystem.
struct PeerChunks(Weak<Daemon>);

impl ChunkSource for PeerChunks {
    fn fetch(&self, id: &ChunkId, accept: &dyn Fn(&[u8]) -> bool) -> Option<Vec<u8>> {
        self.0.upgrade()?.fetch_chunk(id, accept)
    }
}

impl Daemon {
    pub fn open(
        dir: &Path,
        config: Config,
        node: NodeConfig,
        clock: Arc<dyn Clock>,
        advertise_host: String,
    ) -> FsResult<Arc<Self>> {
        let membership = Membership::load(&dir.join("membership.json"));
        let fs = Arc::new(LocalFs::open(dir, node, config.cache_bytes, clock.clone())?);
        fs.set_staging_limit(config.staging_bytes);
        if let Some(me) = membership.me() {
            fs.set_node(NodeId(me));
            fs.set_replicating();
        }
        let started = clock.now();
        Ok(Arc::new_cyclic(|me| {
            fs.set_chunk_source(Arc::new(PeerChunks(me.clone())));
            Self {
                replicator: Replicator::new(fs.clone()),
                scrubber: Scrubber::new(fs.clone()),
                power: Mutex::new(Arc::new(MainsPower)),
                fs,
                clock,
                chunk_replies: Mutex::new(HashMap::new()),
                transport: Mutex::new(None),
                syncer: Mutex::new(None),
                join_reply: Mutex::new(None),
                heard: Mutex::new(HashMap::new()),
                last_gossip: Mutex::new(None),
                last_gc: Mutex::new(None),
                last_compact: Mutex::new(None),
                started,
                wait: Mutex::new(Arc::new(|| {
                    std::thread::sleep(std::time::Duration::from_millis(10))
                })),
                membership: Mutex::new(membership),
                config,
                advertise_host,
            }
        }))
    }

    pub fn fs(&self) -> Arc<LocalFs> {
        self.fs.clone()
    }

    /// Where to learn whether the machine is on battery power.
    pub fn set_power_source(&self, power: Arc<dyn PowerSource>) {
        *self.power.lock().unwrap() = power;
    }

    fn on_battery(&self) -> bool {
        self.power.lock().unwrap().on_battery()
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
        self.sync_peers();
        while let Some((from, raw)) = transport.recv() {
            busy = true;
            self.heard.lock().unwrap().insert(from, self.clock.now());
            if let Some(msg) = Message::decode(&raw) {
                self.dispatch(&*transport, from, msg);
            }
        }
        let (peers, members) = {
            let m = self.membership.lock().unwrap();
            (m.peers(), m.records())
        };
        let now = self.clock.now();
        let mut last = self.last_gossip.lock().unwrap();
        if last.is_none_or(|t| now.saturating_sub(t) >= HELLO_INTERVAL.as_nanos() as u64) {
            *last = Some(now);
            let msg = Message::Members(members).encode();
            for p in &peers {
                transport.send(*p, msg.clone());
            }
            busy |= !peers.is_empty();
        }
        drop(last);
        if let Some(s) = self.syncer.lock().unwrap().as_mut() {
            busy |= s.poll();
        }
        busy |= self
            .with_view(|v| self.replicator.poll(v, &*transport))
            .unwrap_or(false);
        busy |= self.finish_leaving(&*transport);
        busy |= self.collect_garbage();
        busy |= self.compact_log();
        if !busy {
            // The scrub only gets the disk when nothing else wants it.
            busy |= self.scrub();
        }
        busy
    }

    /// Drop log entries that every member still around already holds.
    fn compact_log(&self) -> bool {
        let now = self.clock.now();
        let mut last = self.last_compact.lock().unwrap();
        if last.is_some_and(|t| now.saturating_sub(t) < COMPACT_INTERVAL.as_nanos() as u64) {
            return false;
        }
        let gone: Vec<NodeId> = self
            .with_view(|v| {
                v.members
                    .iter()
                    .filter(|m| v.lost.contains(&m.id))
                    .map(|m| m.endpoint)
                    .collect()
            })
            .unwrap_or_default();
        let held = self
            .syncer
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|s| s.held_by_all(&gone));
        let Some(held) = held else {
            return false;
        };
        *last = Some(now);
        let _ = self.fs.compact(&held);
        false
    }

    /// Check some replicas if the scrub is due; corrupt ones are deleted and
    /// fetched afresh from another replica holder.
    fn scrub(&self) -> bool {
        let period = Duration::from_secs(self.config.scrub_period_secs);
        let step = self.scrubber
            .step(self.clock.now(), period, self.on_battery());
        for id in step.corrupt {
            self.fs.drop_replica(&id);
            self.replicator.heal(id);
        }
        step.worked
    }

    /// Reclaim chunks that have been orphaned for the retention period.
    fn collect_garbage(&self) -> bool {
        let Some(retention) = self
            .membership
            .lock()
            .unwrap()
            .filesystem()
            .map(|f| Duration::from_secs(f.gc_retention_secs))
        else {
            return false;
        };
        let now = self.clock.now();
        let mut last = self.last_gc.lock().unwrap();
        if last.is_some_and(|t| now.saturating_sub(t) < GC_INTERVAL.as_nanos() as u64) {
            return false;
        }
        *last = Some(now);
        self.fs
            .collect_garbage(retention)
            .is_ok_and(|r| r.reclaimed_chunks > 0)
    }

    /// A leaving node goes once its data is on the others and they have its log.
    fn finish_leaving(&self, transport: &dyn Transport) -> bool {
        let (leaving, peers) = {
            let m = self.membership.lock().unwrap();
            (m.is_leaving(), m.peers())
        };
        if !leaving
            || !self.fs.held().unwrap_or_default().is_empty()
            || !self.fs.staged().unwrap_or_default().is_empty()
        {
            return false;
        }
        let gone: Vec<NodeId> = self
            .with_view(|v| {
                v.members
                    .iter()
                    .filter(|m| v.lost.contains(&m.id))
                    .map(|m| m.endpoint)
                    .collect()
            })
            .unwrap_or_default();
        let synced = self
            .syncer
            .lock()
            .unwrap()
            .as_ref()
            .is_none_or(|s| s.peers_have_our_log(&gone));
        if !synced {
            return false;
        }
        let Some(records) = self.membership.lock().unwrap().finish_leave() else {
            return false;
        };
        let msg = Message::Members(records).encode();
        for p in peers {
            transport.send(p, msg.clone());
        }
        true
    }

    /// Run `f` with the replicator's view of the filesystem, if we belong to one.
    fn with_view<T>(&self, f: impl FnOnce(&View<'_>) -> T) -> Option<T> {
        let (me, copies, grace, members, offer) = {
            let m = self.membership.lock().unwrap();
            let fs = m.filesystem()?;
            (
                m.me()?,
                fs.copies,
                fs.repair_grace_secs,
                m.members(),
                m.my_offer(self.config.offer_bytes),
            )
        };
        let now = self.clock.now();
        let lost = self.lost_members(&members, me, grace, now);
        Some(f(&View {
            me,
            copies,
            members: &members,
            lost: &lost,
            offer,
            repair_rate: self.config.repair_bytes_per_sec,
            now,
        }))
    }

    /// Members unseen for the repair grace period: repair treats them as gone.
    fn lost_members(&self, members: &[Member], me: u32, grace: u64, now: Nanos) -> HashSet<u32> {
        let heard = self.heard.lock().unwrap();
        let grace = Duration::from_secs(grace).as_nanos() as u64;
        members
            .iter()
            .filter(|m| m.id != me)
            .filter(|m| {
                let seen = heard.get(&m.endpoint).copied().unwrap_or(self.started);
                now.saturating_sub(seen) >= grace
            })
            .map(|m| m.id)
            .collect()
    }

    /// Exchange logs with exactly the other members.
    fn sync_peers(&self) {
        let peers = self.membership.lock().unwrap().peers();
        if let Some(s) = self.syncer.lock().unwrap().as_mut() {
            s.set_peers(&peers);
        }
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
                let result = self.membership.lock().unwrap().admit(
                    now,
                    &fs_id,
                    &token_id,
                    from,
                    addr,
                    offer_bytes,
                );
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
            Message::ChunkRequest(id) => {
                if self.membership.lock().unwrap().is_member_endpoint(from) {
                    let data = self.fs.local_chunk(&id);
                    transport.send(from, Message::ChunkReply(id, data).encode());
                }
            }
            Message::ChunkReply(id, data) => {
                if self.membership.lock().unwrap().is_member_endpoint(from) {
                    let healed = self
                        .with_view(|v| self.replicator.on_heal_reply(v, &id, data.as_deref()))
                        .unwrap_or(false);
                    if !healed {
                        self.chunk_replies.lock().unwrap().insert((from, id), data);
                    }
                }
            }
            Message::ChunkPush(id, data) => {
                if self.membership.lock().unwrap().is_member_endpoint(from) {
                    let standing = self
                        .with_view(|v| self.replicator.on_push(v, &id, data.as_deref()))
                        .unwrap_or(Replica::Full);
                    transport.send(from, Message::ChunkAck(id, standing).encode());
                }
            }
            Message::ChunkAck(id, standing) => {
                if self.membership.lock().unwrap().is_member_endpoint(from) {
                    self.with_view(|v| self.replicator.on_ack(v, from, &id, standing));
                }
            }
            Message::Members(list) => {
                let mut m = self.membership.lock().unwrap();
                if m.is_member_endpoint(from) && m.merge(list) {
                    drop(m);
                    self.sync_peers();
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

    /// Ask peers, most recently heard first, until one supplies the chunk.
    fn fetch_chunk(&self, id: &ChunkId, accept: &dyn Fn(&[u8]) -> bool) -> Option<Vec<u8>> {
        let transport = self.transport()?;
        let mut peers = self.membership.lock().unwrap().peers();
        let heard = self.heard.lock().unwrap().clone();
        peers.sort_by_key(|p| std::cmp::Reverse(heard.get(p).copied()));
        for peer in peers {
            self.chunk_replies.lock().unwrap().remove(&(peer, *id));
            transport.send(peer, Message::ChunkRequest(*id).encode());
            let reply = self.wait_for(|| self.chunk_replies.lock().unwrap().remove(&(peer, *id)));
            if let Some(Some(data)) = reply
                && accept(&data)
            {
                return Some(data);
            }
        }
        None
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
        self.membership.lock().unwrap().install(reply, offer)?;
        self.fs.set_replicating();
        Ok(())
    }

    /// Tell every member about a change to the member list right away.
    fn announce_members(&self) {
        let Some(transport) = self.transport() else {
            return;
        };
        let (peers, records) = {
            let m = self.membership.lock().unwrap();
            (m.peers(), m.records())
        };
        let msg = Message::Members(records).encode();
        for p in peers {
            transport.send(p, msg.clone());
        }
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
                        FilesystemInfo {
                            id,
                            name,
                            copies,
                            repair_grace_secs: self.config.repair_grace_secs,
                            gc_retention_secs: self.config.gc_retention_secs,
                        },
                        endpoint,
                        self.addr(),
                        self.config.offer_bytes,
                    )
                    .and_then(|_| self.mint(&mut membership));
                match created {
                    Ok(token) => {
                        self.fs.set_replicating();
                        Response::Created { token }
                    }
                    Err(e) => Response::Error(e),
                }
            }
            Request::NewToken => match self.mint(&mut self.membership.lock().unwrap()) {
                Ok(token) => Response::Created { token },
                Err(e) => Response::Error(e),
            },
            Request::Config { offer_bytes } => {
                let mut membership = self.membership.lock().unwrap();
                if let Some(bytes) = offer_bytes
                    && let Err(e) = membership.set_offer(bytes)
                {
                    return Response::Error(e);
                }
                Response::Done
            }
            Request::Revoke { token } => {
                match self.membership.lock().unwrap().revoke_token(&token) {
                    Ok(()) => Response::Done,
                    Err(e) => Response::Error(e),
                }
            }
            Request::Leave => {
                let begun = self.membership.lock().unwrap().begin_leave();
                match begun {
                    Ok(()) => {
                        self.announce_members();
                        Response::Done
                    }
                    Err(e) => Response::Error(e),
                }
            }
            Request::Remove { member } => {
                let removed = {
                    let mut m = self.membership.lock().unwrap();
                    let up = m
                        .members()
                        .into_iter()
                        .find(|x| x.id == member)
                        .is_some_and(|x| {
                            self.heard
                                .lock()
                                .unwrap()
                                .get(&x.endpoint)
                                .is_some_and(|t| {
                                    self.clock.now().saturating_sub(*t)
                                        < DOWN_AFTER.as_nanos() as u64
                                })
                        });
                    if up {
                        Err(format!(
                            "member {member} is still online; it can leave with `startfs leave`"
                        ))
                    } else {
                        m.remove(member)
                    }
                };
                match removed {
                    Ok(()) => {
                        self.sync_peers();
                        self.announce_members();
                        Response::Done
                    }
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
                let unreplicated = self
                    .with_view(|v| self.replicator.unreplicated(v))
                    .unwrap_or_default();
                let repair = self
                    .with_view(|v| self.replicator.repair_progress(v))
                    .flatten();
                let lost = self.with_view(|v| v.lost.clone()).unwrap_or_default();
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
                            state: if m.leaving {
                                NodeState::Leaving
                            } else if up {
                                NodeState::Online
                            } else if lost.contains(&m.id) {
                                NodeState::Lost
                            } else {
                                NodeState::Down
                            },
                            offer_bytes: m.offer_bytes,
                            is_self,
                        }
                    })
                    .collect();
                let nodes = members.len() as u32;
                // Only members offering space, and with room left, can hold a copy.
                let full = self.replicator.full_members();
                let storage_nodes = membership
                    .members()
                    .iter()
                    .filter(|m| m.offer_bytes > 0 && !m.leaving && !full.contains(&m.id))
                    .count() as u32;
                let copies_possible = filesystem
                    .as_ref()
                    .map_or(0, |f| f.copies.min(storage_nodes));
                let offered: u64 = members.iter().map(|m| m.offer_bytes).sum();
                let usable_bytes = filesystem
                    .as_ref()
                    .map_or(0, |f| offered / u64::from(f.copies.max(1)));
                let mut warnings = Vec::new();
                for m in &members {
                    if full.contains(&m.id) {
                        warnings.push(format!(
                            "{} is full: it cannot take new copies{}",
                            m.addr,
                            if m.is_self { " (this node)" } else { "" }
                        ));
                    }
                }
                if let Some(f) = &filesystem
                    && copies_possible < f.copies
                {
                    warnings.push(format!(
                        "redundancy: {} wanted, {} possible (only {} node{} can store copies)",
                        f.copies,
                        copies_possible,
                        storage_nodes,
                        if storage_nodes == 1 { "" } else { "s" }
                    ));
                }
                Response::Status(Box::new(Status {
                    filesystem,
                    node_state: NodeState::Online,
                    usage: Usage {
                        chunks: stats.chunks,
                        stored_bytes: stats.stored_bytes,
                        offer_bytes: membership.my_offer(self.config.offer_bytes),
                    },
                    staging: match self.fs.staging_stats() {
                        Ok(s) => Staging {
                            chunks: s.chunks,
                            bytes: s.stored_bytes,
                        },
                        Err(e) => return Response::Error(e.to_string()),
                    },
                    cache: self.fs.cache_stats(),
                    members,
                    nodes,
                    copies_possible,
                    usable_bytes,
                    unreplicated,
                    repair,
                    log: LogStatus {
                        entries: self.fs.log_len().unwrap_or(0),
                    },
                    scrub: self.scrubber.status(
                        now,
                        Duration::from_secs(self.config.scrub_period_secs),
                        self.on_battery(),
                        self.replicator.healing(),
                    ),
                    warnings,
                    conflicts: match self.fs.conflicts() {
                        Ok(c) => c,
                        Err(e) => return Response::Error(e.to_string()),
                    },
                }))
            }
        }
    }
}
