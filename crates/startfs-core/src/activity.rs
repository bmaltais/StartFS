//! The daemon's own recent rates and timings, for `startfs activity`: traffic
//! per peer path, how long each tick phase takes, send outcomes, and the sizes
//! of the chunks being replicated. Kept in memory only, over a rolling window
//! of one-second buckets read from the injected [`Clock`].

use crate::clock::Clock;
use crate::control::NodeState;
use crate::members::MemberId;
use crate::net::{NodeId, Transport};
use crate::ops::Nanos;
use crate::protocol::{Category, Message};
use crate::resources::Resources;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Seconds of history the view covers.
pub const WINDOW_SECS: u64 = 60;

const NANOS_PER_SEC: u64 = 1_000_000_000;

/// Upper bounds of the chunk size classes; a last class takes everything larger.
const SIZE_CLASSES: [u64; 5] = [4 << 10, 16 << 10, 64 << 10, 256 << 10, 1 << 20];

/// The parts of a tick that are timed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Flush,
    Sync,
    Receive,
    Gossip,
    Replicate,
    /// Everything else a tick does (garbage collection, compaction, scrub, leaving).
    Other,
}

const PHASES: usize = 6;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Traffic {
    pub bytes: u64,
    pub messages: u64,
}

impl Traffic {
    fn add(&mut self, bytes: u64) {
        self.bytes += bytes;
        self.messages += 1;
    }

    fn merge(&mut self, other: &Traffic) {
        self.bytes += other.bytes;
        self.messages += other.messages;
    }
}

/// Traffic split into the four fixed categories.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Categories {
    pub chunk: Traffic,
    pub metadata: Traffic,
    pub gossip: Traffic,
    pub other: Traffic,
}

impl Categories {
    fn of(&mut self, category: Category) -> &mut Traffic {
        match category {
            Category::Chunk => &mut self.chunk,
            Category::Metadata => &mut self.metadata,
            Category::Gossip => &mut self.gossip,
            Category::Other => &mut self.other,
        }
    }

    fn merge(&mut self, other: &Categories) {
        self.chunk.merge(&other.chunk);
        self.metadata.merge(&other.metadata);
        self.gossip.merge(&other.gossip);
        self.other.merge(&other.other);
    }
}

/// Everything counted for one peer path over the window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerTraffic {
    pub sent: Categories,
    pub received: Categories,
    /// Answers to our chunk pushes (a push that carries the bytes counts again when resent).
    pub acknowledged: u64,
    /// Asks that went unanswered for the retry delay.
    pub timed_out: u64,
    /// Asks sent again after a timeout.
    pub retried: u64,
}

impl PeerTraffic {
    fn merge(&mut self, other: &PeerTraffic) {
        self.sent.merge(&other.sent);
        self.received.merge(&other.received);
        self.acknowledged += other.acknowledged;
        self.timed_out += other.timed_out;
        self.retried += other.retried;
    }
}

/// How long ticks took over the window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TickActivity {
    pub ticks: u64,
    pub avg_ms: f64,
    pub max_ms: f64,
    /// Average milliseconds each phase takes per tick.
    pub avg_phase_ms: PhaseAvg,
    /// Share of the window the tick thread spent working, 0 to 1.
    pub busy_fraction: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PhaseAvg {
    pub flush: f64,
    pub sync: f64,
    pub receive: f64,
    pub gossip: f64,
    pub replicate: f64,
    pub other: f64,
}

/// Chunks replicated whose size is at most `up_to_bytes` (and above the class before it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SizeClass {
    /// `None` for the last class: everything larger.
    pub up_to_bytes: Option<u64>,
    pub chunks: u64,
}

/// One member's row: its state, what waits on it, and the window's counters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerActivity {
    pub member: MemberId,
    pub addr: String,
    pub state: NodeState,
    /// Pushes and requests sent to it that it has not answered.
    pub in_flight: u64,
    /// `None` while the counters are switched off.
    pub traffic: Option<PeerTraffic>,
}

/// The replication queue as it is now.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueActivity {
    /// Chunks the replicator is still working on.
    pub chunks: u64,
    /// Age in seconds of the chunk that has waited longest.
    pub oldest_secs: Option<u64>,
}

