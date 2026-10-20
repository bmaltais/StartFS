use crate::network::{SimNetwork, SimTransport};
use crate::releases::{NodeRelease, Releases};
use startfs_core::clock::{Clock, ManualClock};
use startfs_core::config::Config;
use startfs_core::control::{self, Client};
use startfs_core::daemon::Daemon;
use startfs_core::fs::{LocalFs, NodeConfig};
use startfs_core::members::MemberId;
use startfs_core::net::NodeId;
use startfs_core::ops::*;
use startfs_core::power::ManualPower;
use startfs_core::protocol::PROTOCOL_VERSION;
use startfs_core::resources::{ManualResources, Resources};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tempfile::TempDir;

/// Handle to one node's filesystem. Operations fail with `NodeDown` while the
/// node is crashed; after a restart the same handle serves the recovered node.
#[derive(Clone)]
pub struct NodeFs {
    slot: Arc<Mutex<Option<Arc<LocalFs>>>>,
}

impl NodeFs {
    /// Whether the node holds commits that are not yet on disk.
    pub fn has_unsynced(&self) -> bool {
        self.fs().is_ok_and(|fs| fs.has_unsynced())
    }

    fn fs(&self) -> FsResult<Arc<LocalFs>> {
        self.slot.lock().unwrap().clone().ok_or(FsError::NodeDown)
    }
}

impl FsOps for NodeFs {
    fn lookup(&self, p: Ino, n: &str) -> FsResult<Attr> {
        self.fs()?.lookup(p, n)
    }
    fn getattr(&self, i: Ino) -> FsResult<Attr> {
        self.fs()?.getattr(i)
    }
    fn setattr(&self, i: Ino, s: SetAttr) -> FsResult<Attr> {
        self.fs()?.setattr(i, s)
    }
    fn mkdir(&self, p: Ino, n: &str, m: u32) -> FsResult<Attr> {
        self.fs()?.mkdir(p, n, m)
    }
    fn create(&self, p: Ino, n: &str, m: u32) -> FsResult<Attr> {
        self.fs()?.create(p, n, m)
    }
    fn symlink(&self, p: Ino, n: &str, t: &str) -> FsResult<Attr> {
        self.fs()?.symlink(p, n, t)
    }
    fn readlink(&self, i: Ino) -> FsResult<String> {
        self.fs()?.readlink(i)
    }
    fn link(&self, i: Ino, p: Ino, n: &str) -> FsResult<Attr> {
        self.fs()?.link(i, p, n)
    }
    fn read(&self, i: Ino, o: u64, l: u32) -> FsResult<Vec<u8>> {
        self.fs()?.read(i, o, l)
    }
    fn write(&self, i: Ino, o: u64, d: &[u8]) -> FsResult<usize> {
        self.fs()?.write(i, o, d)
    }
    fn flush(&self, i: Ino) -> FsResult<()> {
        self.fs()?.flush(i)
    }
    fn fsync(&self, i: Ino) -> FsResult<()> {
        self.fs()?.fsync(i)
    }
    fn unlink(&self, p: Ino, n: &str) -> FsResult<()> {
        self.fs()?.unlink(p, n)
    }
    fn rmdir(&self, p: Ino, n: &str) -> FsResult<()> {
        self.fs()?.rmdir(p, n)
    }
    fn rename(&self, p: Ino, n: &str, np: Ino, nn: &str) -> FsResult<()> {
        self.fs()?.rename(p, n, np, nn)
    }
    fn readdir(&self, i: Ino) -> FsResult<Vec<DirEntry>> {
        self.fs()?.readdir(i)
    }
    fn setxattr(&self, i: Ino, n: &str, v: &[u8]) -> FsResult<()> {
        self.fs()?.setxattr(i, n, v)
    }
    fn getxattr(&self, i: Ino, n: &str) -> FsResult<Vec<u8>> {
        self.fs()?.getxattr(i, n)
    }
    fn listxattr(&self, i: Ino) -> FsResult<Vec<String>> {
        self.fs()?.listxattr(i)
    }
    fn removexattr(&self, i: Ino, n: &str) -> FsResult<()> {
        self.fs()?.removexattr(i, n)
    }
    fn stats(&self) -> FsResult<StoreStats> {
        self.fs()?.stats()
    }
    fn space(&self) -> FsResult<Space> {
        self.fs()?.space()
    }
}

