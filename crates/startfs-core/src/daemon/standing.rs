//! Whether a member is up, and the state `status` and `activity` show for it.
//! One answer for every reader, so the two never disagree.

use super::DOWN_AFTER;
use crate::control::NodeState;
use crate::ops::Nanos;

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
