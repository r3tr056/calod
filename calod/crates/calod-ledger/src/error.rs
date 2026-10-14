//! Typed ledger errors. Each variant carries the HTTP status the REST layer
//! should map it to; see [`LedgerError::status_code`].

use calod_core::store::error::PersistenceError;

/// Errors returned by [`crate::Ledger`] operations.
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    /// The requested resource does not exist within the caller's
    /// tenant/project. Maps to 404.
    #[error("{resource} not found: {id}")]
    NotFound {
        /// Resource kind (`run`, `effect`, `checkpoint`).
        resource: &'static str,
        /// Identifier that was looked up.
        id: String,
    },

    /// The idempotency key is already bound to a different canonical input
    /// digest or tool. Maps to 409 (`IDEMPOTENCY_CONFLICT`).
    #[error("idempotency key {key:?} in scope {scope:?} is already bound to digest {existing_digest} (requested {requested_digest})")]
    IdempotencyConflict {
        /// Effect scope.
        scope: String,
        /// Idempotency key.
        key: String,
        /// Digest stored on the existing effect.
        existing_digest: String,
        /// Digest computed from the conflicting request.
        requested_digest: String,
    },

    /// The presented fencing token is not the current one; a newer lease
    /// epoch owns the effect. Maps to 409 (`LEASE_LOST`).
    #[error("stale fencing token {got}: current fencing token is {current}")]
    StaleFencingToken {
        /// The effect's current fencing token.
        current: i64,
        /// The fencing token presented by the caller.
        got: i64,
    },

    /// Lease token mismatch or missing caller capability. Maps to 403.
    #[error("forbidden: {0}")]
    Forbidden(String),

    /// The requested transition is not legal from the current state.
    /// Maps to 422 (`INVALID_TRANSITION`).
    #[error("invalid effect transition {transition} from state {from}")]
    InvalidTransition {
        /// Current state of the resource.
        from: String,
        /// Attempted transition.
        transition: String,
    },

    /// A request field failed validation. Maps to 400.
    #[error("validation error on {field}: {message}")]
    Validation {
        /// Offending field name.
        field: &'static str,
        /// What is wrong with it.
        message: String,
    },

    /// Optimistic-concurrency version mismatch (`If-Match`). Maps to 412.
    #[error("precondition failed: expected version {expected}, current version is {current}")]
    PreconditionFailed {
        /// Version the caller expected.
        expected: i64,
        /// Current resource version.
        current: i64,
    },

    /// Durable storage failed; no state was accepted. Maps to 503.
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
}

impl LedgerError {
    /// HTTP status the REST layer should return for this error.
    pub fn status_code(&self) -> u16 {
        match self {
            Self::NotFound { .. } => 404,
            Self::IdempotencyConflict { .. } | Self::StaleFencingToken { .. } => 409,
            Self::Forbidden(_) => 403,
            Self::InvalidTransition { .. } => 422,
            Self::Validation { .. } => 400,
            Self::PreconditionFailed { .. } => 412,
            Self::Persistence(_) => 503,
        }
    }

    /// Stable machine-readable code (`Problem.code` in the OpenAPI contract).
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound { .. } => "RESOURCE_NOT_FOUND",
            Self::IdempotencyConflict { .. } => "IDEMPOTENCY_CONFLICT",
            Self::StaleFencingToken { .. } => "LEASE_LOST",
            Self::Forbidden(_) => "INSUFFICIENT_SCOPE",
            Self::InvalidTransition { .. } => "INVALID_TRANSITION",
            Self::Validation { .. } => "VALIDATION_ERROR",
            Self::PreconditionFailed { .. } => "PRECONDITION_FAILED",
            Self::Persistence(_) => "SERVICE_UNAVAILABLE",
        }
    }
}

/// Convenience result alias for ledger operations.
pub type Result<T> = std::result::Result<T, LedgerError>;