struct Node {
    dir: TempDir,
    config: NodeConfig,
    fs: NodeFs,
    /// Running control server; present while the node is up.
    server: Mutex<Option<control::Server>>,
    daemon: Mutex<Option<Arc<Daemon>>>,
    daemon_config: Config,
    power: Arc<ManualPower>,
    /// What the node's OS reports about it: nothing until a test says so.
    resources: Arc<ManualResources>,
    /// The host this node advertises from its next start; `None` is its tailnet IP.
    host: Mutex<Option<String>>,
    /// The release it runs and what it can upgrade to; kept across restarts, as the
    /// binary on disk is.
    release: Arc<NodeRelease>,
    /// Where an upgrade has taken the node.
    swap: Mutex<SwapState>,
}

/// How an upgrade is taking a node down and bringing it back.
#[derive(Clone, Copy)]
enum SwapState {
    /// Not being swapped.
    Running,
    /// The node asked for a swap; it goes down once its last messages have been
    /// delivered, this many rounds from now.
    Pending(u8),
    /// Down for the swap until the given time.
    Down(Nanos),
}

/// How long a node is down while it swaps in a release and restarts, unless a test says otherwise.
const RESTART_TAKES: Duration = Duration::from_secs(20);

/// One time an upgrade took a node down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restart {
    pub node: usize,
    /// When the node went down.
    pub from: Nanos,
    /// When it was back; `None` while it is still down.
    pub back: Option<Nanos>,
}

/// What the nodes and the work that drives them share.
#[derive(Clone)]
struct Env {
    network: SimNetwork,
    clock: Arc<ManualClock>,
    restarts: Arc<Mutex<Vec<Restart>>>,
    /// How long a node is down for a swap, in seconds.
    restart_takes: Arc<AtomicU64>,
}

/// N real nodes sharing one injectable clock and one simulated network.
pub struct Cluster {
    clock: Arc<ManualClock>,
    network: SimNetwork,
    nodes: Arc<Vec<Node>>,
    releases: Arc<Releases>,
    restarts: Arc<Mutex<Vec<Restart>>>,
    restart_takes: Arc<AtomicU64>,
}

/// 2023-11-14T22:13:20Z, an arbitrary fixed start so runs are deterministic.
const START: Nanos = 1_700_000_000 * 1_000_000_000;

impl Cluster {
    /// Start `n` nodes. Node `i` runs as local user `1000 + i`.
    pub fn start(n: usize) -> Self {
        Self::start_with_config(n, "")
    }

    /// `nodes` nodes (with `config_toml`) in one filesystem of `copies` copies,
    /// every node offering 10 GiB, synced. Node 0 keeps its default offer (also
    /// 10 GiB) instead of setting it, unlike [`Cluster::with_offers`].
    pub fn with_filesystem(copies: u32, nodes: usize, config_toml: &str) -> Self {
        const OFFER: u64 = 10 << 30;
        let c = Self::start_with_config(nodes, config_toml);
        let token = c.control(0).create("home", copies).unwrap();
        for i in 1..nodes {
            c.control(i).join(&token, Some(OFFER)).unwrap();
        }
        c.sync();
        c
    }

