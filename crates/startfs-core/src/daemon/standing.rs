//! Whether a member is up, and the state `status` and `activity` show for it.
//! One answer for every reader, so the two never disagree.

use super::DOWN_AFTER;
use crate::control::NodeState;
use crate::members::MemberId;
use crate::net::NodeId;
use crate::ops::Nanos;
use crate::protocol::Health;
use std::collections::HashMap;
use std::time::Duration;

/// Since when a member counts as unseen here, for the long waits (taking it for asleep, giving
/// it up for lost): when this daemon last heard it, or, when it has not heard it since it
/// started, from then or from earlier when its peers relayed (in `Health`, ADR 0003) that
/// nobody has seen it for longer. A restarted daemon so does not wait out the stand-in delay
/// or the grace period again for a member long gone. What peers relay never makes a member
/// look more recently seen than the daemon's start. Being down is still judged on this
/// node's own sightings: a peer that just woke from a suspend relays stale ones, and waiting
/// out the short down delay again costs nothing.
pub(super) fn unseen_since(
    last_heard: Option<Nanos>,
    member: MemberId,
    peer_health: &HashMap<NodeId, (Nanos, Health)>,
    started: Nanos,
) -> Nanos {
    last_heard.unwrap_or_else(|| {
        let relayed = peer_health.values().filter_map(|(arrived, h)| {
            let s = h.seen.iter().find(|s| s.member == member)?;
            Some(arrived.saturating_sub(Duration::from_secs(s.secs_ago).as_nanos() as Nanos))
        });
        relayed.max().map_or(started, |at| at.min(started))
    })
}

/// A member heard from within `DOWN_AFTER` is up.
pub(super) fn is_up(last_heard: Option<Nanos>, now: Nanos) -> bool {
    last_heard.is_some_and(|t| now.saturating_sub(t) < DOWN_AFTER.as_nanos() as Nanos)
}

/// Not heard from since this daemon started, and not yet for long enough to call it down.
pub(super) fn is_unknown(last_heard: Option<Nanos>, started: Nanos, now: Nanos) -> bool {
    last_heard.is_none() && now.saturating_sub(started) < DOWN_AFTER.as_nanos() as Nanos
}

/// `paused`: the member announced an upgrade pause and is expected back.
pub(super) fn node_state(
    leaving: bool,
    up: bool,
    unknown: bool,
    lost: bool,
    paused: bool,
) -> NodeState {
    if leaving {
        NodeState::Leaving
    } else if up {
        NodeState::Online
    } else if unknown {
        NodeState::Unknown
    } else if paused {
        NodeState::Upgrading
    } else if lost {
        NodeState::Lost
    } else {
        NodeState::Down
    }
}
