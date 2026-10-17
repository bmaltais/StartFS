//! The wire protocol peers speak over a [`Transport`](crate::net::Transport).

use crate::chunkstore::ChunkId;
use crate::control::{FileHealth, Repair};
use crate::members::{Admission, JoinRequest, Member, TokenRec};
use crate::oplog::{Entry, Frontier, StateChunk};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Message {
    /// "This is what I have; send me what I lack."
    Hello(Frontier),
    /// Log entries, in per-origin order.
    Entries(Vec<Entry>),
    /// One piece of the sender's merge state, for a peer whose missing log
    /// entries were compacted.
    StateChunk(StateChunk),
    /// A member's view of who belongs to the filesystem.
    Members(Vec<Member>),
    /// A new node asks the member named in its token to admit it.
    JoinRequest(JoinRequest),
    /// The admission, or why the admitting member refused. Deliberate exception
    /// to typed errors: the refusal is plain text, since the wire format has no
    /// error codes and changing it would break the protocol.
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
    /// A member's view of the join tokens: any member can admit with any of them.
    Tokens(Vec<TokenRec>),
    /// How well replicated the sender's own chunks are, so any member's
    /// status can speak for the whole cluster.
    Health(Health),
}

/// One member's view of its replication, as gossiped to the others.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Health {
    pub unreplicated: Vec<FileHealth>,
    pub repair: Option<Repair>,
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

/// Version of the wire protocol. Bumped whenever messages change shape.
pub const PROTOCOL_VERSION: u8 = 4;

/// Why a received message could not be understood.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error(
        "protocol version mismatch: the peer speaks version {theirs}, this node speaks version {ours}"
    )]
    VersionMismatch { ours: u8, theirs: u8 },
    #[error("malformed message")]
    Malformed,
}

impl Message {
    /// The wire form: a version byte, then the message.
    pub fn encode(&self) -> Vec<u8> {
        let mut raw = vec![PROTOCOL_VERSION];
        postcard::to_io(self, &mut raw).expect("message serialises");
        raw
    }

    pub fn decode(raw: &[u8]) -> Result<Message, DecodeError> {
        let (&theirs, body) = raw.split_first().ok_or(DecodeError::Malformed)?;
        if theirs != PROTOCOL_VERSION {
            return Err(DecodeError::VersionMismatch {
                ours: PROTOCOL_VERSION,
                theirs,
            });
        }
        postcard::from_bytes(body).map_err(|_| DecodeError::Malformed)
    }

    /// Whether an encoded message carries chunk bytes. Transports send these
    /// on streams of their own so they never hold up the metadata exchange.
    pub fn is_bulk(raw: &[u8]) -> bool {
        // Postcard writes the variant index first; the chunk-carrying
        // variants are `ChunkReply` and `ChunkPush`.
        matches!(raw.get(1), Some(7 | 8))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oplog::Frontier;

    #[test]
    fn a_message_survives_the_wire() {
        let msg = Message::ChunkRequest([7u8; 32]);
        assert_eq!(Message::decode(&msg.encode()), Ok(msg));
    }

    #[test]
    fn a_message_from_another_protocol_version_fails_with_a_clear_error() {
        let mut raw = Message::Hello(Frontier::default()).encode();
        raw[0] = PROTOCOL_VERSION + 1;
        let err = Message::decode(&raw).unwrap_err();
        assert_eq!(
            err,
            DecodeError::VersionMismatch {
                ours: PROTOCOL_VERSION,
                theirs: PROTOCOL_VERSION + 1
            }
        );
        let text = err.to_string();
        assert!(
            text.contains("version") && text.contains("mismatch"),
            "{text}"
        );
    }

    #[test]
    fn only_messages_carrying_chunk_bytes_are_bulk() {
        let id = [1u8; 32];
        let bulk = [
            Message::ChunkReply(id, Some(vec![1, 2, 3])),
            Message::ChunkReply(id, None),
            Message::ChunkPush(id, Some(vec![1])),
        ];
        let small = [
            Message::Hello(Frontier::default()),
            Message::Entries(vec![]),
            Message::Members(vec![]),
            Message::Tokens(vec![]),
            Message::Health(Health::default()),
            Message::ChunkRequest(id),
            Message::ChunkAck(id, Replica::Stored),
        ];
        assert!(bulk.iter().all(|m| Message::is_bulk(&m.encode())));
        assert!(small.iter().all(|m| !Message::is_bulk(&m.encode())));
    }
}
