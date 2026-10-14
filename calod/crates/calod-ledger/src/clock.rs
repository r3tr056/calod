//! Time source abstraction. All lease-expiry decisions read time through a
//! [`Clock`], so tests can pin and advance time deterministically.

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// A source of wall-clock time in milliseconds since the Unix epoch.
pub trait Clock: Send + Sync + std::fmt::Debug {
    /// Current time in milliseconds since the Unix epoch.
    fn now_ms(&self) -> i64;
}

/// Wall-clock time from the operating system.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        i64::try_from(millis).unwrap_or(i64::MAX)
    }
}

/// Test clock: time only moves when told to.
#[derive(Debug)]
pub struct ManualClock {
    now: AtomicI64,
}

impl ManualClock {
    /// A clock pinned at `now_ms`.
    pub fn new(now_ms: i64) -> Self {
        Self {
            now: AtomicI64::new(now_ms),
        }
    }

    /// Set the current time.
    pub fn set(&self, now_ms: i64) {
        self.now.store(now_ms, Ordering::Release);
    }

    /// Advance the clock by `delta_ms` and return the new reading.
    pub fn advance(&self, delta_ms: i64) -> i64 {
        self.now.fetch_add(delta_ms, Ordering::AcqRel) + delta_ms
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> i64 {
        self.now.load(Ordering::Acquire)
    }
}
