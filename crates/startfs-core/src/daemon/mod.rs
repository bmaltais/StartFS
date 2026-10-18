//! The daemon: owns the node's state and answers control requests.
//!
//! Peers talk to it through the [`Transport`]; [`Daemon::tick`] drains the
//! transport and sends whatever is due. A real daemon calls it from a
//! background thread, the test harness calls it to drive simulated time.

mod membership;
mod peers;
mod requests;
mod status;

use crate::chunkstore::ChunkId;
use crate::clock::Clock;
use crate::config::Config;
use crate::control::NodeStats;
use crate::error::Error;
use crate::fs::{ChunkSource, GcReport, LocalFs, NodeConfig};
use crate::members::{Member, MemberId, Membership};
use crate::net::{NodeId, Transport};
use crate::ops::FsResult;
use crate::ops::Nanos;
use crate::power::{MainsPower, PowerSource};
use crate::protocol::{Health, Message};
use crate::replication::{Replicator, View};
use crate::scrub::Scrubber;
use crate::sync::{HELLO_INTERVAL, Syncer};
use std::collections::{BTreeMap, HashMap, HashSet};
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

/// A member unseen for this long is taken for asleep, not rebooting: the
/// others make stand-in copies to cover for it. Also the longest gap between
/// two looks at the cluster before this node suspects it was asleep itself.
const STANDIN_AFTER: Duration = Duration::from_secs(5 * 60);

// Asleep implies down, which the replicator's counting relies on.
const _: () = assert!(DOWN_AFTER.as_secs() <= STANDIN_AFTER.as_secs());

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
    join_reply: Mutex<Option<Result<crate::members::Admission, Error>>>,
    wait: Mutex<WaitHook>,
    /// When each member was last seen asleep (unseen past the stand-in delay);
    /// the extra copies made for it are kept for the stand-in retention after.
    slept: Mutex<HashMap<MemberId, Nanos>>,
    /// When the cluster was last looked at, and until when a long gap since
    /// then (this node was asleep itself) makes every peer look unseen.
    looked: Mutex<(Option<Nanos>, Nanos)>,
    /// When each peer endpoint was last heard from.
    heard: Mutex<HashMap<NodeId, Nanos>>,
    last_gossip: Mutex<Option<Nanos>>,
    /// The latest replication health each peer gossiped.
    peer_health: Mutex<HashMap<NodeId, Health>>,
    /// Storage figures peers sent in answer to the query in flight.
    stats_replies: Mutex<HashMap<NodeId, NodeStats>>,
    last_gc: Mutex<Option<Nanos>>,
    /// What garbage collection has done and is waiting on, for `status`.
    gc: Mutex<GcProgress>,
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

/// Garbage collection as `status` reports it.
#[derive(Default)]
struct GcProgress {
    reclaimed_chunks: u64,
    reclaimed_bytes: u64,
    last: GcReport,
}

/// Whether we hold everything `peer` reported holding at or after `since`.
fn caught_up(synced: &BTreeMap<NodeId, Option<Nanos>>, peer: &NodeId, since: Nanos) -> bool {
    synced
        .get(peer)
        .copied()
        .flatten()
        .is_some_and(|t| t >= since)
}

