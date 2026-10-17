//! Per-node operation logs stamped with hybrid logical clocks, and the small
//! wire protocol peers use to exchange them.

use crate::ops::{FileKind, Ino, Nanos};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Hybrid logical clock timestamp. Ordered by wall time, then counter, then
/// node, so every operation in the filesystem has a unique place in one total order.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct Hlc {
    pub wall: Nanos,
    pub counter: u32,
    pub node: u32,
}

impl Hlc {
    pub const ZERO: Hlc = Hlc {
        wall: 0,
        counter: 0,
        node: 0,
    };

    /// Stamp for a local event at physical time `now`, after `last`.
    pub fn tick(last: Hlc, now: Nanos, node: u32) -> Hlc {
        if now > last.wall {
            Hlc {
                wall: now,
                counter: 0,
                node,
            }
        } else {
            Hlc {
                wall: last.wall,
                counter: last.counter + 1,
                node,
            }
        }
    }

    /// The local clock after seeing a remote stamp, so later local events sort after it.
    pub fn observe(last: Hlc, seen: Hlc, now: Nanos) -> Hlc {
        let wall = now.max(last.wall).max(seen.wall);
        let counter = if wall == last.wall && wall == seen.wall {
            last.counter.max(seen.counter)
        } else if wall == last.wall {
            last.counter
        } else if wall == seen.wall {
            seen.counter
        } else {
            return Hlc {
                wall,
                counter: 0,
                node: last.node,
            };
        };
        Hlc {
            wall,
            counter,
            node: last.node,
        }
    }
}

/// One state of an object: everything a write, chmod or xattr change can alter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub mode: u32,
    pub mtime: Nanos,
    pub xattrs: BTreeMap<String, Vec<u8>>,
    /// File bytes or symlink target; empty for directories. Carried inline
    /// until chunk replication exists.
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op {
    Create {
        ino: Ino,
        kind: FileKind,
        parent: Ino,
        name: String,
        snap: Snapshot,
    },
    /// A new state of an existing object, derived from the state stamped `base`.
    Set {
        ino: Ino,
        base: Option<Hlc>,
        snap: Snapshot,
    },
    Rename {
        ino: Ino,
        parent: Ino,
        name: String,
    },
    Delete {
        ino: Ino,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub origin: u32,
    pub seq: u64,
    pub hlc: Hlc,
    pub op: Op,
}

/// How much of each node's log a peer holds: next expected seq per origin.
pub type Frontier = BTreeMap<u32, u64>;

/// The whole wire protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Message {
    /// "This is what I have; send me what I lack."
    Hello(Frontier),
    /// Log entries, in per-origin order.
    Entries(Vec<Entry>),
}

impl Message {
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("message serialises")
    }

    pub fn decode(raw: &[u8]) -> Option<Message> {
        postcard::from_bytes(raw).ok()
    }
}
