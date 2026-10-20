//! The wire protocol peers speak over a [`Transport`](crate::net::Transport).

use crate::chunkstore::ChunkId;
use crate::control::{NodeStats, Repair, Shortfall, Stranded};
use crate::load::Load;
use crate::members::{Admission, JoinRequest, Member, MemberId, TokenRec};
use crate::oplog::{Entry, Frontier, StateChunk};
use crate::upgrade::UpgradeGossip;
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
    /// The filesystem's number of copies and the revision of that setting;
    /// the higher revision wins.
    Copies { copies: u32, rev: u64 },
    /// "Tell me your storage figures now."
    StatsRequest,
    /// The sender's own storage figures, in answer to a [`Message::StatsRequest`].
    StatsReply(NodeStats),
    /// "Reclaim the chunks no file uses now, without waiting out the retention
    /// period." The reclaim gate still applies.
    Purge,
    /// The release the sender runs and, while an upgrade is under way, what it is
    /// moving to and how far the sender has got (ADR 0007).
    Upgrade(UpgradeGossip),
    /// Which release the cluster is on, in a form every release reads whatever its
    /// protocol version (ADR 0006). It never goes through the enum's own encoding:
    /// see [`NOTICE_BYTE`].
    UpgradeNotice(UpgradeNotice),
}

/// Which release the cluster is on, sent to a member that cannot otherwise hear
/// the others because its protocol version differs. The form is frozen: a change
/// to it breaks every deployed daemon (ADR 0006), so it names a release and a
/// protocol number and nothing else, and never carries a binary or a command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpgradeNotice {
    /// The release tag, as `v1.10.0`.
    pub release: String,
    /// That release's protocol version.
    pub protocol: u8,
}

/// The first byte of an [`UpgradeNotice`], in the place of a protocol version. No
/// protocol version will be this value, so `decode` routes it to the notice instead
/// of refusing it as a mismatch.
pub const NOTICE_BYTE: u8 = 0xFF;

/// The kinds of traffic the activity view tells apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Category {
    /// Chunk requests, replies, pushes and their acknowledgements.
    Chunk,
    /// Operation log entries and state transfer.
    Metadata,
    /// Hello, members, tokens, health, the copies setting and upgrades.
    Gossip,
    /// Joining and storage-figure queries.
    Other,
}

/// One member's view of its replication, as gossiped to the others.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Health {
    pub unreplicated: Vec<Shortfall>,
    pub repair: Option<Repair>,
    /// When the sender last heard from each other member, so the cluster
    /// remembers a member's last sighting even when one node forgets it.
    pub seen: Vec<Sighting>,
    /// For each member the sender does not see, the single-copy chunks its
    /// last report listed (none included), as the sender or its peers heard
    /// them, so the cluster remembers what a sleeping member alone may hold.
    pub reports: Vec<Stranded>,
    /// For each member the sender saw asleep within the stand-in retention,
    /// how long ago (rounded up), so the others keep the extra copies made for
    /// it for as long as the sender would.
    pub slept: Vec<Sighting>,
    /// How far behind the sender's tick and inbox are, so the others can say
    /// it is behind (and not down) and hold off sending it more.
    pub load: Load,
}

/// That the sender observed a member `secs_ago` seconds ago: heard from it, or
/// (in `Health.slept`) saw it asleep. A duration, not
/// a timestamp, since members' clocks do not agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sighting {
    pub member: MemberId,
    pub secs_ago: u64,
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
pub const PROTOCOL_VERSION: u8 = 15;

/// Which side of a version mismatch is behind, in the words of the node reading it.
pub fn upgrade_hint(ours: u8, theirs: u8) -> &'static str {
    if theirs > ours {
        "upgrade this node"
    } else {
        "upgrade the peer"
    }
}

/// Why a received message could not be understood.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error(
        "protocol version mismatch: the peer speaks version {theirs}, this node speaks version {ours}; {}",
        upgrade_hint(*ours, *theirs)
    )]
    VersionMismatch { ours: u8, theirs: u8 },
    #[error("malformed message")]
    Malformed,
}

impl Message {
    /// The wire form: a version byte, then the message.
    pub fn encode(&self) -> Vec<u8> {
        if let Message::UpgradeNotice(notice) = self {
            let mut raw = vec![NOTICE_BYTE];
            postcard::to_io(notice, &mut raw).expect("notice serialises");
            return raw;
        }
        let mut raw = vec![PROTOCOL_VERSION];
        postcard::to_io(self, &mut raw).expect("message serialises");
        raw
    }

    pub fn decode(raw: &[u8]) -> Result<Message, DecodeError> {
        let (&theirs, body) = raw.split_first().ok_or(DecodeError::Malformed)?;
        if theirs == NOTICE_BYTE {
            return postcard::from_bytes(body)
                .map(Message::UpgradeNotice)
                .map_err(|_| DecodeError::Malformed);
        }
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
        // variants are `ChunkReply` and `ChunkPush`. A notice has no index.
        raw.first() != Some(&NOTICE_BYTE) && matches!(raw.get(1), Some(7 | 8))
    }

