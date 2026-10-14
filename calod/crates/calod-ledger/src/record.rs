//! WAL record payloads.
//!
//! All ledger mutations travel as [`calod_storage::wal::RecordType::Put`]
//! records whose payload is the JSON encoding of [`LedgerWalRecord`]. The
//! effect mutation and its run-timeline event share **one** record
//! ([`RecordKind::EffectApplied`]), which is what makes "effect update +
//! event append" a single durable unit.
//!
//! Decoding is fail-closed: an unknown `v` or `kind` is corruption, never a
//! silent skip.

use serde::{Deserialize, Serialize};

use calod_core::store::error::PersistenceError;

use crate::model::{CheckpointRecord, EffectRecord, EventRecord, RunRecord};

/// On-disk record schema version.
pub const RECORD_VERSION: u32 = 1;

/// One ledger mutation as stored in the WAL.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerWalRecord {
    /// Record schema version ([`RECORD_VERSION`]).
    pub v: u32,
    /// The mutation.
    #[serde(flatten)]
    pub kind: RecordKind,
}

/// The mutation carried by a [`LedgerWalRecord`]. Records are boxed to keep
/// the enum small; they are per-transition allocations anyway.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecordKind {
    /// A run was created.
    RunCreated {
        /// The new run.
        run: Box<RunRecord>,
    },
    /// A run's mutable fields (status, version) changed; the record
    /// replaces the stored one wholesale.
    RunUpdated {
        /// The run after the update.
        run: Box<RunRecord>,
    },
    /// An effect changed state; the record replaces the stored one
    /// wholesale. The run-timeline event documenting the transition travels
    /// in the same record, so the two are one atomic durable unit.
    EffectApplied {
        /// The effect after the transition.
        effect: Box<EffectRecord>,
        /// The appended run event, when the effect has a run.
        event: Option<Box<EventRecord>>,
    },
    /// A standalone run event append (not tied to an effect transition).
    EventAppended {
        /// The appended event.
        event: Box<EventRecord>,
    },
    /// An immutable checkpoint was created.
    CheckpointCreated {
        /// The new checkpoint.
        checkpoint: Box<CheckpointRecord>,
    },
}

/// Encode one mutation into a WAL payload.
pub(crate) fn encode(kind: &RecordKind) -> Result<Vec<u8>, PersistenceError> {
    let record = LedgerWalRecord {
        v: RECORD_VERSION,
        kind: kind.clone(),
    };
    serde_json::to_vec(&record)
        .map_err(|err| PersistenceError::WalWriteFailed(format!("ledger record encode: {err}")))
}

/// Decode a WAL payload, rejecting unknown versions and malformed records.
pub(crate) fn decode(payload: &[u8]) -> Result<LedgerWalRecord, PersistenceError> {
    let record: LedgerWalRecord = serde_json::from_slice(payload).map_err(|err| {
        PersistenceError::Corruption(format!("undecodable ledger WAL record: {err}"))
    })?;
    if record.v != RECORD_VERSION {
        return Err(PersistenceError::Corruption(format!(
            "unsupported ledger record version {} (expected {RECORD_VERSION})",
            record.v
        )));
    }
    Ok(record)
}
