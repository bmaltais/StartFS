//! The activity view: this node's recent rates, queues and resource use.

use super::standing::{is_up, node_state};
use super::{Daemon, UNCLAIMED_REPLY_BYTES, answer_bytes};
use crate::activity::{ActivityReport, PeerActivity, QueueActivity, Structure, Structures};
use crate::error::Error;
use crate::net::Inbox;

/// A shorter window says too little to hint at a busy tick.
const MIN_HINT_WINDOW_SECS: f64 = 10.0;

impl Daemon {
    /// Switch the activity counters on or off.
    pub(super) fn set_activity(&self, on: bool) {
        self.activity.set_enabled(on);
    }

    /// The share of the window the tick thread worked and the average tick in
    /// milliseconds, once the window is long enough to say anything.
    pub(super) fn tick_busy(&self) -> Option<(f64, f64)> {
        let totals = self.activity.totals();
        if totals.window_secs < MIN_HINT_WINDOW_SECS {
            return None;
        }
        let t = totals.tick?;
        Some((t.busy_fraction, t.avg_ms))
    }

    pub(super) fn activity_report(&self) -> Result<ActivityReport, Error> {
        let now = self.clock.now();
        let totals = self.activity.totals();
        let enabled = self.activity.enabled();
        let (chunks, in_flight, oldest_secs) = self.replicator.queue(now);
        let heard = self.heard.lock().unwrap().clone();
        let lost = self.with_view(|v| v.lost.clone()).unwrap_or_default();
        let paused = self.paused_members(now);
        let peers = {
            let membership = self.membership.lock().unwrap();
            let me = membership.me();
            membership
                .members()
                .into_iter()
                .filter(|m| Some(m.id) != me)
                .map(|m| {
                    let up = is_up(heard.get(&m.endpoint).copied(), now);
                    PeerActivity {
                        member: m.id,
                        state: node_state(m.leaving, up, lost.contains(&m.id), paused.contains(&m.id)),
                        in_flight: in_flight.get(&m.id).copied().unwrap_or(0),
                        traffic: enabled
                            .then(|| totals.peers.get(&m.endpoint).copied().unwrap_or_default()),
                        addr: m.addr,
                    }
                })
                .collect()
        };
        let staging = self.fs.staging_stats()?;
        let (decoded_entries, decoded_bytes, decoded_limit) = self.fs.decoded_stats();
        let (reply_entries, reply_bytes) = {
            let replies = self.chunk_replies.lock().unwrap();
            let bytes = replies.values().map(|(_, a)| answer_bytes(a)).sum();
            (replies.len() as u64, bytes)
        };
        let structures = Structures {
            inbox: self
                .transport()
                .and_then(|t| t.inbox())
                .map(|Inbox { messages, bytes }| Structure {
                    entries: messages,
                    bytes: Some(bytes),
                    limit_bytes: None,
                }),
            chunk_replies: Some(Structure {
                entries: reply_entries,
                bytes: Some(reply_bytes),
                limit_bytes: Some(UNCLAIMED_REPLY_BYTES),
            }),
            decoded_cache: Some(Structure {
                entries: decoded_entries,
                bytes: Some(decoded_bytes),
                limit_bytes: Some(decoded_limit),
            }),
            replication_index: Some(Structure {
                entries: self.replicator.tracked(),
                bytes: None,
                limit_bytes: None,
            }),
            staging_index: Some(Structure {
                entries: staging.chunks,
                bytes: None,
                limit_bytes: None,
            }),
        };
        Ok(ActivityReport {
            enabled,
            window_secs: if enabled { totals.window_secs } else { 0.0 },
            peers,
            tick: totals.tick,
            queue: QueueActivity {
                chunks,
                oldest_secs,
            },
            chunk_sizes: if enabled {
                totals.chunk_sizes
            } else {
                Vec::new()
            },
            structures,
            resources: self.resources.lock().unwrap().clone().sample(),
        })
    }
}
