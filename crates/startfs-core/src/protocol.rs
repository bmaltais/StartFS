//! The wire protocol peers speak over a [`Transport`](crate::net::Transport).

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
}

impl Message {
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("message serialises")
    }

    pub fn decode(raw: &[u8]) -> Option<Message> {
        postcard::from_bytes(raw).ok()
    }
}