    /// One node per entry of `offers`, in one filesystem of `copies` copies, each
    /// node offering that many bytes (0 makes a pure client), synced.
    pub fn with_offers(copies: u32, offers: &[u64]) -> Self {
        let c = Self::start(offers.len());
        let token = c.control(0).create("home", copies).unwrap();
        c.control(0).set_offer(offers[0]).unwrap();
        for (i, offer) in offers.iter().enumerate().skip(1) {
            c.control(i).join(&token, Some(*offer)).unwrap();
        }
        c.sync();
        c
    }

    /// Like [`Cluster::start`], with every node's `config.toml` content given.
    pub fn start_with_config(n: usize, config_toml: &str) -> Self {
        let mut config_for_nodes = Config::parse(config_toml).expect("valid config");
        // Tests that write and then let the cluster settle expect the write to
        // be committed by the next tick, unless they ask for a delay.
        if !config_toml.contains("sync_quiet_ms") {
            config_for_nodes.sync_quiet_ms = 0;
        }
        if !config_toml.contains("write_commit_delay_ms") {
            config_for_nodes.write_commit_delay_ms = 0;
        }
        let clock = Arc::new(ManualClock::new(START));
        let network = SimNetwork::new(clock.clone());
        let releases = Arc::new(Releases::default());
        let nodes: Vec<Node> = (0..n)
            .map(|i| Node {
                dir: TempDir::new().expect("tempdir"),
                // Every node starts as member 0, as the CLI does; `join` assigns real ids.
                config: NodeConfig {
                    node: MemberId(0),
                    uid: 1000 + i as u32,
                    gid: 1000 + i as u32,
                },
                fs: NodeFs {
                    slot: Arc::new(Mutex::new(None)),
                },
                server: Mutex::new(None),
                daemon: Mutex::new(None),
                daemon_config: config_for_nodes.clone(),
                power: Arc::new(ManualPower::default()),
                resources: Arc::new(ManualResources::default()),
                host: Mutex::new(None),
                release: Arc::new(NodeRelease::new(releases.clone())),
                swap: Mutex::new(SwapState::Running),
            })
            .collect();
        let nodes = Arc::new(nodes);
        let restarts = Arc::new(Mutex::new(Vec::new()));
        let cluster = Self {
            clock,
            network,
            nodes,
            releases,
            restarts,
            restart_takes: Arc::new(AtomicU64::new(RESTART_TAKES.as_secs())),
        };
        for i in 0..n {
            cluster
                .network
                .register(&address(i, &config_for_nodes), NodeId(i as u32));
            open(&cluster.nodes, i, &cluster.env());
        }
        cluster
    }

    fn env(&self) -> Env {
        Env {
            network: self.network.clone(),
            clock: self.clock.clone(),
            restarts: self.restarts.clone(),
            restart_takes: self.restart_takes.clone(),
        }
    }

    /// Let the live nodes exchange messages until the network goes quiet.
    /// Time does not pass; advance the clock to trigger retries after a heal.
    pub fn sync(&self) {
        for _ in 0..200 {
            if !pump(&self.nodes, &self.env()) {
                return;
            }
        }
        panic!("cluster did not settle");
    }

    /// One round of background work on every live node, nothing more.
    pub fn tick(&self) -> bool {
        pump(&self.nodes, &self.env())
    }

    /// Like [`Cluster::tick`], but node `stuck` does no background work: its
    /// tick is stuck behind slow disk or a long queue.
    pub fn tick_except(&self, stuck: usize) -> bool {
        let mut busy = false;
        for (i, n) in self.nodes.iter().enumerate() {
            let daemon = n.daemon.lock().unwrap().clone();
            if let Some(d) = daemon.filter(|_| i != stuck) {
                busy |= d.tick();
            }
        }
        busy
    }

