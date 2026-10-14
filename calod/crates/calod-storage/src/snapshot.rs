//! Checksummed, manifest-backed snapshots covering every logical database.
//!
//! A snapshot on disk is a set of per-database directories (`db_0/`, `db_3/`,
//! … — one per non-empty logical DB, each holding that DB's `shard_N.bin`
//! files as written by [`CachePersistence`]) plus a `manifest.json` written
//! **last**:
//!
//! ```json
//! {
//!   "version": 1,
//!   "created_at": 1760000000,
//!   "shard_count": 16,
//!   "wal_watermark": { "segment_id": 3, "offset": 1048576 },
//!   "files": [
//!     { "path": "db_0/shard_0.bin", "len": 1234, "crc32c": 2974676131 }
//!   ]
//! }
//! ```
//!
//! `wal_watermark` is the position of the last WAL record the snapshot is
//! guaranteed to cover; recovery replays only records after it (and the
//! caller may `truncate_before` its segment once the manifest is durable).
//!
//! # Failure policy (fail-closed)
//!
//! - Snapshot-looking files (`db_*` directories or top-level `shard_*.bin`)
//!   without a readable manifest are treated as corruption — a crash
//!   mid-save or a legacy pre-manifest snapshot must never be applied
//!   silently.
//! - Every file listed in the manifest is read and checked (length +
//!   CRC32C) before anything is applied; any mismatch is
//!   [`PersistenceError::Corruption`].
//! - A DB directory listing a number of shard files different from
//!   `shard_count`, or referencing a DB index the server does not have, is
//!   corruption as well.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::fs;

use calod_core::store::{
    calod_data::DataType, error::PersistenceError, sharded_store::ShardedStore,
};

use crate::persistence::CachePersistence;

/// Name of the manifest file inside the snapshot directory.
pub const MANIFEST_FILE_NAME: &str = "manifest.json";
/// On-disk snapshot format version written into the manifest.
pub const SNAPSHOT_FORMAT_VERSION: u32 = 1;
/// Prefix of the per-database directories inside the snapshot directory.
const DB_DIR_PREFIX: &str = "db_";

/// One checksummed file belonging to a snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotFileEntry {
    /// Path relative to the snapshot directory (e.g. `db_0/shard_0.bin`).
    pub path: String,
    /// File length in bytes.
    pub len: u64,
    /// CRC32C of the whole file.
    pub crc32c: u32,
}

/// WAL position (segment id + header offset of the record) up to which a
/// snapshot is complete; recovery skips WAL records at or before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotWalWatermark {
    /// WAL segment file id.
    pub segment_id: u64,
    /// Byte offset of the record header within the segment.
    pub offset: u64,
}

/// The snapshot manifest, written after all shard files are durable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotManifest {
    /// Snapshot format version ([`SNAPSHOT_FORMAT_VERSION`]).
    pub version: u32,
    /// Unix time (seconds) when the snapshot was taken.
    pub created_at: u64,
    /// Number of shards per stored database (each `db_*` directory must
    /// hold exactly this many `shard_N.bin` files).
    pub shard_count: usize,
    /// WAL watermark covered by this snapshot, when the WAL is enabled.
    pub wal_watermark: Option<SnapshotWalWatermark>,
    /// Every snapshot file with its checksum.
    pub files: Vec<SnapshotFileEntry>,
}

fn db_dir_name(db: usize) -> String {
    format!("{DB_DIR_PREFIX}{db}")
}

fn parse_db_dir_name(name: &str) -> Option<usize> {
    let rest = name.strip_prefix(DB_DIR_PREFIX)?;
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    rest.parse().ok()
}

/// Remove the previous snapshot state in `dir`: the manifest, every `db_*`
/// directory, and any legacy top-level `shard_*.bin` files. Missing entries
/// are ignored; anything else in the directory is left alone.
async fn clear_previous_snapshot(dir: &Path) -> Result<(), PersistenceError> {
    let manifest = dir.join(MANIFEST_FILE_NAME);
    match fs::remove_file(&manifest).await {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(PersistenceError::Io(err)),
    }
    let mut read_dir = match fs::read_dir(dir).await {
        Ok(read_dir) => read_dir,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(PersistenceError::Io(err)),
    };
    while let Some(entry) = read_dir.next_entry().await.map_err(PersistenceError::Io)? {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if parse_db_dir_name(name).is_some() && entry.path().is_dir() {
            fs::remove_dir_all(entry.path())
                .await
                .map_err(PersistenceError::Io)?;
        } else if name.starts_with("shard_") && name.ends_with(".bin") {
            match fs::remove_file(entry.path()).await {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(PersistenceError::Io(err)),
            }
        }
    }
    Ok(())
}

