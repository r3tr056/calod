use thiserror::Error;
use std::io;

/// Network errors
#[derive(Debug, Error)]
pub enum NetworkError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    #[error("Failed to bind to address: {0}")]
    BindFailed(String),

    #[error("Connection closed")]
    ConnectionClosed,

    #[error("Connection timeout")]
    ConnectionTimeout,

    #[error("TLS error: {0}")]
    TlsError(String),

    #[error("Too many connections")]
    TooManyConnections,

    #[error("Protocol error: {0}")]
    Protocol(String),
}
