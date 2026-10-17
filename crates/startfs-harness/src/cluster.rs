use crate::network::{SimNetwork, SimTransport};
use startfs_core::clock::{Clock, ManualClock};
use startfs_core::config::Config;
use startfs_core::control::{self, Client};
use startfs_core::daemon::Daemon;
use startfs_core::fs::{LocalFs, NodeConfig};
use startfs_core::net::NodeId;
use startfs_core::ops::*;
use startfs_core::power::ManualPower;
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
}

/// N real nodes sharing one injectable clock and one simulated network.
pub struct Cluster {
    clock: Arc<ManualClock>,
    network: SimNetwork,
    nodes: Arc<Vec<Node>>,
}

/// 2023-11-14T22:13:20Z, an arbitrary fixed start so runs are deterministic.
const START: Nanos = 1_700_000_000 * 1_000_000_000;

impl Cluster {
    /// Start `n` nodes. Node `i` runs as local user `1000 + i`.
    pub fn start(n: usize) -> Self {
        Self::start_with_config(n, "")
    }

    /// Like [`Cluster::start`], with every node's `config.toml` content given.
    pub fn start_with_config(n: usize, config_toml: &str) -> Self {
        let mut config_for_nodes = Config::parse(config_toml).expect("valid config");
        // Tests that write and then let the cluster settle expect the write to
        // be committed by the next tick, unless they ask for a delay.
        if !config_toml.contains("write_commit_delay_ms") {
            config_for_nodes.write_commit_delay_ms = 0;
        }
        let clock = Arc::new(ManualClock::new(START));
        let network = SimNetwork::new(clock.clone());
        let nodes: Vec<Node> = (0..n)
            .map(|i| Node {
                dir: TempDir::new().expect("tempdir"),
                // Every node starts as member 0, as the CLI does; `join` assigns real ids.
                config: NodeConfig {
                    node: NodeId(0),
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
            })
            .collect();
        let nodes = Arc::new(nodes);
        for i in 0..n {
            network.register(&address(i, &config_for_nodes), NodeId(i as u32));
            open(&nodes, i, &network, clock.clone());
        }
        Self {
            clock,
            network,
            nodes,
        }
    }

    /// Let the live nodes exchange messages until the network goes quiet.
    /// Time does not pass; advance the clock to trigger retries after a heal.
    pub fn sync(&self) {
        for _ in 0..200 {
            if !pump(&self.nodes) {
                return;
            }
        }
        panic!("cluster did not settle");
    }

    /// One round of background work on every live node, nothing more.
    pub fn tick(&self) -> bool {
        pump(&self.nodes)
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

    pub fn now(&self) -> Nanos {
        self.clock.now()
    }

    pub fn advance(&self, by: Duration) {
        self.clock.advance(by);
    }

    /// Plug the node's machine in or unplug it.
    pub fn set_on_battery(&self, i: usize, on: bool) {
        self.nodes[i].power.set_on_battery(on);
    }

    /// Kill the node process: in-memory state is lost, disk state survives.
    pub fn crash(&self, i: usize) {
        self.nodes[i].server.lock().unwrap().take();
        self.nodes[i].daemon.lock().unwrap().take();
        self.nodes[i].fs.slot.lock().unwrap().take();
        self.network.set_down(NodeId(i as u32), true);
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

    /// Start the node again from its on-disk state.
    pub fn restart(&self, i: usize) {
        open(&self.nodes, i, &self.network, self.clock.clone());
        self.network.set_down(NodeId(i as u32), false);
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

/// One round of background work on every live node; whether anything happened.
fn pump(nodes: &[Node]) -> bool {
    let mut busy = false;
    for n in nodes {
        let daemon = n.daemon.lock().unwrap().clone();
        if let Some(d) = daemon {
            busy |= d.tick();
        }
    }
    busy
}

/// Start node `index` from its on-disk state and connect it to the network.
fn open(nodes: &Arc<Vec<Node>>, index: usize, network: &SimNetwork, clock: Arc<ManualClock>) {
    let node = &nodes[index];
    let host = format!("100.64.0.{}", index + 1);
    let daemon = Daemon::open(
        node.dir.path(),
        node.daemon_config.clone(),
        node.config.clone(),
        clock,
        host,
    )
    .expect("open node");
    daemon.set_power_source(node.power.clone());
    daemon.attach(Arc::new(network.transport(NodeId(index as u32))));
    // A request that waits for a peer lets the whole simulated cluster run.
    let cluster: Weak<Vec<Node>> = Arc::downgrade(nodes);
    daemon.set_wait_hook(Arc::new(move || {
        if let Some(nodes) = cluster.upgrade() {
            pump(&nodes);
        }
    }));
    *node.fs.slot.lock().unwrap() = Some(daemon.fs());
    *node.daemon.lock().unwrap() = Some(daemon.clone());
    let server = control::serve(daemon, &socket_path(node)).expect("serve control socket");
    *node.server.lock().unwrap() = Some(server);
}
