use std::io;
use thiserror::Error;

/// Errors that can occur during RESP protocol parsing
#[derive(Debug, Error)]
pub enum RespError {
    /// The message is incomplete and more data is needed
    #[error("Incomplete message, need more data")]
    Incomplete,

    /// The message format is invalid
    #[error("Invalid RESP format: {0}")]
    InvalidFormat(String),

    /// The message type is not supported
    #[error("Unsupported RESP type")]
    UnsupportedType,

    /// Protocol error occurred
    #[error("Protocol error: {0}")]
    Protocol(String),

    /// I/O error occurred
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    /// Integer parse error
    #[error("Integer parse error: {0}")]
    ParseInt(#[from] std::num::ParseIntError),

    /// UTF-8 error
    #[error("UTF-8 error: {0}")]
    Utf8(#[from] std::str::Utf8Error),

    /// Integer overflow
    #[error("Integer overflow")]
    Overflow,

    /// A bulk string declared a length larger than the configured maximum
    #[error("Protocol error: invalid bulk length")]
    BulkLengthExceeded,

    /// An aggregate (array/map/set) declared more elements than the maximum
    #[error("Protocol error: invalid multibulk length")]
    MultibulkLengthExceeded,

    /// Aggregate nesting exceeded the maximum recursion depth
    #[error("Protocol error: maximum nesting depth exceeded")]
    DepthExceeded,

    /// An inline command line exceeded the maximum permitted size
    #[error("Protocol error: too big inline request")]
    InlineTooLong,
}

impl RespError {
    /// Returns true if the error indicates an incomplete message
    pub fn is_incomplete(&self) -> bool {
        matches!(self, RespError::Incomplete)
    }

    /// Creates a new invalid format error with a message
    pub fn invalid_format(msg: impl Into<String>) -> Self {
        RespError::InvalidFormat(msg.into())
    }

    /// Creates a new protocol error with a message
    pub fn protocol_error(msg: impl Into<String>) -> Self {
        RespError::Protocol(msg.into())
    }
}