/// The most the staging area may hold. A node offering space is limited by its
/// disk alone; one offering none keeps what it writes in staging, so it is capped.
fn staging_limit(config: &Config, offer: u64) -> u64 {
    if offer > 0 {
        u64::MAX
    } else {
        config.staging_bytes
    }
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
        fs.set_staging_limit(staging_limit(
            &config,
            membership.my_offer(config.offer_bytes),
        ));
        fs.set_write_buffer_limit(config.write_buffer_bytes);
        if let Some(me) = membership.me() {
            fs.set_node(me);
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
                slept: Mutex::new(HashMap::new()),
                looked: Mutex::new((None, 0)),
                heard: Mutex::new(HashMap::new()),
                last_gossip: Mutex::new(None),
                peer_health: Mutex::new(HashMap::new()),
                stats_replies: Mutex::new(HashMap::new()),
                last_gc: Mutex::new(None),
                gc: Mutex::new(GcProgress::default()),
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
        let mut syncer = Syncer::new(self.fs.clone(), transport.clone(), self.clock.clone());
        syncer.set_chunk_bytes(self.config.state_chunk_bytes);
        *self.syncer.lock().unwrap() = Some(syncer);
        self.membership.lock().unwrap().refresh_self(
            &self.addr(),
            transport.local(),
            self.config.offer_bytes,
        );
        *self.transport.lock().unwrap() = Some(transport);
    }

    /// One round of background work: returns whether anything happened.
    pub fn tick(&self) -> bool {
        self.fs
            .sync_when_quiet(self.config.sync_quiet_ms * 1_000_000);
        let Some(transport) = self.transport() else {
            return false;
        };
        let mut busy = false;
        // Peers and replication only see committed writes; a file still being
        // written keeps batching until it has been quiet for the commit delay.
        self.fs
            .flush_all_logged(false, self.config.write_commit_delay_ms * 1_000_000);
        // Lazy commits reach disk once quiet, and always before peers or
        // replication can build on them.
        if !self.membership.lock().unwrap().peers().is_empty() {
            self.fs.sync_logged();
        }
        self.sync_peers();
        let offer = self
            .membership
            .lock()
            .unwrap()
            .my_offer(self.config.offer_bytes);
        self.fs
            .set_staging_limit(staging_limit(&self.config, offer));
        self.fs
            .set_usable_bytes(status::usable_bytes(&self.membership.lock().unwrap()));
        while let Some((from, raw)) = transport.recv() {
            busy = true;
            self.heard.lock().unwrap().insert(from, self.clock.now());
            match Message::decode(&raw) {
                Ok(msg) => self.handle_peer_message(&*transport, from, msg),
                Err(e) => eprintln!("startfs: dropped a message from node {}: {e}", from.0),
            }
        }
        let (peers, members, tokens) = {
            let m = self.membership.lock().unwrap();
            (m.peers(), m.records(), m.token_records())
        };
        let now = self.clock.now();
        let mut last = self.last_gossip.lock().unwrap();
        if last.is_none_or(|t| now.saturating_sub(t) >= HELLO_INTERVAL.as_nanos() as u64) {
            *last = Some(now);
            let members_msg = Message::Members(members).encode();
            let tokens_msg = Message::Tokens(tokens).encode();
            let health_msg = Message::Health(self.local_health()).encode();
            let copies_msg = self
                .membership
                .lock()
                .unwrap()
                .copies_setting()
                .map(|(copies, rev)| Message::Copies { copies, rev }.encode());
            for p in &peers {
                if let Some(msg) = &copies_msg {
                    transport.send(*p, msg.clone());
                }
                transport.send(*p, members_msg.clone());
                transport.send(*p, tokens_msg.clone());
                transport.send(*p, health_msg.clone());
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
        let (held, settled) = {
            let syncer = self.syncer.lock().unwrap();
            let Some(s) = syncer.as_ref() else {
                return false;
            };
            (s.held_by_all(&gone), s.converged())
        };
        let Some(held) = held else {
            return false;
        };
        *last = Some(now);
        if let Err(e) = self.fs.compact(&held, settled) {
            eprintln!("startfs: cannot compact the operation log: {e}");
        }
        false
    }

    /// Check some replicas if the scrub is due; corrupt ones are deleted and
    /// fetched afresh from another replica holder.
    fn scrub(&self) -> bool {
        let period = Duration::from_secs(self.config.scrub_period_secs);
        let step = self
            .scrubber
            .step(self.clock.now(), period, self.on_battery());
        for id in step.corrupt {
            self.fs.drop_replica(&id);
            self.replicator.heal(id);
        }
        step.worked
    }

    /// Reclaim chunks that have been orphaned for the retention period, once
    /// the reclaim gate opens: this node has caught up with every current
    /// member since the orphan became eligible.
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
        let synced = self.synced_with_peers();
        let mut last = self.last_gc.lock().unwrap();
        let peers = self.membership.lock().unwrap().peers();
        // A pass the gate held back runs again as soon as the gate would open for it.
        let gate_now_open = self
            .gc
            .lock()
            .unwrap()
            .last
            .held_since
            .is_some_and(|since| peers.iter().all(|p| caught_up(&synced, p, since)));
        let due = last.is_none_or(|t| now.saturating_sub(t) >= GC_INTERVAL.as_nanos() as u64);
        if !due && !gate_now_open {
            return false;
        }
        *last = Some(now);
        let Ok(report) = self.fs.collect_garbage(retention, |since| {
            peers.iter().all(|p| caught_up(&synced, p, since))
        }) else {
            eprintln!("startfs: garbage collection failed");
            return false;
        };
        let mut gc = self.gc.lock().unwrap();
        gc.reclaimed_chunks += report.reclaimed_chunks;
        gc.reclaimed_bytes += report.reclaimed_bytes;
        gc.last = report;
        report.reclaimed_chunks > 0
    }

    /// For each peer, when it last reported its own frontier while we held
    /// everything up to it. A peer the syncer does not know yet is missing.
    fn synced_with_peers(&self) -> BTreeMap<NodeId, Option<Nanos>> {
        self.syncer
            .lock()
            .unwrap()
            .as_ref()
            .map(Syncer::synced_with)
            .unwrap_or_default()
    }

    /// The members the reclaim gate is waiting on to free orphans eligible since `since`.
    pub(super) fn gc_waiting_on(&self, since: Nanos) -> Vec<MemberId> {
        let synced = self.synced_with_peers();
        let m = self.membership.lock().unwrap();
        m.members()
            .into_iter()
            .filter(|x| m.peers().contains(&x.endpoint))
            .filter(|x| !caught_up(&synced, &x.endpoint, since))
            .map(|x| x.id)
            .collect()
    }

    fn with_view<T>(&self, f: impl FnOnce(&View<'_>) -> T) -> Option<T> {
        let (me, copies, grace, standin_days, members, offer) = {
            let m = self.membership.lock().unwrap();
            let fs = m.filesystem()?;
            (
                m.me()?,
                fs.copies,
                fs.repair_grace_secs,
                fs.standin_days,
                m.members(),
                m.my_offer(self.config.offer_bytes),
            )
        };
        let now = self.clock.now();
        let lost = self.unseen_members(&members, me, grace, now);
        let down = self.unseen_members(&members, me, grace.min(DOWN_AFTER.as_secs()), now);
        let (sleeping, kept) = self.sleepers(&members, me, grace, standin_days, now);
        Some(f(&View {
            me,
            copies,
            members: &members,
            lost: &lost,
            down: &down,
            sleeping: &sleeping,
            kept: &kept,
            offer,
            repair_rate: self.config.repair_bytes_per_sec,
            now,
        }))
    }

    /// The members asleep now (unseen for the stand-in delay) and those seen
    /// asleep within the retention, which includes the former.
    fn sleepers(
        &self,
        members: &[Member],
        me: MemberId,
        grace: u64,
        standin_days: u64,
        now: Nanos,
    ) -> (HashSet<MemberId>, HashSet<MemberId>) {
        if standin_days == 0 {
            return (HashSet::new(), HashSet::new());
        }
        // After a long gap this node was likely asleep itself, so every peer
        // looks unseen until their hellos arrive: nobody is taken for asleep then.
        let settling = {
            let mut looked = self.looked.lock().unwrap();
            let stalled = looked
                .0
                .is_some_and(|at| now.saturating_sub(at) >= STANDIN_AFTER.as_nanos() as u64);
            looked.0 = Some(now);
            if stalled {
                looked.1 = now + DOWN_AFTER.as_nanos() as u64;
            }
            now < looked.1
        };
        let sleeping = if settling {
            HashSet::new()
        } else {
            self.unseen_members(members, me, grace.min(STANDIN_AFTER.as_secs()), now)
        };
        let retention = Duration::from_secs(standin_days * 24 * 3600).as_nanos() as u64;
        let mut slept = self.slept.lock().unwrap();
        for m in &sleeping {
            slept.insert(*m, now);
        }
        slept.retain(|_, at| now.saturating_sub(*at) < retention);
        let kept = slept.keys().copied().collect();
        (sleeping, kept)
    }

    /// Seconds the extra copies made for `member` are still kept, if it was
    /// seen asleep within the retention.
    pub(super) fn standin_secs_left(
        &self,
        member: MemberId,
        standin_days: u64,
        now: Nanos,
    ) -> Option<u64> {
        let retention = Duration::from_secs(standin_days * 24 * 3600).as_nanos() as u64;
        let at = *self.slept.lock().unwrap().get(&member)?;
        let left = retention.checked_sub(now.saturating_sub(at))?;
        Some(Duration::from_nanos(left).as_secs())
    }

    /// Members unseen for `secs`.
    fn unseen_members(
        &self,
        members: &[Member],
        me: MemberId,
        grace: u64,
        now: Nanos,
    ) -> HashSet<MemberId> {
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
        let peers = {
            let m = self.membership.lock().unwrap();
            if let Some(t) = self.transport() {
                for member in m.members() {
                    t.learn(member.endpoint, &member.addr);
                }
            }
            m.peers()
        };
        if let Some(s) = self.syncer.lock().unwrap().as_mut() {
            s.set_peers(&peers);
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
}
