use crate::network::{SimNetwork, SimTransport};
use startfs_core::clock::{Clock, ManualClock};
use startfs_core::fs::{LocalFs, NodeConfig};
use startfs_core::net::NodeId;
use startfs_core::ops::*;
use std::sync::{Arc, Mutex};
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
}

/// N real nodes sharing one injectable clock and one simulated network.
pub struct Cluster {
    clock: Arc<ManualClock>,
    network: SimNetwork,
    nodes: Vec<Node>,
}

/// 2023-11-14T22:13:20Z, an arbitrary fixed start so runs are deterministic.
const START: Nanos = 1_700_000_000 * 1_000_000_000;

impl Cluster {
    /// Start `n` nodes. Node `i` runs as local user `1000 + i`.
    pub fn start(n: usize) -> Self {
        let clock = Arc::new(ManualClock::new(START));
        let network = SimNetwork::new(clock.clone());
        let nodes = (0..n)
            .map(|i| {
                let dir = TempDir::new().expect("tempdir");
                let config = NodeConfig {
                    uid: 1000 + i as u32,
                    gid: 1000 + i as u32,
                };
                let fs = NodeFs {
                    slot: Arc::new(Mutex::new(None)),
                };
                let node = Node { dir, config, fs };
                open(&node, clock.clone());
                node
            })
            .collect();
        Self {
            clock,
            network,
            nodes,
        }
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn fs(&self, i: usize) -> NodeFs {
        self.nodes[i].fs.clone()
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

    /// Kill the node process: in-memory state is lost, disk state survives.
    pub fn crash(&self, i: usize) {
        self.nodes[i].fs.slot.lock().unwrap().take();
        self.network.set_down(NodeId(i as u32), true);
    }

    /// Fault injection: flip a payload byte in every chunk file on the node's disk.
    pub fn corrupt_chunks(&self, i: usize) {
        fn walk(dir: &std::path::Path) {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p);
                } else {
                    let mut b = std::fs::read(&p).unwrap();
                    if let Some(last) = b.last_mut() {
                        *last ^= 0xFF;
                    }
                    std::fs::write(&p, b).unwrap();
                }
            }
        }
        walk(&self.nodes[i].dir.path().join("chunks"));
    }

    /// Start the node again from its on-disk state.
    pub fn restart(&self, i: usize) {
        open(&self.nodes[i], self.clock.clone());
        self.network.set_down(NodeId(i as u32), false);
    }
}

fn open(node: &Node, clock: Arc<ManualClock>) {
    let fs = LocalFs::open(node.dir.path(), node.config.clone(), clock).expect("open node");
    *node.fs.slot.lock().unwrap() = Some(Arc::new(fs));
}
