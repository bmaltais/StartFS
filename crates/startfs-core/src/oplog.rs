//! Per-node operation logs stamped with hybrid logical clocks, and the small
//! wire protocol peers use to exchange them.

use crate::fs::Content;
use crate::members::MemberId;
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
    pub node: MemberId,
}

impl Hlc {
    pub const ZERO: Hlc = Hlc {
        wall: 0,
        counter: 0,
        node: MemberId(0),
    };

    /// Stamp for a local event at physical time `now`, after `last`.
    pub fn tick(last: Hlc, now: Nanos, node: MemberId) -> Hlc {
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
    /// Tiny files and symlink targets inline; otherwise chunk references.
    /// Empty for directories.
    pub content: Content,
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
    pub origin: MemberId,
    pub seq: u64,
    pub hlc: Hlc,
    pub op: Op,
}

/// Where a state transfer stands when it starts: the receiver carries on from
/// `frontier` once it has every chunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatePin {
    /// What the sender's log held when the transfer started.
    pub frontier: Frontier,
    /// The sender's clock then, so the receiver's later operations sort after it.
    pub hlc: Hlc,
}

/// One bounded piece of a state transfer, for a peer that missed log entries
/// which have since been compacted away. Its objects are joined into the
/// receiver's own state as they arrive; the receiver moves on to `pin` only
/// with the last chunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateChunk {
    pub pin: StatePin,
    /// Merge registers by object (encoded), in object order.
    pub objects: Vec<(Ino, Vec<u8>)>,
    /// Whether no objects follow in this transfer.
    pub last: bool,
}

/// How much of each node's log a peer holds: next expected seq per origin.
pub type Frontier = BTreeMap<MemberId, u64>;