    /// Run only the liveness work of node `i`, as its own thread does while
    /// the tick is stuck.
    pub fn keepalive(&self, i: usize) {
        let daemon = self.nodes[i].daemon.lock().unwrap().clone();
        if let Some(d) = daemon {
            let _hello_sent = d.keepalive();
        }
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn fs(&self, i: usize) -> NodeFs {
        self.nodes[i].fs.clone()
    }

    /// The control interface of node `i`, over its local socket (as the CLI uses it).
    pub fn control(&self, i: usize) -> Client {
        Client::new(socket_path(&self.nodes[i]))
    }

    pub fn network(&self) -> &SimNetwork {
        &self.network
    }

    pub fn transport(&self, i: usize) -> SimTransport {
        self.network.transport(NodeId(i as u32))
    }

    /// Publish a release the nodes can upgrade to, speaking the current protocol version.
    pub fn publish(&self, tag: &str) {
        self.releases.publish(tag, PROTOCOL_VERSION, false);
    }

    /// Publish a release that speaks another protocol version.
    pub fn publish_with_protocol(&self, tag: &str, protocol: u8) {
        self.releases.publish(tag, protocol, false);
    }

    /// Publish a pre-release: never the latest stable one.
    pub fn publish_prerelease(&self, tag: &str) {
        self.releases.publish(tag, PROTOCOL_VERSION, true);
    }

    /// Make every node run a release that speaks `protocol`, as the one before an
    /// upgrade across a protocol change does.
    pub fn set_protocol(&self, protocol: u8) {
        for n in self.nodes.iter() {
            n.release.set_protocol(protocol);
        }
    }

    /// Make every node run the release `version` (without the leading `v`).
    pub fn set_version(&self, version: &str) {
        for n in self.nodes.iter() {
            n.release.set_version(version);
        }
    }

    /// The release node `i` runs, as a tag.
    pub fn release_of(&self, i: usize) -> String {
        self.nodes[i].release.tag()
    }

    /// The release node `i` has staged and not yet swapped in.
    pub fn staged_on(&self, i: usize) -> Option<String> {
        self.nodes[i].release.staged_tag()
    }

    /// Say whether processes are using node `i`'s mount, which a restart would take from them.
    pub fn set_mount_in_use(&self, i: usize, in_use: bool) {
        self.nodes[i].release.set_mount_in_use(in_use);
    }

    /// Node `i` cannot fetch the release `tag`, for `why`.
    pub fn break_release(&self, i: usize, tag: &str, why: &str) {
        self.nodes[i].release.break_release(tag, why);
    }

    /// The next swap of node `i` fails to bring up the new daemon: the installer
    /// restores the old release and the node comes back on it.
    pub fn fail_next_swap(&self, i: usize) {
        self.nodes[i].release.fail_next_swap();
    }

    /// How long a node is down while an upgrade swaps in its release and restarts it.
    pub fn set_restart_takes(&self, by: Duration) {
        self.restart_takes.store(by.as_secs(), Ordering::SeqCst);
    }

    /// Every time an upgrade took a node down, in order.
    pub fn restarts(&self) -> Vec<Restart> {
        self.restarts.lock().unwrap().clone()
    }

    pub fn now(&self) -> Nanos {
        self.clock.now()
    }

    /// Move the clock in one jump. After a jump of minutes every node sees every
    /// peer as unseen until the hellos arrive, as a machine does after it
    /// suspended itself; use [`Cluster::elapse`] to let time pass the way it does
    /// on running machines.
    pub fn advance(&self, by: Duration) {
        self.clock.advance(by);
    }

    /// Let `by` pass with every live node running: steps of 4 minutes (short
    /// of the 5 minutes after which a member is taken for asleep), each followed
    /// by a sync, then a few more rounds so work started at the end completes.
    pub fn elapse(&self, by: Duration) {
        const STEP: Duration = Duration::from_secs(240);
        for _ in 0..by.as_secs() / STEP.as_secs() {
            self.advance(STEP);
            self.sync();
        }
        for _ in 0..3 {
            self.advance(Duration::from_secs(10));
            self.sync();
        }
    }

    /// What the node's operating system reports about its resources.
    pub fn set_resources(&self, i: usize, resources: Resources) {
        self.nodes[i].resources.set(resources);
    }

    /// Plug the node's machine in or unplug it.
    pub fn set_on_battery(&self, i: usize, on: bool) {
        self.nodes[i].power.set_on_battery(on);
    }

    /// Kill the node process: in-memory state is lost, disk state survives.
    pub fn crash(&self, i: usize) {
        crash_node(&self.nodes, i, &self.env());
    }

    /// Faults hit what is on disk, so commit the node's buffered writes first.
    fn commit_writes(&self, i: usize) {
        if let Ok(fs) = self.nodes[i].fs.fs() {
            fs.flush_all(true).expect("commit buffered writes");
            fs.sync().expect("sync");
        }
    }

    /// Fault injection: flip a payload byte in every replica chunk file on the node's disk.
    pub fn corrupt_chunks(&self, i: usize) {
        self.commit_writes(i);
        corrupt_files(&self.nodes[i].dir.path().join("chunks"));
    }

    /// Fault injection: the same for the node's cache of fetched chunks.
    pub fn corrupt_cache(&self, i: usize) {
        self.commit_writes(i);
        corrupt_files(&self.nodes[i].dir.path().join("cache"));
    }

    /// Fault injection: the power goes. Chunk files that were written without a
    /// sync, which the stores flag with a marker, are cut short as a disk
    /// might leave them. A crash here syncs on the way down, as a clean
    /// shutdown does, so the marker is put back afterwards.
    pub fn power_cut(&self, i: usize) {
        let dir = self.nodes[i].dir.path();
        let marker = |store: &str| dir.join(format!("{store}.dirty"));
        let torn: Vec<&str> = ["chunks", "staging"]
            .into_iter()
            .filter(|store| marker(store).exists())
            .collect();
        self.crash(i);
        for store in torn {
            truncate_files(&dir.join(store));
            std::fs::write(marker(store), b"").unwrap();
        }
    }

    /// The machine dies and is replaced by a blank one at the same address.
    pub fn rebuild(&self, i: usize) {
        self.crash(i);
        for e in std::fs::read_dir(self.nodes[i].dir.path())
            .unwrap()
            .flatten()
        {
            let p = e.path();
            if p.is_dir() {
                std::fs::remove_dir_all(&p).unwrap();
            } else {
                std::fs::remove_file(&p).unwrap();
            }
        }
        self.restart(i);
    }

    /// Make node `i` advertise `host` (a name, as MagicDNS gives) from its next
    /// start, and reachable at it.
    pub fn advertise_as(&self, i: usize, host: &str) {
        let port = self.nodes[i].daemon_config.port;
        self.network
            .register(&format!("{host}:{port}"), NodeId(i as u32));
        *self.nodes[i].host.lock().unwrap() = Some(host.to_string());
    }

    /// Start the node again from its on-disk state.
    pub fn restart(&self, i: usize) {
        open(&self.nodes, i, &self.env());
        self.network.set_down(NodeId(i as u32), false);
    }
}

fn truncate_files(dir: &std::path::Path) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            truncate_files(&p);
        } else {
            let len = std::fs::metadata(&p).unwrap().len();
            std::fs::OpenOptions::new()
                .write(true)
                .open(&p)
                .unwrap()
                .set_len(len / 2)
                .unwrap();
        }
    }
}

