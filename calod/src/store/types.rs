
use std::fmt;
use std::time::{Duration, SystemTime};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ttl(pub u64);

impl Ttl {
    /// Convert TTL to Duration
    pub fn to_duration(&self) -> Duration {
        Duration::from_millis(self.0)
    }

    /// Check if TTL has expired relative to the given timestamp
    pub fn is_expired(&self, created_at: &Timestamp) -> bool {
        if self.0 == 0 {
            return false;
        }
        created_at.elapsed_ms() >= self.0
    }
}

/// Timestamp wrapper with serialization support
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timestamp(pub SystemTime);

impl Timestamp {
    /// Create a new timestamp representing the current time
    pub fn now() -> Self {
        Timestamp(SystemTime::now())
    }
    
    /// Get elapsed milliseconds since this timestamp
    pub fn elapsed_ms(&self) -> u64 {
        self.0
            .elapsed()
            .unwrap_or_else(|_| Duration::from_secs(0))
            .as_millis() as u64
    }
}

impl fmt::Debug for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(d) => write!(f, "Timestamp({}s)", d.as_secs()),
            Err(_) => write!(f, "Timestamp(invalid)"),
        }
    }
}