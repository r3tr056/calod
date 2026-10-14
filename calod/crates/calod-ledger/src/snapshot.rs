//! Ledger index snapshots, following the `calod-storage` snapshot pattern:
//! the state file is written first, then a `manifest.json` recording the
//! format version, per-file CRC32C, and the WAL watermark the snapshot
//! covers. Loading is fail-closed: state files without a readable manifest,
//! unknown format versions, and checksum mismatches are all corruption.
//!
//! (`calod_storage::snapshot::save_verified`/`load_verified` themselves are
//! tied to the cache's `ShardedStore` and cannot be reused directly.)

use std::path::Path;

use serde::{Deserialize, Serialize};
use tokio::fs;

use calod_core::store::error::PersistenceError;
use calod_storage::snapshot::{SnapshotFileEntry, SnapshotWalWatermark};

use crate::model::{CheckpointRecord, EffectRecord, EventRecord, RunRecord};

/// Manifest file name (same convention as `calod-storage` snapshots).
pub const MANIFEST_FILE_NAME: &str = "manifest.json";
/// State file name inside the snapshot directory.
pub const STATE_FILE_NAME: &str = "ledger-state.json";
/// On-disk snapshot format version.
pub const SNAPSHOT_FORMAT_VERSION: u32 = 1;

/// The full in-memory ledger state as persisted in a snapshot.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct PersistedState {
    pub schema_version: u32,
    pub runs: Vec<RunRecord>,
    pub effects: Vec<EffectRecord>,
    pub events: Vec<EventRecord>,
    pub checkpoints: Vec<CheckpointRecord>,
}

/// The snapshot manifest, written after the state file is durable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerSnapshotManifest {
    /// Snapshot format version ([`SNAPSHOT_FORMAT_VERSION`]).
    pub version: u32,
    /// Unix time (ms) when the snapshot was taken.
    pub created_at_ms: i64,
    /// WAL position up to which the snapshot is complete; recovery replays
    /// only records after it.
    pub wal_watermark: Option<SnapshotWalWatermark>,
    /// Every snapshot file with its checksum.
    pub files: Vec<SnapshotFileEntry>,
}

/// Save `state` to `dir`: write `ledger-state.json`, then the manifest with
/// its CRC32C and the WAL watermark.
pub(crate) async fn save_snapshot(
    dir: &Path,
    state: &PersistedState,
    watermark: Option<SnapshotWalWatermark>,
    created_at_ms: i64,
) -> Result<LedgerSnapshotManifest, PersistenceError> {
    fs::create_dir_all(dir)
        .await
        .map_err(PersistenceError::Io)?;

    let encoded = serde_json::to_vec(state).map_err(|err| {
        PersistenceError::SnapshotCreationFailed(format!("ledger state encode: {err}"))
    })?;
    let state_path = dir.join(STATE_FILE_NAME);
    {
        use tokio::io::AsyncWriteExt;
        // Open for writing and keep the handle: on Windows, sync_all on a
        // read-only handle fails with ERROR_ACCESS_DENIED.
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&state_path)
            .await
            .map_err(PersistenceError::Io)?;
        file.write_all(&encoded)
            .await
            .map_err(PersistenceError::Io)?;
        file.sync_all().await.map_err(PersistenceError::Io)?;
    }
    // Manifest is written only after the state file is durable, so a crash
    // mid-save leaves a file without a manifest — which load refuses.
    let manifest = LedgerSnapshotManifest {
        version: SNAPSHOT_FORMAT_VERSION,
        created_at_ms,
        wal_watermark: watermark,
        files: vec![SnapshotFileEntry {
            path: STATE_FILE_NAME.to_string(),
            len: encoded.len() as u64,
            crc32c: crc32c::crc32c(&encoded),
        }],
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(|err| {
        PersistenceError::SnapshotCreationFailed(format!("manifest encode: {err}"))
    })?;
    fs::write(dir.join(MANIFEST_FILE_NAME), manifest_bytes)
        .await
        .map_err(PersistenceError::Io)?;
    Ok(manifest)
}

/// Load and verify a snapshot from `dir`.
///
/// Returns `Ok(None)` when no snapshot exists. Refuses (fail-closed) when a
/// state file exists without a manifest, the manifest version is unknown, or
/// the state file fails length/CRC32C verification.
pub(crate) async fn load_snapshot(
    dir: &Path,
) -> Result<Option<(PersistedState, LedgerSnapshotManifest)>, PersistenceError> {
    let manifest_path = dir.join(MANIFEST_FILE_NAME);
    let raw = match fs::read(&manifest_path).await {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let state_exists = matches!(fs::try_exists(dir.join(STATE_FILE_NAME)).await, Ok(true));
            if state_exists {
                return Err(PersistenceError::Corruption(format!(
                    "ledger snapshot state exists in {} but {MANIFEST_FILE_NAME} is missing; \
                     refusing to apply an unverifiable snapshot",
                    dir.display()
                )));
            }
            return Ok(None);
        }
        Err(err) => return Err(PersistenceError::Io(err)),
    };
    let manifest: LedgerSnapshotManifest = serde_json::from_slice(&raw).map_err(|err| {
        PersistenceError::Corruption(format!(
            "invalid ledger snapshot manifest {}: {err}",
            manifest_path.display()
        ))
    })?;
    if manifest.version != SNAPSHOT_FORMAT_VERSION {
        return Err(PersistenceError::Corruption(format!(
            "unsupported ledger snapshot version {} (expected {SNAPSHOT_FORMAT_VERSION})",
            manifest.version
        )));
    }

    // Verify every listed file (presence, length, CRC32C) before applying.
    let mut state: Option<PersistedState> = None;
    for entry in &manifest.files {
        if entry.path != STATE_FILE_NAME {
            return Err(PersistenceError::Corruption(format!(
                "ledger snapshot manifest lists unknown file {}",
                entry.path
            )));
        }
        let data = fs::read(dir.join(&entry.path)).await.map_err(|err| {
            PersistenceError::Corruption(format!(
                "ledger snapshot file {} is unreadable: {err}",
                entry.path
            ))
        })?;
        if data.len() as u64 != entry.len || crc32c::crc32c(&data) != entry.crc32c {
            return Err(PersistenceError::Corruption(format!(
                "ledger snapshot file {} failed checksum verification",
                entry.path
            )));
        }
        let parsed: PersistedState = serde_json::from_slice(&data).map_err(|err| {
            PersistenceError::Corruption(format!("undecodable ledger snapshot state: {err}"))
        })?;
        state = Some(parsed);
    }

    match state {
        Some(state) => Ok(Some((state, manifest))),
        None => Err(PersistenceError::Corruption(format!(
            "ledger snapshot manifest in {} lists no state file",
            dir.display()
        ))),
    }
}
