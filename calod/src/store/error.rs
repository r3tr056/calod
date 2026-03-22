use std::io;
use thiserror::Error;
use tracing::error;

/// Cache errors that can occur during operations
#[derive(Debug, Error, Clone)]
pub enum CacheError {
    #[error("Key `{0}` not found")]
    KeyNotFound(String),

    #[error("Key `{0}` has expired")]
    KeyExpired(String),

    #[error("Invalid command arguments: {0}")]
    InvalidCommandArguments(String),

    #[error("Command not implemented: {0}")]
    CommandNotImplemented(String),

    #[error("Internal cache error: {0}")]
    InternalError(String),

    #[error("Data type mismatch for key `{0}`. Expected `{1}`, found `{2}`")]
    DataTypeMismatch(String, String, String),

    #[error("Field `{0}` not found in hash for key `{1}`")]
    FieldNotFound(String, String),

    #[error("Mutatuon `{0}` not supported")]
    MutationNotSupported(String),

    #[error("Invalid score format")]
    InvalidScoreFormat,

    #[error("Index out of range")]
    IndexOutOfRange,

    #[error("Value is not an integer")]
    NotAnInteger,

    #[error("Transaction failed: {0}")]
    TransactionFailed(String),
    
    #[error("Operation would exceed memory limit")]
    MemoryLimitExceeded,
    
    #[error("Write operation failed due to read-only mode")]
    ReadOnlyMode,
    
    #[error("Authentication failed")]
    AuthenticationFailed,
    
    #[error("Permission denied")]
    PermissionDenied,
    
    #[error("Replication error: {0}")]
    ReplicationError(String),
    
    #[error("Cluster error: {0}")]
    ClusterError(String),
    
    
    #[error("Configuration error: {0}")]
    ConfigurationError(String),
}

#[derive(Debug, Error)]
pub enum PersistenceError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    #[error("Serialization error: {0}")]
    Serialization(bincode::Error),

    #[error("Corruption detected: {0}")]
    Corruption(String),

    #[error("Permission denied: {0}")]
    PermissionDenied(String),

    #[error("Persistence directory not writable")]
    DirectoryNotWritable,

    #[error("Snapshot creation failed: {0}")]
    SnapshotCreationFailed(String),

    #[error("AOF write failed: {0}")]
    AofWriteFailed(String),

    #[error("Failed to load data: {0}")]
    LoadFailed(String),
}
