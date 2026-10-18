//! Messages from peers: join admission, chunk traffic, gossip and log exchange.

use super::Daemon;
use crate::error::Error;
use crate::net::{NodeId, Transport};
use crate::protocol::{Message, Replica};

impl Daemon {
    /// Act on one message a peer sent.
    pub(super) fn handle_peer_message(
        &self,
        transport: &dyn Transport,
        from: NodeId,
        msg: Message,
    ) {
        // Replies expose log entries and acknowledge chunks: make them durable first.
        self.fs.sync_logged();
        // Chunk traffic is for members only.
        if matches!(
            msg,
            Message::ChunkRequest(_)
                | Message::ChunkReply(..)
                | Message::ChunkPush(..)
                | Message::ChunkAck(..)
                | Message::StatsRequest
                | Message::StatsReply(_)
        ) && !self.membership.lock().unwrap().is_member_endpoint(from)
        {
            return;
        }
        match msg {
            Message::JoinRequest(request) => {
                let now = self.clock.now();
                let result = self.membership.lock().unwrap().admit(now, from, request);
                if let Ok(adm) = &result {
                    let members = Message::Members(adm.members.clone()).encode();
                    for peer in self.membership.lock().unwrap().peers() {
                        transport.send(peer, members.clone());
                    }
                }
                // The refusal travels as its display text.
                let reply = result.map_err(|e| e.to_string());
                transport.send(from, Message::JoinReply(reply).encode());
            }
            Message::JoinReply(reply) => {
                *self.join_reply.lock().unwrap() = Some(reply.map_err(Error::Remote));
            }
            Message::ChunkRequest(id) => {
                let data = self.fs.local_chunk(&id);
                transport.send(from, Message::ChunkReply(id, data).encode());
            }
            Message::ChunkReply(id, data) => {
                let healed = self
                    .with_view(|v| self.replicator.on_heal_reply(v, &id, data.as_deref()))
                    .unwrap_or(false);
                if !healed {
                    let at = self.clock.now();
                    self.chunk_replies
                        .lock()
                        .unwrap()
                        .insert((from, id), (at, data));
                }
            }
            Message::ChunkPush(id, data) => {
                let standing = self
                    .with_view(|v| self.replicator.on_push(v, &id, data.as_deref()))
                    .unwrap_or(Replica::Full);
                transport.send(from, Message::ChunkAck(id, standing).encode());
            }
            Message::ChunkAck(id, standing) => {
                self.with_view(|v| self.replicator.on_ack(v, from, &id, standing));
            }
            Message::StatsRequest => {
                if let Ok(stats) = self.local_node_stats() {
                    transport.send(from, Message::StatsReply(stats).encode());
                }
            }
            Message::StatsReply(stats) => {
                self.stats_replies.lock().unwrap().insert(from, stats);
            }
            Message::Tokens(list) => {
                let mut m = self.membership.lock().unwrap();
                if m.is_member_endpoint(from) {
                    m.merge_tokens(list);
                }
            }
            Message::Health(health) => {
                if self.membership.lock().unwrap().is_member_endpoint(from) {
                    let now = self.clock.now();
                    let me = self.membership.lock().unwrap().me();
                    // Keep the latest sleep anyone saw: the extra copies made
                    // for a member stay as long as the freshest sighting says.
                    let mut slept = self.slept.lock().unwrap();
                    for s in health.slept.iter().filter(|s| Some(s.member) != me) {
                        let at = now.saturating_sub(s.secs_ago * 1_000_000_000);
                        let known = slept.entry(s.member).or_insert(at);
                        *known = (*known).max(at);
                    }
                    drop(slept);
                    self.peer_health.lock().unwrap().insert(from, (now, health));
                }
            }
            Message::Copies { copies, rev } => {
                let mut m = self.membership.lock().unwrap();
                if m.is_member_endpoint(from) {
                    m.merge_copies(copies, rev);
                }
            }
            Message::Members(list) => {
                let mut m = self.membership.lock().unwrap();
                if m.is_member_endpoint(from) && m.merge(list) {
                    drop(m);
                    self.sync_peers();
                }
            }
            msg @ (Message::Hello(_) | Message::Entries(_) | Message::StateChunk(_)) => {
                if let Some(s) = self.syncer.lock().unwrap().as_mut() {
                    s.handle(from, msg);
                }
            }
        }
    }
}
