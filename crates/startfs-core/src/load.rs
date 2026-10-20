//! How far behind a node's work is, as it gossips it in `Health`.

use serde::{Deserialize, Serialize};

/// A tick that has not finished for this long (seconds) puts a node behind.
const BEHIND_TICK_SECS: u64 = 30;

/// What a node says about its own backlog. All zeros reads as "keeping up".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Load {
    /// Seconds since the sender's last tick began. A tick stuck behind slow
    /// disk keeps growing it while the sender's hellos still go out.
    pub tick_secs_ago: u64,
    /// Messages received and not yet handled.
    pub inbox_messages: u64,
    /// Bytes of those messages.
    pub inbox_bytes: u64,
}

impl Load {
    /// How far behind this is, for `status`: `3 min behind (10k messages queued)`.
    pub fn behind_text(&self) -> String {
        let queued = match self.inbox_messages {
            n if n >= 1000 => format!("{}k", n.div_ceil(1000)),
            n => n.to_string(),
        };
        match self.tick_secs_ago {
            s if s >= 60 => format!("{} min behind ({queued} messages queued)", s / 60),
            s if s >= BEHIND_TICK_SECS => format!("{s} s behind ({queued} messages queued)"),
            _ => format!("behind ({queued} messages queued)"),
        }
    }

    /// Whether the sender is behind: its tick has not begun a new round in a while, or
    /// `inbox_limit` messages wait. A node already taken for behind is let off
    /// only once both are under half their limit, so one that hovers at the
    /// limit is not skipped and resumed on every hello.
    pub fn is_behind(&self, inbox_limit: u64, was_behind: bool) -> bool {
        let (tick, inbox) = if was_behind {
            (BEHIND_TICK_SECS / 2, inbox_limit / 2)
        } else {
            (BEHIND_TICK_SECS, inbox_limit)
        };
        self.tick_secs_ago >= tick || (inbox > 0 && self.inbox_messages >= inbox)
    }
}
