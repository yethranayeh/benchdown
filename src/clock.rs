//! The wall clock and sleeping, so tests can drive time by hand.

#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub trait Clock: Send + Sync {
    /// Milliseconds since the Unix epoch.
    fn now_ms(&self) -> u64;
    fn sleep_ms(&self, milliseconds: u64);
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("the clock is after 1970")
            .as_millis() as u64
    }

    fn sleep_ms(&self, milliseconds: u64) {
        std::thread::sleep(Duration::from_millis(milliseconds));
    }
}

/// A clock that only moves when told to, for tests.
#[cfg(test)]
pub struct FakeClock {
    now: AtomicU64,
    slept: AtomicU64,
}

#[cfg(test)]
impl FakeClock {
    pub fn new(now_ms: u64) -> Self {
        Self {
            now: AtomicU64::new(now_ms),
            slept: AtomicU64::new(0),
        }
    }

    pub fn advance(&self, milliseconds: u64) {
        self.now.fetch_add(milliseconds, Ordering::SeqCst);
    }

    pub fn slept_ms(&self) -> u64 {
        self.slept.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        self.now.load(Ordering::SeqCst)
    }

    fn sleep_ms(&self, milliseconds: u64) {
        self.slept.fetch_add(milliseconds, Ordering::SeqCst);
        self.advance(milliseconds);
    }
}