fn corrupt_files(dir: &std::path::Path) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            corrupt_files(&p);
        } else {
            let mut b = std::fs::read(&p).unwrap();
            if let Some(last) = b.last_mut() {
                *last ^= 0xFF;
            }
            std::fs::write(&p, b).unwrap();
        }
    }
}

fn socket_path(node: &Node) -> std::path::PathBuf {
    node.dir.path().join("control.sock")
}

fn address(index: usize, config: &Config) -> String {
    format!("100.64.0.{}:{}", index + 1, config.port)
}

/// The node's process dies: in-memory state is lost, disk state survives.
fn crash_node(nodes: &[Node], i: usize, env: &Env) {
    nodes[i].server.lock().unwrap().take();
    nodes[i].daemon.lock().unwrap().take();
    nodes[i].fs.slot.lock().unwrap().take();
    env.network.set_down(NodeId(i as u32), true);
}

/// One round of background work on every live node; whether anything happened.
/// An upgrade that was handed over takes its node down once the node's last messages
/// have had a round to arrive, and brings it back on the new release after a while.
fn pump(nodes: &Arc<Vec<Node>>, env: &Env) -> bool {
    let mut busy = false;
    let now = env.clock.now();
    for (i, n) in nodes.iter().enumerate() {
        let back = matches!(*n.swap.lock().unwrap(), SwapState::Down(at) if now >= at);
        if back {
            n.release.finish_swap();
            open(nodes, i, env);
            env.network.set_down(NodeId(i as u32), false);
            if let Some(r) = env
                .restarts
                .lock()
                .unwrap()
                .iter_mut()
                .rev()
                .find(|r| r.node == i && r.back.is_none())
            {
                r.back = Some(now);
            }
            *n.swap.lock().unwrap() = SwapState::Running;
            busy = true;
        }
    }
    for n in nodes.iter() {
        let daemon = n.daemon.lock().unwrap().clone();
        if let Some(d) = daemon {
            busy |= d.tick();
        }
    }
    for (i, n) in nodes.iter().enumerate() {
        let state = *n.swap.lock().unwrap();
        match state {
            SwapState::Running if n.release.swap_requested() => {
                *n.swap.lock().unwrap() = SwapState::Pending(2);
                busy = true;
            }
            SwapState::Pending(0) => {
                crash_node(nodes, i, env);
                env.restarts.lock().unwrap().push(Restart {
                    node: i,
                    from: now,
                    back: None,
                });
                let takes = Duration::from_secs(env.restart_takes.load(Ordering::SeqCst));
                *n.swap.lock().unwrap() = SwapState::Down(now + takes.as_nanos() as u64);
                busy = true;
            }
            SwapState::Pending(rounds) => {
                *n.swap.lock().unwrap() = SwapState::Pending(rounds - 1);
                busy = true;
            }
            SwapState::Running | SwapState::Down(_) => {}
        }
    }
    busy
}