/// Whether a directory contains anything that looks like snapshot data
/// (a `db_*` directory or a top-level `shard_*.bin` file).
async fn dir_has_snapshot_files(dir: &Path) -> Result<bool, PersistenceError> {
    let mut read_dir = match fs::read_dir(dir).await {
        Ok(read_dir) => read_dir,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(PersistenceError::Io(err)),
    };
    while let Some(entry) = read_dir.next_entry().await.map_err(PersistenceError::Io)? {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if parse_db_dir_name(name).is_some()
            || (name.starts_with("shard_") && name.ends_with(".bin"))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn store_is_empty(store: &Arc<ShardedStore>) -> bool {
    matches!(store.dbsize_cmd().await, Ok(DataType::Integer(0)))
}

/// Save every non-empty logical database to `dir` (as `db_N/` directories),
/// then write a checksummed `manifest.json` recording `watermark`.
///
/// The previous snapshot state in `dir` is cleared first; the manifest is
/// written last, so a crash mid-save leaves "files without a manifest",
/// which [`load_verified`] refuses (fail-closed) rather than applying a
/// torn snapshot.
///
/// Note: like the underlying per-shard save, this does not quiesce writers;
/// post-watermark WAL records replayed on recovery restore any mutations
/// the snapshot did not capture.
pub async fn save_verified(
    dbs: &[Arc<ShardedStore>],
    dir: &Path,
    watermark: Option<SnapshotWalWatermark>,
    created_at: u64,
) -> Result<SnapshotManifest, PersistenceError> {
    fs::create_dir_all(dir)
        .await
        .map_err(PersistenceError::Io)?;
    clear_previous_snapshot(dir).await?;

    let shard_count = dbs.first().map(|db| db.get_all_shards().len()).unwrap_or(0);
    let mut files = Vec::new();

    for (index, db) in dbs.iter().enumerate() {
        if store_is_empty(db).await {
            continue;
        }
        let db_dir = dir.join(db_dir_name(index));
        let db_dir_str = db_dir.to_str().ok_or_else(|| {
            PersistenceError::SnapshotCreationFailed(format!(
                "snapshot directory path is not valid UTF-8: {}",
                db_dir.display()
            ))
        })?;
        db.save(db_dir_str).await?;
        for shard in 0..shard_count {
            let rel = format!("{}/shard_{shard}.bin", db_dir_name(index));
            let data = fs::read(dir.join(&rel))
                .await
                .map_err(PersistenceError::Io)?;
            files.push(SnapshotFileEntry {
                path: rel,
                len: data.len() as u64,
                crc32c: crc32c::crc32c(&data),
            });
        }
    }

    let manifest = SnapshotManifest {
        version: SNAPSHOT_FORMAT_VERSION,
        created_at,
        shard_count,
        wal_watermark: watermark,
        files,
    };
    let encoded = serde_json::to_vec_pretty(&manifest).map_err(|err| {
        PersistenceError::SnapshotCreationFailed(format!("manifest encode: {err}"))
    })?;
    fs::write(dir.join(MANIFEST_FILE_NAME), encoded)
        .await
        .map_err(PersistenceError::Io)?;
    Ok(manifest)
}

/// Load a verified snapshot from `dir` into `dbs`.
///
/// Returns `Ok(None)` when no snapshot exists (the directory is absent or
/// holds no snapshot data). Otherwise every manifest entry is verified
/// (presence, length, CRC32C) *before* any database is loaded, and the
/// applied manifest — including its WAL watermark — is returned.
pub async fn load_verified(
    dbs: &[Arc<ShardedStore>],
    dir: &Path,
) -> Result<Option<SnapshotManifest>, PersistenceError> {
    let manifest_path = dir.join(MANIFEST_FILE_NAME);
    let raw = match fs::read(&manifest_path).await {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if dir_has_snapshot_files(dir).await? {
                return Err(PersistenceError::Corruption(format!(
                    "snapshot files exist in {} but {MANIFEST_FILE_NAME} is missing; \
                     refusing to apply an unverifiable snapshot",
                    dir.display()
                )));
            }
            return Ok(None);
        }
        Err(err) => return Err(PersistenceError::Io(err)),
    };
    let manifest: SnapshotManifest = serde_json::from_slice(&raw).map_err(|err| {
        PersistenceError::Corruption(format!(
            "invalid snapshot manifest {}: {err}",
            manifest_path.display()
        ))
    })?;
    if manifest.version != SNAPSHOT_FORMAT_VERSION {
        return Err(PersistenceError::Corruption(format!(
            "unsupported snapshot format version {} (expected {SNAPSHOT_FORMAT_VERSION})",
            manifest.version
        )));
    }

    // Verify every listed file (presence, length, CRC32C) before applying.
    let mut by_db: BTreeMap<usize, Vec<&SnapshotFileEntry>> = BTreeMap::new();
    for entry in &manifest.files {
        let data = fs::read(dir.join(&entry.path)).await.map_err(|err| {
            PersistenceError::Corruption(format!(
                "snapshot file {} listed in manifest is unreadable: {err}",
                entry.path
            ))
        })?;
        if data.len() as u64 != entry.len || crc32c::crc32c(&data) != entry.crc32c {
            return Err(PersistenceError::Corruption(format!(
                "snapshot file {} failed checksum verification",
                entry.path
            )));
        }
        let db_name = entry.path.split('/').next().unwrap_or_default();
        let Some(db_index) = parse_db_dir_name(db_name) else {
            return Err(PersistenceError::Corruption(format!(
                "snapshot manifest lists file outside a {DB_DIR_PREFIX}N directory: {}",
                entry.path
            )));
        };
        by_db.entry(db_index).or_default().push(entry);
    }

    for (db_index, entries) in &by_db {
        if *db_index >= dbs.len() {
            return Err(PersistenceError::Corruption(format!(
                "snapshot manifest covers database {db_index} but only {} databases exist",
                dbs.len()
            )));
        }
        if entries.len() != manifest.shard_count {
            return Err(PersistenceError::Corruption(format!(
                "snapshot database {db_index} lists {} shard files, expected {}",
                entries.len(),
                manifest.shard_count
            )));
        }
    }

    for db_index in by_db.keys() {
        let db_dir = dir.join(db_dir_name(*db_index));
        let db_dir_str = db_dir.to_str().ok_or_else(|| {
            PersistenceError::LoadFailed(format!(
                "snapshot directory path is not valid UTF-8: {}",
                db_dir.display()
            ))
        })?;
        dbs[*db_index].load(db_dir_str).await?;
    }

    Ok(Some(manifest))
}
