//! Injectable time. Everything time-dependent takes a [`Clock`].

use crate::ops::Nanos;
use std::sync::atomic::{AtomicU64, Ordering};

pub trait Clock: Send + Sync {
    fn now(&self) -> Nanos;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Nanos {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    }
}

/// A clock that only moves when told to.
#[derive(Debug)]
pub struct ManualClock(AtomicU64);

impl ManualClock {
    pub fn new(start: Nanos) -> Self {
        Self(AtomicU64::new(start))
    }
    pub fn advance(&self, by: std::time::Duration) {
        self.0.fetch_add(by.as_nanos() as u64, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Nanos {
        self.0.load(Ordering::SeqCst)
    }
}