/// Start node `index` from its on-disk state and connect it to the network.
fn open(nodes: &Arc<Vec<Node>>, index: usize, env: &Env) {
    let node = &nodes[index];
    let clock = env.clock.clone();
    let network = &env.network;
    let host = node
        .host
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| format!("100.64.0.{}", index + 1));
    let daemon = Daemon::open(
        node.dir.path(),
        node.daemon_config.clone(),
        node.config.clone(),
        clock.clone(),
        host,
    )
    .expect("open node");
    daemon.set_power_source(node.power.clone());
    daemon.set_resource_source(node.resources.clone());
    daemon.set_installer(node.release.clone());
    daemon.attach(Arc::new(
        network.transport_speaking(NodeId(index as u32), node.release.wire()),
    ));
    // A request that waits for a peer lets the whole simulated cluster run, and
    // time pass once it has nothing left to do, so that delayed messages arrive.
    let cluster: Weak<Vec<Node>> = Arc::downgrade(nodes);
    let env = env.clone();
    daemon.set_wait_hook(Arc::new(move || {
        if let Some(nodes) = cluster.upgrade()
            && !pump(&nodes, &env)
        {
            env.clock.advance(Duration::from_millis(1));
        }
    }));
    *node.fs.slot.lock().unwrap() = Some(daemon.fs());
    *node.daemon.lock().unwrap() = Some(daemon.clone());
    let server = control::serve(daemon, &socket_path(node)).expect("serve control socket");
    *node.server.lock().unwrap() = Some(server);
}