    /// What an encoded message is for, as the activity view groups traffic.
    pub fn category(raw: &[u8]) -> Category {
        // The variant index again; see `is_bulk`.
        if raw.first() == Some(&NOTICE_BYTE) {
            return Category::Gossip;
        }
        match raw.get(1) {
            Some(6..=9) => Category::Chunk,
            Some(1 | 2) => Category::Metadata,
            Some(0 | 3 | 10..=12 | 13) => Category::Gossip,
            _ => Category::Other,
        }
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

    /// The upgrade notice is the one message whose form never changes (ADR 0006):
    /// every release, whatever its protocol version, must read it. Pinned as bytes
    /// so any change to it fails here.
    #[test]
    fn the_upgrade_notice_keeps_its_frozen_form() {
        let notice = UpgradeNotice {
            release: "v1.10.0".into(),
            protocol: 15,
        };
        let mut frozen = vec![NOTICE_BYTE, 7];
        frozen.extend_from_slice(b"v1.10.0");
        frozen.push(15);
        let msg = Message::UpgradeNotice(notice);
        assert_eq!(msg.encode(), frozen);
        assert_eq!(Message::decode(&frozen), Ok(msg));
    }

    #[test]
    fn a_notice_is_read_whatever_the_protocol_version_that_sent_it() {
        // `decode` never reads the version byte of an ordinary message as a notice.
        assert_ne!(NOTICE_BYTE, PROTOCOL_VERSION);
        let raw = Message::UpgradeNotice(UpgradeNotice {
            release: "v2.0.0".into(),
            protocol: PROTOCOL_VERSION + 1,
        })
        .encode();
        assert!(matches!(
            Message::decode(&raw),
            Ok(Message::UpgradeNotice(n)) if n.protocol == PROTOCOL_VERSION + 1
        ));
        assert!(!Message::is_bulk(&raw));
        assert_eq!(Message::category(&raw), Category::Gossip);
    }

    /// The `(PROTOCOL_VERSION, digest)` the sample messages below encoded to
    /// when the version was last bumped.
    const PINNED: (u8, &str) = (
        14,
        "b4fc671a2a69c38829213aed8deec77d0455b1ac577d813058b8c86265b5105a",
    );

    #[test]
    fn a_change_to_the_wire_shape_comes_with_a_protocol_version_bump() {
        use crate::control::{Efficiency, FilesystemInfo, NodeStats, Repair, Shortfall, Usage};
        use crate::members::{Admission, IdRange, Member, MemberId, TokenRec};
        use crate::net::NodeId;
        use crate::upgrade::{Intent, Progress, Release};
        let member = Member {
            id: MemberId(2),
            endpoint: NodeId(1),
            addr: "host:7600".into(),
            offer_bytes: 5,
            weight_bytes: 4,
            site: "home".into(),
            rev: 3,
            leaving: false,
            removed: false,
        };
        let fs = FilesystemInfo {
            id: "fs".into(),
            name: "home".into(),
            copies: 3,
            copies_rev: 1,
            repair_grace_secs: 1800,
            gc_retention_secs: 604800,
            standin_days: 3,
        };
        let samples = [
            Message::JoinReply(Ok(Admission {
                fs,
                id: MemberId(2),
                ids: IdRange { start: 1, end: 9 },
                members: vec![member.clone()],
            })),
            Message::JoinReply(Err("no".into())),
            Message::Members(vec![member]),
            Message::Tokens(vec![TokenRec {
                id: "t".into(),
                expires: 9,
                revoked: false,
            }]),
            Message::Health(Health {
                unreplicated: vec![Shortfall {
                    copies: 1,
                    chunks: 2,
                }],
                repair: Some(Repair {
                    at_risk: 1,
                    degraded: 2,
                }),
                seen: vec![Sighting {
                    member: MemberId(2),
                    secs_ago: 7,
                }],
                reports: vec![Stranded {
                    member: MemberId(3),
                    chunks: 4,
                    secs_ago: 8,
                }],
                slept: vec![Sighting {
                    member: MemberId(3),
                    secs_ago: 9,
                }],
                load: Load {
                    tick_secs_ago: 10,
                    inbox_messages: 11,
                    inbox_bytes: 12,
                },
            }),
            Message::Copies { copies: 3, rev: 2 },
            Message::ChunkAck([1u8; 32], Replica::Missing),
            Message::StatsRequest,
            Message::Purge,
            Message::Upgrade(UpgradeGossip {
                running: Release {
                    version: "1.10.0".into(),
                    protocol: 15,
                },
                intent: Some(Intent {
                    tag: "v1.11.0".into(),
                    rev: 2,
                    issuer: Some(MemberId(1)),
                    go: true,
                }),
                progress: Some(Progress::Failed("why".into())),
                pause_secs: Some(30),
            }),
            Message::StatsReply(NodeStats {
                usage: Usage {
                    chunks: 1,
                    stored_bytes: 2,
                    offer_bytes: 3,
                },
                efficiency: Efficiency {
                    logical_bytes: 4,
                    referenced_bytes: 5,
                    inline_bytes: 6,
                    local_plain_bytes: 7,
                    local_disk_bytes: 8,
                    raw_chunks: 9,
                    zstd_chunks: 10,
                    zstd_plain_bytes: 11,
                },
                pending_gc_bytes: 12,
            }),
        ];
        // Without the version byte, so only the shape of the messages is pinned.
        let bytes: Vec<u8> = samples
            .iter()
            .flat_map(|m| m.encode()[1..].to_vec())
            .collect();
        let digest = blake3::hash(&bytes).to_hex().to_string();
        assert!(
            PINNED.0 == PROTOCOL_VERSION && digest == PINNED.1,
            "the wire shape changed (digest {digest}): bump PROTOCOL_VERSION (ADR 0002) \
             and set PINNED to ({PROTOCOL_VERSION}, \"{digest}\")",
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
            Message::Copies { copies: 3, rev: 2 },
            Message::ChunkRequest(id),
            Message::ChunkAck(id, Replica::Stored),
        ];
        assert!(bulk.iter().all(|m| Message::is_bulk(&m.encode())));
        assert!(small.iter().all(|m| !Message::is_bulk(&m.encode())));
    }
}
