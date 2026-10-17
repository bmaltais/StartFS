//! The wire protocol peers speak over a [`Transport`](crate::net::Transport).

use crate::chunkstore::ChunkId;
use crate::members::{Admission, Member};
use crate::oplog::{Entry, Frontier};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Message {
    /// "This is what I have; send me what I lack."
    Hello(Frontier),
    /// Log entries, in per-origin order.
    Entries(Vec<Entry>),
    /// A member's view of who belongs to the filesystem.
    Members(Vec<Member>),
    /// A new node asks the member named in its token to admit it.
    JoinRequest {
        fs_id: String,
        token_id: String,
        /// The joiner's advertised `host:port`.
        addr: String,
        offer_bytes: u64,
    },
    JoinReply(Result<Admission, String>),
    /// "Send me this chunk if you hold it."
    ChunkRequest(ChunkId),
    /// The chunk's bytes, or `None` when the sender does not hold it.
    ChunkReply(ChunkId, Option<Vec<u8>>),
    /// Ask the receiver to hold a replica of a chunk. Without the bytes it is
    /// a question: "do you hold it, and would you take it?"
    ChunkPush(ChunkId, Option<Vec<u8>>),
    /// The receiver's answer to a [`Message::ChunkPush`].
    ChunkAck(ChunkId, Replica),
}

/// A node's standing towards holding a replica of one chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Replica {
    /// It holds the chunk.
    Stored,
    /// It does not, and would take it.
    Missing,
    /// It does not, and has no room (or offers no space).
    Full,
}

impl Message {
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("message serialises")
    }

    pub fn decode(raw: &[u8]) -> Option<Message> {
        postcard::from_bytes(raw).ok()
    }
}