/// An in-memory structure: what it holds, and how much it may.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Structure {
    pub entries: u64,
    /// `None` when the structure does not count bytes.
    pub bytes: Option<u64>,
    /// `None` when nothing bounds it.
    pub limit_bytes: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Structures {
    /// Messages that arrived and wait for the tick; `None` when the transport cannot say.
    pub inbox: Option<Structure>,
    /// Chunk answers nobody has claimed yet.
    pub chunk_replies: Option<Structure>,
    pub decoded_cache: Option<Structure>,
    pub replication_index: Option<Structure>,
    pub staging_index: Option<Structure>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActivityReport {
    /// False when the counters are switched off (`config --activity off`).
    pub enabled: bool,
    /// Seconds the counters have been collecting, at most the window.
    pub window_secs: f64,
    pub peers: Vec<PeerActivity>,
    pub tick: Option<TickActivity>,
    pub queue: QueueActivity,
    pub chunk_sizes: Vec<SizeClass>,
    pub structures: Structures,
    pub resources: Resources,
}

#[derive(Default)]
struct TickSums {
    ticks: u64,
    total: u64,
    max: u64,
    phases: [u64; PHASES],
}

#[derive(Default)]
struct Bucket {
    peers: HashMap<NodeId, PeerTraffic>,
    tick: TickSums,
    sizes: [u64; SIZE_CLASSES.len() + 1],
}

struct Window {
    /// When collecting began (or began again after a switch-on).
    since: Nanos,
    /// One bucket per second that saw anything, oldest first, with its second.
    buckets: VecDeque<(u64, Bucket)>,
}

/// What a snapshot of the window adds up to.
pub struct Totals {
    pub window_secs: f64,
    pub peers: HashMap<NodeId, PeerTraffic>,
    pub tick: Option<TickActivity>,
    pub chunk_sizes: Vec<SizeClass>,
}

pub struct Activity {
    clock: Arc<dyn Clock>,
    enabled: AtomicBool,
    window: Mutex<Window>,
}

impl Activity {
    pub fn new(clock: Arc<dyn Clock>, enabled: bool) -> Self {
        let since = clock.now();
        Self {
            clock,
            enabled: AtomicBool::new(enabled),
            window: Mutex::new(Window {
                since,
                buckets: VecDeque::new(),
            }),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Switch the counters on (starting a fresh window) or off (dropping it).
    pub fn set_enabled(&self, on: bool) {
        let mut w = self.window.lock().unwrap();
        if self.enabled.swap(on, Ordering::Relaxed) != on {
            w.since = self.clock.now();
            w.buckets.clear();
        }
    }

    fn with_bucket(&self, f: impl FnOnce(&mut Bucket)) {
        if !self.enabled() {
            return;
        }
        let now = self.clock.now() / NANOS_PER_SEC;
        let mut w = self.window.lock().unwrap();
        while w
            .buckets
            .front()
            .is_some_and(|(sec, _)| sec + WINDOW_SECS <= now)
        {
            w.buckets.pop_front();
        }
        if w.buckets.back().is_none_or(|(sec, _)| *sec != now) {
            w.buckets.push_back((now, Bucket::default()));
        }
        f(&mut w.buckets.back_mut().expect("just pushed").1);
    }

    fn peer(&self, peer: NodeId, f: impl FnOnce(&mut PeerTraffic)) {
        self.with_bucket(|b| f(b.peers.entry(peer).or_default()));
    }

    pub fn sent(&self, to: NodeId, category: Category, bytes: usize) {
        self.peer(to, |p| p.sent.of(category).add(bytes as u64));
    }

    pub fn received(&self, from: NodeId, category: Category, bytes: usize) {
        self.peer(from, |p| p.received.of(category).add(bytes as u64));
    }

    pub fn acknowledged(&self, peer: NodeId) {
        self.peer(peer, |p| p.acknowledged += 1);
    }

    pub fn timed_out(&self, peer: NodeId) {
        self.peer(peer, |p| p.timed_out += 1);
    }

    /// An ask went unanswered for the retry delay and is sent again.
    pub fn resent_after_timeout(&self, peer: NodeId) {
        self.peer(peer, |p| {
            p.timed_out += 1;
            p.retried += 1;
        });
    }

    /// A push carrying `bytes` of chunk data was sent (each resend counts).
    pub fn chunk_replicated(&self, bytes: usize) {
        let class = SIZE_CLASSES.partition_point(|up_to| *up_to < bytes as u64);
        self.with_bucket(|b| b.sizes[class] += 1);
    }

    /// Start timing a tick. Does nothing, not even reading the clock, while off.
    pub fn tick_timer(&self) -> TickTimer<'_> {
        let at = self.enabled().then(|| self.clock.now());
        TickTimer {
            activity: self,
            started: at,
            last: at.unwrap_or(0),
            phases: [0; PHASES],
        }
    }

    /// The window's totals as of now.
    pub fn totals(&self) -> Totals {
        let now = self.clock.now();
        let w = self.window.lock().unwrap();
        let window_secs =
            (now.saturating_sub(w.since) as f64 / NANOS_PER_SEC as f64).min(WINDOW_SECS as f64);
        let current = now / NANOS_PER_SEC;
        let mut peers: HashMap<NodeId, PeerTraffic> = HashMap::new();
        let mut ticks = TickSums::default();
        let mut sizes = [0u64; SIZE_CLASSES.len() + 1];
        for (_, b) in w
            .buckets
            .iter()
            .filter(|(sec, _)| sec + WINDOW_SECS > current)
        {
            for (peer, t) in &b.peers {
                peers.entry(*peer).or_default().merge(t);
            }
            ticks.ticks += b.tick.ticks;
            ticks.total += b.tick.total;
            ticks.max = ticks.max.max(b.tick.max);
            for (sum, add) in ticks.phases.iter_mut().zip(&b.tick.phases) {
                *sum += add;
            }
            for (sum, add) in sizes.iter_mut().zip(&b.sizes) {
                *sum += add;
            }
        }
        let ms = |nanos: u64| nanos as f64 / 1e6;
        let tick = (ticks.ticks > 0).then(|| {
            let per_tick = |i: usize| ms(ticks.phases[i]) / ticks.ticks as f64;
            TickActivity {
                ticks: ticks.ticks,
                avg_ms: ms(ticks.total) / ticks.ticks as f64,
                max_ms: ms(ticks.max),
                avg_phase_ms: PhaseAvg {
                    flush: per_tick(0),
                    sync: per_tick(1),
                    receive: per_tick(2),
                    gossip: per_tick(3),
                    replicate: per_tick(4),
                    other: per_tick(5),
                },
                busy_fraction: (ticks.total as f64 / (window_secs.max(1.0) * NANOS_PER_SEC as f64))
                    .min(1.0),
            }
        });
        let chunk_sizes = sizes
            .iter()
            .enumerate()
            .map(|(i, chunks)| SizeClass {
                up_to_bytes: SIZE_CLASSES.get(i).copied(),
                chunks: *chunks,
            })
            .collect();
        Totals {
            window_secs,
            peers,
            tick,
            chunk_sizes,
        }
    }
}

/// Times the phases of one tick. Inert while the counters are off.
pub struct TickTimer<'a> {
    activity: &'a Activity,
    started: Option<Nanos>,
    last: Nanos,
    phases: [u64; PHASES],
}

impl TickTimer<'_> {
    /// Charge the time since the last mark to `phase`.
    pub fn phase(&mut self, phase: Phase) {
        if self.started.is_none() {
            return;
        }
        let now = self.activity.clock.now();
        self.phases[phase as usize] += now.saturating_sub(self.last);
        self.last = now;
    }

    /// End the tick, charging what is left to `Other`.
    pub fn finish(mut self) {
        self.phase(Phase::Other);
        let Some(started) = self.started else {
            return;
        };
        let total = self.last.saturating_sub(started);
        self.activity.with_bucket(|b| {
            b.tick.ticks += 1;
            b.tick.total += total;
            b.tick.max = b.tick.max.max(total);
            for (sum, add) in b.tick.phases.iter_mut().zip(&self.phases) {
                *sum += add;
            }
        });
    }
}

