//! A wake-up for a thread that sleeps between rounds of work: it waits up to a
//! timeout, and anything that gives it work cuts the wait short.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

/// One waiter at a time; any number of threads may wake it.
#[derive(Default)]
pub struct Waker {
    woken: AtomicBool,
    lock: Mutex<()>,
    cv: Condvar,
}

impl Waker {
    /// Cut the current wait short, or the next one if nobody is waiting. Wakes
    /// that arrive before the waiter has run again are one wake. While one is
    /// pending this is a single atomic read, so it is cheap enough for hot paths.
    pub fn wake(&self) {
        if !self.woken.swap(true, Ordering::SeqCst) {
            // Taking the lock orders the flag before the waiter's next check of it.
            drop(self.lock.lock().unwrap());
            self.cv.notify_one();
        }
    }

    /// Sleep up to `timeout`, or return at once if woken since the last wait.
    /// Returns whether it was woken.
    pub fn wait(&self, timeout: Duration) -> bool {
        let guard = self.lock.lock().unwrap();
        let (guard, _) = self
            .cv
            .wait_timeout_while(guard, timeout, |()| !self.woken.load(Ordering::SeqCst))
            .unwrap();
        drop(guard);
        self.woken.swap(false, Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Instant;

    #[test]
    fn a_wake_before_the_wait_is_not_lost() {
        let w = Waker::default();
        w.wake();
        let started = Instant::now();
        assert!(w.wait(Duration::from_secs(10)));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(!w.wait(Duration::from_millis(1)), "one wake, one return");
    }

    #[test]
    fn a_wake_during_the_wait_ends_it() {
        let w = Arc::new(Waker::default());
        let waker = w.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            waker.wake();
        });
        let started = Instant::now();
        assert!(w.wait(Duration::from_secs(10)));
        assert!(started.elapsed() < Duration::from_secs(5));
        t.join().unwrap();
    }

    #[test]
    fn a_wait_times_out_when_nobody_wakes() {
        assert!(!Waker::default().wait(Duration::from_millis(5)));
    }
}
