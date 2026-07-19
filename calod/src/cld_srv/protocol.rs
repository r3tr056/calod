//! RESP protocol-version negotiation and the per-connection request context.
//!
//! A connection starts in [`ProtocolVersion::Resp2`] and may upgrade to RESP3
//! via the `HELLO 3` handshake. The negotiated version is stored on the
//! connection in a shared `AtomicU8` so it can be read on the hot path without
//! locking, and a cheap [`ConnCtx`] snapshot is threaded into command execution
//! and reply encoding.

use std::sync::atomic::{AtomicU8, Ordering};

/// The RESP protocol version negotiated for a connection.
///
/// RESP2 is the historical default understood by every client. RESP3 (Redis 6+)
/// adds native maps, sets, doubles, booleans, big numbers, verbatim strings and
/// out-of-band push frames. The wire byte value matches the integer a client
/// sends in `HELLO <ver>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum ProtocolVersion {
    #[default]
    Resp2 = 2,
    Resp3 = 3,
}

impl ProtocolVersion {
    /// The on-the-wire integer for this version (`2` or `3`).
    #[inline]
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// Parse a `HELLO` protover argument; `None` for anything other than 2 or 3.
    #[inline]
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            2 => Some(ProtocolVersion::Resp2),
            3 => Some(ProtocolVersion::Resp3),
            _ => None,
        }
    }

    /// Whether this is RESP3 (i.e. native maps/sets/doubles/push are in effect).
    #[inline]
    pub fn is_resp3(self) -> bool {
        matches!(self, ProtocolVersion::Resp3)
    }

    /// Load a version from a shared atomic, defaulting to RESP2 on any
    /// unexpected stored byte.
    #[inline]
    pub fn load(atomic: &AtomicU8) -> Self {
        Self::from_u8(atomic.load(Ordering::Relaxed)).unwrap_or(ProtocolVersion::Resp2)
    }

    /// Store this version into a shared atomic.
    #[inline]
    pub fn store(self, atomic: &AtomicU8) {
        atomic.store(self.as_u8(), Ordering::Relaxed);
    }
}

/// A cheap, `Copy` snapshot of the per-connection state needed to execute a
/// command and encode its reply. Threaded into `execute()`/`wrap_response()` so
/// the hot path never locks the connection mutex just to learn the protocol
/// version.
#[derive(Debug, Clone, Copy, Default)]
pub struct ConnCtx {
    /// Negotiated protocol version, decides RESP2 vs RESP3 reply framing.
    pub proto: ProtocolVersion,
    /// Whether the connection currently has any pub/sub subscriptions. In RESP2
    /// this restricts the allowed command set (subscriber mode); RESP3 is
    /// unrestricted. Carried here so the dispatch hot path can gate without
    /// locking the connection mutex.
    pub subscribed: bool,
    /// The currently `SELECT`ed logical database index (0-based). Routes data
    /// commands to the matching `ShardedStore`; defaults to 0.
    pub db: u8,
}

impl ConnCtx {
    #[inline]
    pub fn new(proto: ProtocolVersion) -> Self {
        ConnCtx {
            proto,
            subscribed: false,
            db: 0,
        }
    }
}
