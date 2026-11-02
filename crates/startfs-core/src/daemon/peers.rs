//! Messages from peers: join admission, chunk traffic, gossip and log exchange.

use super::Daemon;
use crate::chunkstore::ChunkId;
use crate::error::Error;
use crate::net::{NodeId, Transport};
use crate::oplog::Entry;
use crate::protocol::{Message, Replica};

impl Daemon {
    /// Apply the entries several peers sent during one drain, in one commit.
    pub(super) fn handle_peer_entries(&self, batches: Vec<(NodeId, Vec<Entry>)>) {
        if batches.is_empty() {
            return;
        }
        // Replies say what this node holds durably; its own lazy commits go first.
        self.fs.sync_local_logged();
        if let Some(s) = self.syncer.lock().unwrap().as_mut() {
            s.handle_entries(batches);
        }
    }

    /// Act on one message a peer sent.
    pub(super) fn handle_peer_message(
        &self,
        transport: &dyn Transport,
        from: NodeId,
        msg: Message,
    ) {
        // Replies acknowledge chunks and expose this node's own entries: make them durable
        // first. Peers' entries applied lazily are not exposed until a durable commit holds them.
        self.fs.sync_local_logged();
        // Chunk traffic is for members only.
        if matches!(
            msg,
            Message::ChunkRequest(_)
                | Message::ChunkReply(..)
                | Message::ChunkPush(..)
                | Message::ChunkAck(..)
                | Message::StatsRequest
                | Message::StatsReply(_)
                | Message::Purge
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
                    self.prune_chunk_replies();
                }
            }
            Message::ChunkPush(id, data) => {
                // The tick answers pushes in groups (`take_push`); this is one on its own.
                let mut group = vec![self.take_push(from, id, data.as_deref())];
                self.ack_pushes(transport, &mut group);
            }
            Message::ChunkAck(id, standing) => {
                self.with_view(|v| self.replicator.on_ack(v, from, &id, standing));
            }
            Message::StatsRequest => {
                if let Ok(stats) = self.local_node_stats() {
                    transport.send(from, Message::StatsReply(stats).encode());
                }
            }
            Message::Purge => self.begin_purge(),
            Message::StatsReply(stats) => {
                self.stats_replies.lock().unwrap().insert(from, stats);
            }
            Message::Tokens(list) => {
                let mut m = self.membership.lock().unwrap();
                if m.is_member_endpoint(from) {
                    m.merge_tokens(list);
                }
            }
            Message::KeyWraps(list) => {
                let mut m = self.membership.lock().unwrap();
                if m.is_member_endpoint(from) {
                    m.merge_key_wraps(list);
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
                    self.note_load(from, health.load);
                    self.peer_health.lock().unwrap().insert(from, (now, health));
                }
            }
            Message::Copies { copies, rev } => {
                let mut m = self.membership.lock().unwrap();
                if m.is_member_endpoint(from) {
                    m.merge_copies(copies, rev);
                }
            }
            Message::KeepVersions { secs, rev } => {
                let mut m = self.membership.lock().unwrap();
                if m.is_member_endpoint(from) {
                    m.merge_keep_versions(secs, rev);
                }
            }
            Message::Members(list) => {
                let mut m = self.membership.lock().unwrap();
                if m.is_member_endpoint(from) && m.merge(list) {
                    drop(m);
                    self.sync_peers();
                }
            }
            Message::Upgrade(gossip) => self.on_upgrade_gossip(from, gossip),
            Message::UpgradeNotice(notice) => self.on_upgrade_notice(from, notice),
            msg @ (Message::Hello(_) | Message::Entries(_) | Message::StateChunk(_)) => {
                if let Some(s) = self.syncer.lock().unwrap().as_mut() {
                    s.handle(from, msg);
                }
            }
        }
    }
}

impl Daemon {
    /// Take the chunk a member pushed, not yet synced, and say what to answer once it is.
    pub(super) fn take_push(
        &self,
        from: NodeId,
        id: ChunkId,
        data: Option<&[u8]>,
    ) -> (NodeId, ChunkId, Replica) {
        let standing = self
            .with_view(|v| self.replicator.on_push(v, &id, data))
            .unwrap_or(Replica::Full);
        (from, id, standing)
    }

    /// Answer a group of pushes after one sync for all of them: nothing is acknowledged that is
    /// not on disk, and a failed sync says "full" instead.
    pub(super) fn ack_pushes(
        &self,
        transport: &dyn Transport,
        group: &mut Vec<(NodeId, ChunkId, Replica)>,
    ) {
        if group.is_empty() {
            return;
        }
        let durable = self.fs.sync_checked();
        for (from, id, standing) in group.drain(..) {
            let standing = match standing {
                Replica::Stored if !durable => Replica::Full,
                other => other,
            };
            transport.send(from, Message::ChunkAck(id, standing).encode());
        }
    }
}