/// Counts every message through the transport it wraps.
pub struct CountingTransport {
    inner: Arc<dyn Transport>,
    activity: Arc<Activity>,
}

impl CountingTransport {
    pub fn new(inner: Arc<dyn Transport>, activity: Arc<Activity>) -> Self {
        Self { inner, activity }
    }
}

impl Transport for CountingTransport {
    fn send(&self, to: NodeId, msg: Vec<u8>) {
        if self.activity.enabled() {
            self.activity.sent(to, Message::category(&msg), msg.len());
        }
        self.inner.send(to, msg);
    }

    fn recv(&self) -> Option<(NodeId, Vec<u8>)> {
        let (from, msg) = self.inner.recv()?;
        if self.activity.enabled() {
            self.activity
                .received(from, Message::category(&msg), msg.len());
        }
        Some((from, msg))
    }

    fn local(&self) -> NodeId {
        self.inner.local()
    }

    fn resolve(&self, addr: &str) -> Option<NodeId> {
        self.inner.resolve(addr)
    }

    fn learn(&self, node: NodeId, addr: &str) {
        self.inner.learn(node, addr);
    }

    fn last_error(&self, node: NodeId) -> Option<String> {
        self.inner.last_error(node)
    }

    fn last_heard(&self, node: NodeId) -> Option<Duration> {
        self.inner.last_heard(node)
    }

    fn rtt(&self, node: NodeId) -> Option<Duration> {
        self.inner.rtt(node)
    }

    fn inbox(&self) -> Option<crate::net::Inbox> {
        self.inner.inbox()
    }
}
