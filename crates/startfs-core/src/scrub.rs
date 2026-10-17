//! The scrub: a slow background pass that reads every replica this node holds
//! and checks it against its hash, so silent corruption is found before the
//! data is needed.

use crate::chunkstore::ChunkId;
use crate::control::{ScrubState, ScrubStatus};
use crate::fs::LocalFs;
use crate::ops::Nanos;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Replicas checked per daemon tick: a pass is spread thin so it never
/// competes with real work for the disk.
const CHUNKS_PER_STEP: usize = 8;

#[derive(Default)]
struct State {
    /// When the last full pass finished (read from disk once).
    last_done: Option<Option<Nanos>>,
    /// Replicas still to check in the pass under way.
    queue: Option<VecDeque<ChunkId>>,
}

pub struct Scrubber {
    fs: Arc<LocalFs>,
    state: Mutex<State>,
}

/// What one step of the scrub did.
#[derive(Default)]
pub struct Step {
    /// Whether any checking was done.
    pub worked: bool,
    /// Replicas found corrupt in this step.
    pub corrupt: Vec<ChunkId>,
}

impl Scrubber {
    pub fn new(fs: Arc<LocalFs>) -> Self {
        Self {
            fs,
            state: Mutex::new(State::default()),
        }
    }

    fn last_done(&self, st: &mut State) -> Option<Nanos> {
        *st.last_done
            .get_or_insert_with(|| self.fs.scrub_last_done())
    }

    fn finish_pass(&self, st: &mut State, now: Nanos) {
        st.last_done = Some(Some(now));
        self.fs.set_scrub_done(now);
    }

    pub fn status(
        &self,
        now: Nanos,
        period: Duration,
        on_battery: bool,
        healing: u32,
    ) -> ScrubStatus {
        let remaining = self
            .state
            .lock()
            .unwrap()
            .queue
            .as_ref()
            .map(|q| q.len() as u64);
        let running = remaining.is_some();
        let last_pass = self.last_done(&mut self.state.lock().unwrap());
        let next_pass = last_pass.map(|t| t + period.as_nanos() as u64);
        let due = next_pass.is_some_and(|t| t <= now);
        let state = match (running || due, on_battery) {
            (true, true) => ScrubState::PausedOnBattery,
            _ if running => ScrubState::Running,
            _ => ScrubState::Idle,
        };
        ScrubStatus {
            state,
            last_pass,
            next_pass,
            remaining: remaining.unwrap_or(0),
            healing,
        }
    }

    /// Check a few more replicas if a pass is due or under way.
    pub fn step(&self, now: Nanos, period: Duration, on_battery: bool) -> Step {
        if on_battery {
            return Step::default();
        }
        let mut st = self.state.lock().unwrap();
        if st.queue.is_none() {
            match self.last_done(&mut st) {
                // The first pass is one period after the node first ran.
                None => {
                    self.finish_pass(&mut st, now);
                    return Step::default();
                }
                Some(last) if now.saturating_sub(last) < period.as_nanos() as u64 => {
                    return Step::default();
                }
                Some(_) => st.queue = Some(self.fs.held().unwrap_or_default().into()),
            }
        }
        let queue = st.queue.as_mut().expect("pass under way");
        let mut step = Step {
            worked: true,
            corrupt: Vec::new(),
        };
        for _ in 0..CHUNKS_PER_STEP {
            let Some(id) = queue.pop_front() else {
                break;
            };
            if self.fs.has_replica(&id) && !self.fs.replica_is_sound(&id) {
                step.corrupt.push(id);
            }
        }
        if queue.is_empty() {
            st.queue = None;
            self.finish_pass(&mut st, now);
        }
        step
    }
}
