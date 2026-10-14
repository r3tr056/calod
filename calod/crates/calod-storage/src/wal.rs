//! Segmented, CRC32C-checksummed write-ahead log (WAL).
//!
//! This module is the production durability machinery: command mutations are
//! appended here (gated by the `enable_aof` persistence setting) and replayed
//! on startup after the latest verified snapshot.
//!
//! # Record format
//!
//! Every record is binary-safe by construction: the payload is an opaque byte
//! string carried with an explicit length, never parsed.
//!
//! ```text
//! +--------+---------+------+--------+---------+==================+
//! | magic  | version | type | length | crc32c  | payload          |
//! | 2 B    | u16 LE  | u8   | u32 LE | u32 LE  | `length` bytes   |
//! +--------+---------+------+--------+---------+==================+
//! | "CW"   | 1       | 0/1  | bytes           | over header[0..9] |
//! |                                           | + payload        |
//! +-------------------------------------------+------------------+
//! ```
//!
//! Header size is [`HEADER_LEN`] (13 bytes). Record type `0` is a user
//! payload (`put`), `1` is a checkpoint marker; all other values are
//! reserved and rejected on read.
//!
//! # Segments
//!
//! Records are appended to segment files named `wal-00000001.log`,
//! `wal-00000002.log`, ... in a configurable directory. A new segment is
//! started once the active segment would exceed
//! [`WalConfig::max_segment_size`] (default 64 MiB). Segment ids are
//! monotonically increasing; recovery reads them in id order.
//!
//! # Durability classes (group commit)
//!
//! The writer is a single background task fed by an unbounded channel. It
//! drains all queued messages, writes the whole batch, performs at most one
//! fsync for the batch, and only then resolves the callers' futures — one
//! fsync amortized over many appends:
//!
//! - [`FsyncStrategy::Always`]: an `append` future resolves after its record
//!   has been written **and** fsynced.
//! - [`FsyncStrategy::Everysec`]: an `append` future resolves once the record
//!   has been written to the file (OS buffer); fsyncs are batched on a 1s
//!   tick (and when a segment rolls).
//! - [`FsyncStrategy::No`]: like `Everysec`, but fsyncs happen only on
//!   demand via [`WalWriter::flush`].
//!
//! [`WalWriter::flush`] always writes and fsyncs everything queued so far,
//! regardless of the durability class.
//!
//! # Recovery and corruption policy (fail-closed)
//!
//! [`recover`] / [`Replay`] tolerate corruption in exactly one case: the
//! final record of the final segment is *incomplete* (a short header, or a
//! declared length that runs past end-of-file) — the provable tail of a
//! crash mid-write. In that case a warning is logged, the segment is
//! truncated to the last good offset, and replay continues. Any other
//! corruption — bad magic, unknown version or record type, CRC mismatch, or
//! a truncated record in a non-final segment — is a hard error
//! ([`PersistenceError::Corruption`]); recovery fails rather than silently
//! skipping data. A CRC mismatch is never treated as a crash tail, even in
//! the final segment: a torn write shows up as a *short* record, so a
//! full-length record with a bad CRC indicates real corruption.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::fs::{create_dir_all, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{self, Duration, MissedTickBehavior};
use tracing::warn;

use calod_core::store::{config::FsyncStrategy, error::PersistenceError};

/// Magic bytes at the start of every record header.
const MAGIC: [u8; 2] = *b"CW";
/// On-disk format version written into every record header.
pub const FORMAT_VERSION: u16 = 1;
/// Size of a record header in bytes: magic (2) + version (2) + type (1) +
/// payload length (4) + CRC32C (4).
pub const HEADER_LEN: usize = 13;
/// Default maximum segment size: 64 MiB.
pub const DEFAULT_MAX_SEGMENT_SIZE: u64 = 64 * 1024 * 1024;

const SEGMENT_PREFIX: &str = "wal-";
const SEGMENT_SUFFIX: &str = ".log";

/// Type of a WAL record. Values other than `Put` and `Checkpoint` are
/// reserved and rejected during recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RecordType {
    /// User payload (opaque bytes).
    Put = 0,
    /// Checkpoint marker; payload is checkpoint-specific metadata.
    Checkpoint = 1,
}

impl RecordType {
    fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Put),
            1 => Some(Self::Checkpoint),
            _ => None,
        }
    }
}

/// Logical position of a record in the WAL: the segment that holds it and
/// the byte offset of its header within that segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalPosition {
    /// Segment file id (the `wal-{id:08}.log` number).
    pub segment_id: u64,
    /// Byte offset of the record header within the segment file.
    pub offset: u64,
}

/// A record yielded by [`Replay`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalRecord {
    /// Segment file id the record was read from.
    pub segment_id: u64,
    /// Byte offset of the record header within the segment file.
    pub offset: u64,
    /// Record type.
    pub record_type: RecordType,
    /// Opaque payload bytes.
    pub payload: Vec<u8>,
}

/// Configuration for a [`WalWriter`].
#[derive(Debug, Clone)]
pub struct WalConfig {
    /// Directory holding the `wal-*.log` segment files.
    pub dir: PathBuf,
    /// Maximum size of a segment before rolling to a new one.
    pub max_segment_size: u64,
    /// Durability class; see the module-level documentation.
    pub fsync_strategy: FsyncStrategy,
}

impl WalConfig {
    /// Configuration with defaults: 64 MiB segments, `everysec` fsync.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            max_segment_size: DEFAULT_MAX_SEGMENT_SIZE,
            fsync_strategy: FsyncStrategy::Everysec,
        }
    }

    /// Override the maximum segment size in bytes.
    pub fn with_max_segment_size(mut self, max_segment_size: u64) -> Self {
        self.max_segment_size = max_segment_size;
        self
    }

    /// Override the fsync strategy.
    pub fn with_fsync_strategy(mut self, fsync_strategy: FsyncStrategy) -> Self {
        self.fsync_strategy = fsync_strategy;
        self
    }
}

/// Metadata about one segment file discovered during recovery.
#[derive(Debug, Clone)]
pub struct SegmentInfo {
    /// Segment file id.
    pub id: u64,
    /// Full path of the segment file.
    pub path: PathBuf,
    /// Number of valid bytes (after any crash-tail truncation).
    pub valid_len: u64,
}

/// Description of a crash-tail truncation applied during recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TailTruncation {
    /// Segment that was truncated (always the final segment).
    pub segment_id: u64,
    /// File size before truncation.
    pub original_len: u64,
    /// File size after truncation (offset of the first bad byte).
    pub valid_len: u64,
}

/// Outcome of [`recover`].
#[derive(Debug, Clone, Default)]
pub struct RecoveryReport {
    /// All segment files found, in id order.
    pub segments: Vec<SegmentInfo>,
    /// Set when the final segment had a crash tail that was truncated away.
    pub truncated: Option<TailTruncation>,
    /// Position of the last valid record, if any.
    pub last_position: Option<WalPosition>,
}

fn segment_path(dir: &Path, segment_id: u64) -> PathBuf {
    dir.join(format!("{SEGMENT_PREFIX}{segment_id:08}{SEGMENT_SUFFIX}"))
}

fn parse_segment_file_name(name: &str) -> Option<u64> {
    let stem = name
        .strip_prefix(SEGMENT_PREFIX)?
        .strip_suffix(SEGMENT_SUFFIX)?;
    if stem.is_empty() || !stem.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    stem.parse().ok()
}

/// List segment files in `dir` as `(segment_id, path)` pairs, sorted by id.
/// Non-matching file names are ignored. A missing directory yields an empty
/// list.
fn list_segments(dir: &Path) -> Result<Vec<(u64, PathBuf)>, PersistenceError> {
    let read_dir = match fs::read_dir(dir) {
        Ok(read_dir) => read_dir,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(PersistenceError::Io(err)),
    };
    let mut segments = Vec::new();
    for entry in read_dir {
        let entry = entry.map_err(PersistenceError::Io)?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some(id) = parse_segment_file_name(name) {
            segments.push((id, entry.path()));
        }
    }
    segments.sort_by_key(|(id, _)| *id);
    Ok(segments)
}

/// Why a record at some offset could not be parsed.
enum ScanError {
    /// The record starting at `valid_len` is incomplete (short header or a
    /// declared length running past EOF): a provable crash tail.
    Truncated { valid_len: u64 },
    /// The record starting at `offset` is corrupt.
    Corrupt { offset: u64, reason: String },
}

struct ParsedRecord<'a> {
    record_type: RecordType,
    payload: &'a [u8],
    next_offset: usize,
}

/// Parse and fully validate (magic, version, type, length, CRC32C) the
/// record starting at `offset` in `buf`.
fn parse_one(buf: &[u8], offset: usize) -> Result<ParsedRecord<'_>, ScanError> {
    let remaining = buf.len() - offset;
    if remaining < HEADER_LEN {
        return Err(ScanError::Truncated {
            valid_len: offset as u64,
        });
    }
    let header = &buf[offset..offset + HEADER_LEN];
    if header[0..2] != MAGIC {
        return Err(ScanError::Corrupt {
            offset: offset as u64,
            reason: "bad magic".to_string(),
        });
    }
    let version = u16::from_le_bytes([header[2], header[3]]);
    if version != FORMAT_VERSION {
        return Err(ScanError::Corrupt {
            offset: offset as u64,
            reason: format!("unsupported format version {version}"),
        });
    }
    let Some(record_type) = RecordType::from_u8(header[4]) else {
        return Err(ScanError::Corrupt {
            offset: offset as u64,
            reason: format!("unknown record type {}", header[4]),
        });
    };
    let payload_len = u32::from_le_bytes([header[5], header[6], header[7], header[8]]) as u64;
    let end = offset as u64 + HEADER_LEN as u64 + payload_len;
    if end > buf.len() as u64 {
        return Err(ScanError::Truncated {
            valid_len: offset as u64,
        });
    }
    let end = end as usize;
    let payload = &buf[offset + HEADER_LEN..end];
    let expected_crc = u32::from_le_bytes([header[9], header[10], header[11], header[12]]);
    let actual_crc = crc32c::crc32c_append(crc32c::crc32c(&header[..9]), payload);
    if actual_crc != expected_crc {
        return Err(ScanError::Corrupt {
            offset: offset as u64,
            reason: "CRC32C mismatch".to_string(),
        });
    }
    Ok(ParsedRecord {
        record_type,
        payload,
        next_offset: end,
    })
}

/// Scan a whole segment buffer, invoking `on_record(offset, type, payload)`
/// for every valid record in order.
fn scan_segment(
    buf: &[u8],
    mut on_record: impl FnMut(u64, RecordType, &[u8]),
) -> Result<(), ScanError> {
    let mut offset = 0usize;
    while offset < buf.len() {
        let parsed = parse_one(buf, offset)?;
        on_record(offset as u64, parsed.record_type, parsed.payload);
        offset = parsed.next_offset;
    }
    Ok(())
}

/// Validate all segment files in `dir`, applying the fail-closed corruption
/// policy described in the module documentation. If the final segment ends
/// in an incomplete record (crash tail), the file is truncated to the last
/// good offset and a warning is logged.
///
/// This performs blocking filesystem I/O; it is intended for startup
/// recovery, not the hot path.
pub fn recover(dir: &Path) -> Result<RecoveryReport, PersistenceError> {
    let segments = list_segments(dir)?;
    let mut report = RecoveryReport::default();
    let last_idx = segments.len().saturating_sub(1);

    for (idx, (segment_id, path)) in segments.into_iter().enumerate() {
        let data = fs::read(&path).map_err(PersistenceError::Io)?;
        let original_len = data.len() as u64;
        let is_final = idx == last_idx;
        let mut last_offset: Option<u64> = None;
        let scan = scan_segment(&data, |offset, _, _| last_offset = Some(offset));
        let valid_len = match scan {
            Ok(()) => original_len,
            Err(ScanError::Truncated { valid_len }) if is_final => {
                warn!(
                    segment_id,
                    original_len,
                    valid_len,
                    "WAL: truncating incomplete crash tail in final segment"
                );
                let file = fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .map_err(PersistenceError::Io)?;
                file.set_len(valid_len).map_err(PersistenceError::Io)?;
                report.truncated = Some(TailTruncation {
                    segment_id,
                    original_len,
                    valid_len,
                });
                valid_len
            }
            Err(ScanError::Truncated { valid_len }) => {
                return Err(PersistenceError::Corruption(format!(
                    "WAL segment {segment_id} ends in a truncated record at offset {valid_len} \
                     but is not the final segment"
                )));
            }
            Err(ScanError::Corrupt { offset, reason }) => {
                return Err(PersistenceError::Corruption(format!(
                    "WAL segment {segment_id} has a corrupt record at offset {offset}: {reason}"
                )));
            }
        };
        if let Some(offset) = last_offset {
            report.last_position = Some(WalPosition { segment_id, offset });
        }
        report.segments.push(SegmentInfo {
            id: segment_id,
            path,
            valid_len,
        });
    }
    Ok(report)
}

/// Replay iterator over all records of a WAL directory, in write order
/// (segment id order, then offset order). Opening runs [`recover`] first,
/// so the crash-tail policy is applied and any hard corruption surfaces as
/// an error from [`Replay::open`].
///
/// Performs blocking filesystem I/O; intended for startup recovery.
pub struct Replay {
    segments: Vec<(u64, PathBuf)>,
    report: RecoveryReport,
    next_segment: usize,
    current_segment: u64,
    buf: Vec<u8>,
    offset: usize,
    finished: bool,
}

impl Replay {
    /// Open a replay iterator over `dir`, recovering (and, if needed,
    /// truncating the crash tail of the final segment) first.
    pub fn open(dir: &Path) -> Result<Self, PersistenceError> {
        let report = recover(dir)?;
        let segments = report
            .segments
            .iter()
            .map(|info| (info.id, info.path.clone()))
            .collect();
        Ok(Self {
            segments,
            report,
            next_segment: 0,
            current_segment: 0,
            buf: Vec::new(),
            offset: 0,
            finished: false,
        })
    }

    /// The recovery report produced when this iterator was opened.
    pub fn report(&self) -> &RecoveryReport {
        &self.report
    }
}

impl Iterator for Replay {
    type Item = Result<WalRecord, PersistenceError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        loop {
            if self.offset < self.buf.len() {
                // recover() already validated this segment; a parse failure
                // here means the file changed underneath us — fail closed.
                match parse_one(&self.buf, self.offset) {
                    Ok(parsed) => {
                        let record = WalRecord {
                            segment_id: self.current_segment,
                            offset: self.offset as u64,
                            record_type: parsed.record_type,
                            payload: parsed.payload.to_vec(),
                        };
                        self.offset = parsed.next_offset;
                        return Some(Ok(record));
                    }
                    Err(ScanError::Truncated { valid_len }) => {
                        self.finished = true;
                        return Some(Err(PersistenceError::Corruption(format!(
                            "WAL segment {} has a truncated record at offset {valid_len} \
                             after recovery",
                            self.current_segment
                        ))));
                    }
                    Err(ScanError::Corrupt { offset, reason }) => {
                        self.finished = true;
                        return Some(Err(PersistenceError::Corruption(format!(
                            "WAL segment {} has a corrupt record at offset {offset}: {reason}",
                            self.current_segment
                        ))));
                    }
                }
            }
            if self.next_segment >= self.segments.len() {
                self.finished = true;
                return None;
            }
            let (segment_id, path) = self.segments[self.next_segment].clone();
            self.next_segment += 1;
            match fs::read(&path) {
                Ok(data) => {
                    self.current_segment = segment_id;
                    self.buf = data;
                    self.offset = 0;
                }
                Err(err) => {
                    self.finished = true;
                    return Some(Err(PersistenceError::Io(err)));
                }
            }
        }
    }
}

/// Convenience helper: replay the whole WAL directory into a vector.
pub fn replay_all(dir: &Path) -> Result<Vec<WalRecord>, PersistenceError> {
    Replay::open(dir)?.collect()
}

/// Serialize one record (header + CRC32C + payload) onto `out`.
fn encode_record(record_type: RecordType, payload: &[u8], out: &mut Vec<u8>) {
    let start = out.len();
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    out.push(record_type as u8);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    let crc = crc32c::crc32c_append(crc32c::crc32c(&out[start..start + 9]), payload);
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(payload);
}

enum WalMessage {
    Append {
        record_type: RecordType,
        payload: Vec<u8>,
        ack: oneshot::Sender<Result<WalPosition, PersistenceError>>,
    },
    Flush(oneshot::Sender<Result<(), PersistenceError>>),
    Shutdown(oneshot::Sender<Result<(), PersistenceError>>),
}

/// Waiters collected while draining one batch of messages.
#[derive(Default)]
struct Batch {
    acks: Vec<(
        oneshot::Sender<Result<WalPosition, PersistenceError>>,
        WalPosition,
    )>,
    flushers: Vec<oneshot::Sender<Result<(), PersistenceError>>>,
    shutdown: Option<oneshot::Sender<Result<(), PersistenceError>>>,
}

impl Batch {
    fn fail_all(self, message: String) {
        for (ack, _) in self.acks {
            let _ = ack.send(Err(PersistenceError::WalWriteFailed(message.clone())));
        }
        for flusher in self.flushers {
            let _ = flusher.send(Err(PersistenceError::WalWriteFailed(message.clone())));
        }
        if let Some(shutdown) = self.shutdown {
            let _ = shutdown.send(Err(PersistenceError::WalWriteFailed(message)));
        }
    }
}

struct WriterState {
    file: tokio::fs::File,
    dir: PathBuf,
    segment_id: u64,
    segment_len: u64,
    max_segment_size: u64,
    fsync_strategy: FsyncStrategy,
    dirty: bool,
    active_segment: Arc<AtomicU64>,
}

impl WriterState {
    /// Roll to the next segment. The old segment is fsynced before sealing
    /// (except under `FsyncStrategy::No`).
    async fn roll_segment(&mut self) -> Result<(), PersistenceError> {
        if self.dirty {
            flush_file(
                &mut self.file,
                !matches!(self.fsync_strategy, FsyncStrategy::No),
            )
            .await?;
            self.dirty = false;
        }
        self.segment_id += 1;
        let path = segment_path(&self.dir, self.segment_id);
        self.file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)
            .await
            .map_err(PersistenceError::Io)?;
        self.segment_len = 0;
        self.active_segment
            .store(self.segment_id, Ordering::Release);
        Ok(())
    }
}

async fn flush_file(file: &mut tokio::fs::File, sync_data: bool) -> Result<(), PersistenceError> {
    file.flush().await.map_err(PersistenceError::Io)?;
    if sync_data {
        file.sync_data().await.map_err(PersistenceError::Io)?;
    }
    Ok(())
}

async fn handle_message(state: &mut WriterState, message: WalMessage, batch: &mut Batch) {
    match message {
        WalMessage::Append {
            record_type,
            payload,
            ack,
        } => {
            let result = handle_append(state, record_type, &payload).await;
            match result {
                Ok(position) => batch.acks.push((ack, position)),
                Err(err) => {
                    let _ = ack.send(Err(err));
                }
            }
        }
        WalMessage::Flush(waiter) => batch.flushers.push(waiter),
        WalMessage::Shutdown(waiter) => batch.shutdown = Some(waiter),
    }
}

async fn handle_append(
    state: &mut WriterState,
    record_type: RecordType,
    payload: &[u8],
) -> Result<WalPosition, PersistenceError> {
    let mut encoded = Vec::with_capacity(HEADER_LEN + payload.len());
    encode_record(record_type, payload, &mut encoded);
    let record_len = encoded.len() as u64;
    // A record larger than the segment limit gets a segment of its own;
    // never roll away from an empty segment.
    if state.segment_len > 0 && state.segment_len + record_len > state.max_segment_size {
        state.roll_segment().await?;
    }
    let position = WalPosition {
        segment_id: state.segment_id,
        offset: state.segment_len,
    };
    state
        .file
        .write_all(&encoded)
        .await
        .map_err(PersistenceError::Io)?;
    state.segment_len += record_len;
    state.dirty = true;
    Ok(position)
}

async fn run_writer(
    mut state: WriterState,
    mut rx: mpsc::UnboundedReceiver<WalMessage>,
) -> Result<(), PersistenceError> {
    let mut everysec_tick = time::interval(Duration::from_secs(1));
    everysec_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            maybe_message = rx.recv() => {
                let Some(first) = maybe_message else {
                    // All handles dropped without shutdown(): flush what we
                    // have and exit (Drop also aborts this task; this path is
                    // a best-effort graceful drain).
                    if state.dirty {
                        flush_file(&mut state.file, !matches!(state.fsync_strategy, FsyncStrategy::No)).await?;
                    }
                    return Ok(());
                };

                let mut batch = Batch::default();
                handle_message(&mut state, first, &mut batch).await;
                while let Ok(message) = rx.try_recv() {
                    handle_message(&mut state, message, &mut batch).await;
                }

                // Group commit: at most one fsync for the whole batch.
                let must_sync = state.dirty
                    && (matches!(state.fsync_strategy, FsyncStrategy::Always)
                        || !batch.flushers.is_empty()
                        || batch.shutdown.is_some());
                if must_sync {
                    if let Err(err) = flush_file(&mut state.file, true).await {
                        batch.fail_all(err.to_string());
                        return Err(err);
                    }
                    state.dirty = false;
                }

                for (ack, position) in std::mem::take(&mut batch.acks) {
                    let _ = ack.send(Ok(position));
                }
                for flusher in batch.flushers.drain(..) {
                    let _ = flusher.send(Ok(()));
                }
                if let Some(shutdown) = batch.shutdown.take() {
                    let _ = shutdown.send(Ok(()));
                    return Ok(());
                }
            }
            _ = everysec_tick.tick(), if matches!(state.fsync_strategy, FsyncStrategy::Everysec) && state.dirty => {
                flush_file(&mut state.file, true).await?;
                state.dirty = false;
            }
        }
    }
}

/// Asynchronous WAL writer with group commit.
///
/// Cheap to clone; all clones share the same underlying writer task.
///
/// **Shutdown semantics:** use [`WalWriter::shutdown`] for a graceful close
/// (drain + fsync). Dropping the handle aborts the writer task *without*
/// flushing, which deliberately mimics a `kill -9`: records already
/// acknowledged according to the configured [`FsyncStrategy`] survive (they
/// are in the OS page cache at worst, durable at best); unacknowledged
/// records may be lost or leave a recoverable crash tail.
pub struct WalWriter {
    tx: mpsc::UnboundedSender<WalMessage>,
    task: Option<JoinHandle<Result<(), PersistenceError>>>,
    dir: PathBuf,
    active_segment: Arc<AtomicU64>,
}

impl WalWriter {
    /// Open (or create) the WAL in `config.dir`, recovering existing
    /// segments first: the crash tail of the final segment is truncated per
    /// the module corruption policy, then appends continue in the last
    /// segment (or a fresh one if the last is full).
    pub async fn open(config: WalConfig) -> Result<Self, PersistenceError> {
        if config.max_segment_size <= HEADER_LEN as u64 {
            return Err(PersistenceError::WalWriteFailed(format!(
                "max_segment_size must exceed the record header ({HEADER_LEN} bytes)"
            )));
        }
        create_dir_all(&config.dir)
            .await
            .map_err(PersistenceError::Io)?;
        let report = recover(&config.dir)?;
        let (segment_id, segment_len) = match report.segments.last() {
            Some(info) if info.valid_len < config.max_segment_size => (info.id, info.valid_len),
            Some(info) => (info.id + 1, 0),
            None => (1, 0),
        };
        let path = segment_path(&config.dir, segment_id);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .map_err(PersistenceError::Io)?;
        let active_segment = Arc::new(AtomicU64::new(segment_id));
        let state = WriterState {
            file,
            dir: config.dir.clone(),
            segment_id,
            segment_len,
            max_segment_size: config.max_segment_size,
            fsync_strategy: config.fsync_strategy.clone(),
            dirty: false,
            active_segment: Arc::clone(&active_segment),
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(run_writer(state, rx));
        Ok(Self {
            tx,
            task: Some(task),
            dir: config.dir,
            active_segment,
        })
    }

    /// Append a record. The returned future resolves with the record's
    /// position once it has been *accepted* according to the configured
    /// durability class (written + fsynced for `always`; written to the OS
    /// buffer for `everysec`/`no`).
    pub async fn append(
        &self,
        record_type: RecordType,
        payload: &[u8],
    ) -> Result<WalPosition, PersistenceError> {
        if u64::try_from(payload.len()).unwrap_or(u64::MAX) > u64::from(u32::MAX) {
            return Err(PersistenceError::WalWriteFailed(
                "payload exceeds the 4 GiB record limit".to_string(),
            ));
        }
        let (ack_tx, ack_rx) = oneshot::channel();
        self.tx
            .send(WalMessage::Append {
                record_type,
                payload: payload.to_vec(),
                ack: ack_tx,
            })
            .map_err(|_| PersistenceError::WalWriteFailed("WAL writer task stopped".to_string()))?;
        ack_rx.await.map_err(|_| {
            PersistenceError::WalWriteFailed("WAL append acknowledgment lost".to_string())
        })?
    }

    /// Write and fsync everything queued so far, regardless of the
    /// configured durability class. Used by tests and (later) snapshotting.
    pub async fn flush(&self) -> Result<(), PersistenceError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(WalMessage::Flush(reply_tx))
            .map_err(|_| PersistenceError::WalWriteFailed("WAL writer task stopped".to_string()))?;
        reply_rx.await.map_err(|_| {
            PersistenceError::WalWriteFailed("WAL flush acknowledgment lost".to_string())
        })?
    }

    /// Delete all segment files with an id strictly below `segment_id`
    /// (never the active segment). Retention hook for later compaction:
    /// call with the segment recorded in a checkpoint once the checkpoint is
    /// durable. Returns the number of segments removed.
    pub async fn truncate_before(&self, segment_id: u64) -> Result<u64, PersistenceError> {
        let cutoff = segment_id.min(self.active_segment.load(Ordering::Acquire));
        let mut removed = 0u64;
        for (id, path) in list_segments(&self.dir)? {
            if id < cutoff {
                tokio::fs::remove_file(&path)
                    .await
                    .map_err(PersistenceError::Io)?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Id of the segment currently being appended to.
    pub fn active_segment_id(&self) -> u64 {
        self.active_segment.load(Ordering::Acquire)
    }

    /// Graceful close: drain all queued appends, fsync them, and stop the
    /// writer task.
    pub async fn shutdown(mut self) -> Result<(), PersistenceError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(WalMessage::Shutdown(reply_tx))
            .map_err(|_| PersistenceError::WalWriteFailed("WAL writer task stopped".to_string()))?;
        let result = reply_rx.await.map_err(|_| {
            PersistenceError::WalWriteFailed("WAL shutdown acknowledgment lost".to_string())
        })?;
        if let Some(task) = self.task.take() {
            task.await
                .map_err(|err| PersistenceError::Io(std::io::Error::other(err)))??;
        }
        result
    }
}

impl Drop for WalWriter {
    fn drop(&mut self) {
        // Abort without flushing: simulates a crash (kill -9). Data already
        // written to the file survives in the OS page cache.
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::SmallRng;
    use rand::{Rng, SeedableRng};
    use uuid::Uuid;

    fn temp_dir() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("calod-wal-test-{}", Uuid::new_v4()));
        p
    }

    fn segment_ids(dir: &Path) -> Vec<u64> {
        list_segments(dir)
            .expect("list segments")
            .into_iter()
            .map(|(id, _)| id)
            .collect()
    }

    fn record_span(payload_len: usize) -> u64 {
        (HEADER_LEN + payload_len) as u64
    }

    #[tokio::test]
    async fn append_flush_replay_basic() {
        let dir = temp_dir();
        let writer = WalWriter::open(WalConfig::new(dir.clone()))
            .await
            .expect("open");

        let pos0 = writer
            .append(RecordType::Put, b"hello")
            .await
            .expect("append");
        let pos1 = writer
            .append(RecordType::Checkpoint, b"cp-1")
            .await
            .expect("append");
        assert_eq!(
            pos0,
            WalPosition {
                segment_id: 1,
                offset: 0
            }
        );
        assert_eq!(pos1.offset, record_span(5));
        writer.flush().await.expect("flush");

        let records = replay_all(&dir).expect("replay");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].record_type, RecordType::Put);
        assert_eq!(records[0].payload, b"hello");
        assert_eq!(records[1].record_type, RecordType::Checkpoint);
        assert_eq!(records[1].payload, b"cp-1");

        writer.shutdown().await.expect("shutdown");
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn binary_payloads_round_trip_exactly() {
        let dir = temp_dir();
        let mut rng = SmallRng::seed_from_u64(0x00B1_A455);
        let writer = WalWriter::open(WalConfig::new(dir.clone()))
            .await
            .expect("open");

        let mut payloads: Vec<Vec<u8>> = vec![
            Vec::new(),
            (0u8..=255).collect(),
            vec![0; 1024],
            vec![0xFF; 1024],
            b"\x00\x00CW\x01\xff\xfe\r\n\x00".to_vec(),
        ];
        for _ in 0..32 {
            let len = rng.random_range(0..=4096);
            let mut payload = vec![0u8; len];
            rng.fill(payload.as_mut_slice());
            payloads.push(payload);
        }
        for payload in &payloads {
            writer
                .append(RecordType::Put, payload)
                .await
                .expect("append");
        }
        writer.flush().await.expect("flush");
        writer.shutdown().await.expect("shutdown");

        let records = replay_all(&dir).expect("replay");
        assert_eq!(records.len(), payloads.len());
        for (record, payload) in records.iter().zip(payloads.iter()) {
            assert_eq!(&record.payload, payload);
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn segment_rollover_preserves_write_order() {
        let dir = temp_dir();
        let config = WalConfig::new(dir.clone()).with_max_segment_size(512);
        let writer = WalWriter::open(config).await.expect("open");

        let mut expected = Vec::new();
        for i in 0u32..100 {
            let payload = format!("record-{i:04}-{}", "x".repeat(40)).into_bytes();
            writer
                .append(RecordType::Put, &payload)
                .await
                .expect("append");
            expected.push(payload);
        }
        writer.flush().await.expect("flush");
        writer.shutdown().await.expect("shutdown");

        let ids = segment_ids(&dir);
        assert!(ids.len() > 3, "expected several segments, got {ids:?}");

        let records = replay_all(&dir).expect("replay");
        assert_eq!(records.len(), expected.len());
        for (idx, (record, payload)) in records.iter().zip(expected.iter()).enumerate() {
            assert_eq!(&record.payload, payload, "record {idx} out of order");
        }
        // Offsets and segment ids must be non-decreasing in replay order.
        for pair in records.windows(2) {
            let (a, b) = (&pair[0], &pair[1]);
            assert!(
                (b.segment_id, b.offset) > (a.segment_id, a.offset),
                "positions must strictly increase"
            );
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn truncate_before_deletes_old_segments() {
        let dir = temp_dir();
        let config = WalConfig::new(dir.clone()).with_max_segment_size(256);
        let writer = WalWriter::open(config).await.expect("open");

        for i in 0u32..60 {
            let payload = format!("payload-{i:03}-{}", "y".repeat(20)).into_bytes();
            writer
                .append(RecordType::Put, &payload)
                .await
                .expect("append");
        }
        writer.flush().await.expect("flush");

        let ids = segment_ids(&dir);
        assert!(ids.len() >= 4, "expected >= 4 segments, got {ids:?}");
        let keep_from = ids[ids.len() - 2];
        let removed = writer
            .truncate_before(keep_from)
            .await
            .expect("truncate_before");
        assert_eq!(removed, keep_from - ids[0]);
        assert_eq!(segment_ids(&dir), vec![keep_from, ids[ids.len() - 1]]);

        // Replay now yields only the tail of the stream.
        let records = replay_all(&dir).expect("replay");
        assert!(records.iter().all(|r| r.segment_id >= keep_from));
        assert!(records.len() < 60);
        // The surviving records are a suffix of the original stream.
        let first_payload =
            format!("payload-{:03}-{}", 60 - records.len(), "y".repeat(20)).into_bytes();
        assert_eq!(records[0].payload, first_payload);

        writer.shutdown().await.expect("shutdown");
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn tail_truncation_mid_record_recovers_with_warning() {
        let dir = temp_dir();
        let writer = WalWriter::open(WalConfig::new(dir.clone()))
            .await
            .expect("open");
        let mut expected = Vec::new();
        for i in 0..10 {
            let payload = format!("record-{i}").into_bytes();
            writer
                .append(RecordType::Put, &payload)
                .await
                .expect("append");
            expected.push(payload);
        }
        writer.shutdown().await.expect("shutdown");

        // Cut the last record in half.
        let path = segment_path(&dir, 1);
        let len = fs::metadata(&path).expect("metadata").len();
        let record_len = record_span("record-9".len());
        let file = fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open");
        file.set_len(len - record_len / 2).expect("set_len");
        drop(file);

        let replay = Replay::open(&dir).expect("replay opens after tail truncation");
        let truncation = replay.report().truncated.expect("truncation reported");
        assert_eq!(truncation.segment_id, 1);
        assert_eq!(truncation.original_len, len - record_len / 2);
        assert_eq!(truncation.valid_len, len - record_len);

        let records: Vec<WalRecord> = replay.collect::<Result<_, _>>().expect("records");
        assert_eq!(records.len(), 9);
        for (record, payload) in records.iter().zip(expected.iter()) {
            assert_eq!(&record.payload, payload);
        }
        // The file was physically truncated.
        assert_eq!(
            fs::metadata(&path).expect("metadata").len(),
            len - record_len
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn tail_truncation_partial_header_recovers() {
        let dir = temp_dir();
        let writer = WalWriter::open(WalConfig::new(dir.clone()))
            .await
            .expect("open");
        for i in 0..5 {
            writer
                .append(RecordType::Put, format!("rec-{i}").as_bytes())
                .await
                .expect("append");
        }
        writer.shutdown().await.expect("shutdown");

        // Leave only 5 bytes of the last record's header.
        let path = segment_path(&dir, 1);
        let len = fs::metadata(&path).expect("metadata").len();
        let last_record = record_span("rec-4".len());
        let file = fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open");
        file.set_len(len - last_record + 5).expect("set_len");
        drop(file);

        let records = replay_all(&dir).expect("replay");
        assert_eq!(records.len(), 4);
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn mid_segment_corruption_fails_recovery() {
        let dir = temp_dir();
        let writer = WalWriter::open(WalConfig::new(dir.clone()))
            .await
            .expect("open");
        for i in 0..10 {
            writer
                .append(RecordType::Put, format!("record-{i:02}").as_bytes())
                .await
                .expect("append");
        }
        writer.shutdown().await.expect("shutdown");

        // Flip one byte inside the payload of the third record.
        let rec_len = record_span("record-00".len()) as usize;
        let path = segment_path(&dir, 1);
        let mut data = fs::read(&path).expect("read");
        let byte_at = 2 * rec_len + HEADER_LEN;
        data[byte_at] ^= 0xA5;
        fs::write(&path, &data).expect("write");

        let err = replay_all(&dir).expect_err("corruption must fail recovery");
        assert!(
            matches!(err, PersistenceError::Corruption(_)),
            "expected Corruption, got {err:?}"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn non_final_segment_corruption_fails_recovery() {
        let dir = temp_dir();
        let config = WalConfig::new(dir.clone()).with_max_segment_size(128);
        let writer = WalWriter::open(config).await.expect("open");
        for i in 0..30 {
            writer
                .append(RecordType::Put, format!("record-{i:02}-padding").as_bytes())
                .await
                .expect("append");
        }
        writer.shutdown().await.expect("shutdown");
        assert!(segment_ids(&dir).len() > 2);

        // Corrupt a byte in the FIRST segment (non-final).
        let path = segment_path(&dir, 1);
        let mut data = fs::read(&path).expect("read");
        data[HEADER_LEN] ^= 0xFF;
        fs::write(&path, &data).expect("write");

        let err = replay_all(&dir).expect_err("non-final corruption must fail");
        assert!(matches!(err, PersistenceError::Corruption(_)));
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn bad_magic_fails_recovery() {
        let dir = temp_dir();
        let writer = WalWriter::open(WalConfig::new(dir.clone()))
            .await
            .expect("open");
        for i in 0..4 {
            writer
                .append(RecordType::Put, format!("rec-{i}").as_bytes())
                .await
                .expect("append");
        }
        writer.shutdown().await.expect("shutdown");

        let path = segment_path(&dir, 1);
        let mut data = fs::read(&path).expect("read");
        data[0] = b'X';
        fs::write(&path, &data).expect("write");

        let err = replay_all(&dir).expect_err("bad magic must fail");
        assert!(matches!(err, PersistenceError::Corruption(_)));
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn reopen_after_clean_shutdown_appends_in_order() {
        let dir = temp_dir();
        let config = WalConfig::new(dir.clone());
        {
            let writer = WalWriter::open(config.clone()).await.expect("open");
            for i in 0..5 {
                writer
                    .append(RecordType::Put, format!("first-{i}").as_bytes())
                    .await
                    .expect("append");
            }
            writer.shutdown().await.expect("shutdown");
        }
        {
            let writer = WalWriter::open(config.clone()).await.expect("reopen");
            let pos = writer
                .append(RecordType::Put, b"second-0")
                .await
                .expect("append");
            assert_eq!(pos.segment_id, 1);
            assert!(pos.offset > 0, "append continues after existing records");
            writer.shutdown().await.expect("shutdown");
        }
        let records = replay_all(&dir).expect("replay");
        assert_eq!(records.len(), 6);
        assert_eq!(records[5].payload, b"second-0");
        let _ = fs::remove_dir_all(dir);
    }

    /// Property test: randomized appends with randomized simulated crashes
    /// (drop without flush, task aborted on Drop = kill -9 with surviving OS
    /// page cache). Crashes happen only at quiescent points (all submitted
    /// appends acknowledged), so the recovered stream must equal the
    /// acknowledged stream exactly — an exact prefix, in fact equality.
    #[tokio::test]
    async fn crash_recovery_prefix_property() {
        const SEED: u64 = 0x000C_A10D_5EED_0001;
        println!("crash_recovery_prefix_property seed = {SEED:#x}");
        let mut rng = SmallRng::seed_from_u64(SEED);
        let dir = temp_dir();
        let config = WalConfig::new(dir.clone())
            .with_max_segment_size(16 * 1024)
            .with_fsync_strategy(FsyncStrategy::Everysec);

        let mut acknowledged: Vec<(RecordType, Vec<u8>)> = Vec::new();
        let mut crashes = 0u32;
        let mut clean_checks = 0u32;
        let mut writer = Some(WalWriter::open(config.clone()).await.expect("open"));

        let assert_prefix = |recovered: &[WalRecord], acknowledged: &[(RecordType, Vec<u8>)]| {
            assert!(
                recovered.len() <= acknowledged.len(),
                "recovered {} records but only {} were acknowledged",
                recovered.len(),
                acknowledged.len()
            );
            for (idx, (record, (record_type, payload))) in
                recovered.iter().zip(acknowledged.iter()).enumerate()
            {
                assert_eq!(
                    (record.record_type, &record.payload),
                    (*record_type, payload),
                    "record {idx} diverges from acknowledged stream (seed {SEED:#x})"
                );
            }
        };

        while acknowledged.len() < 10_000 {
            let batch = rng.random_range(1..=40usize);
            for _ in 0..batch {
                let len = rng.random_range(0..=600usize);
                let mut payload = vec![0u8; len];
                rng.fill(payload.as_mut_slice());
                let record_type = if rng.random_bool(0.05) {
                    RecordType::Checkpoint
                } else {
                    RecordType::Put
                };
                writer
                    .as_ref()
                    .expect("writer")
                    .append(record_type, &payload)
                    .await
                    .expect("append");
                acknowledged.push((record_type, payload));
            }

            let roll = rng.random_range(0..100u8);
            if roll < 15 {
                // Simulated crash at a random point: drop WITHOUT flush.
                crashes += 1;
                drop(writer.take());
                tokio::time::sleep(Duration::from_millis(5)).await;
                let recovered = replay_all(&dir).expect("replay after crash");
                assert_eq!(
                    recovered.len(),
                    acknowledged.len(),
                    "after a quiescent crash the recovered stream must equal the \
                     acknowledged stream exactly (seed {SEED:#x})"
                );
                assert_prefix(&recovered, &acknowledged);
                writer = Some(WalWriter::open(config.clone()).await.expect("reopen"));
            } else if roll < 35 {
                // Clean flush: 100% of acknowledged records must be present.
                clean_checks += 1;
                writer
                    .as_ref()
                    .expect("writer")
                    .flush()
                    .await
                    .expect("flush");
                let recovered = replay_all(&dir).expect("replay after flush");
                assert_eq!(
                    recovered.len(),
                    acknowledged.len(),
                    "after a clean flush all acknowledged records must be present \
                     (seed {SEED:#x})"
                );
                assert_prefix(&recovered, &acknowledged);
            }
        }

        writer
            .as_ref()
            .expect("writer")
            .flush()
            .await
            .expect("final flush");
        let recovered = replay_all(&dir).expect("final replay");
        assert_eq!(recovered.len(), acknowledged.len());
        assert_prefix(&recovered, &acknowledged);
        writer
            .take()
            .expect("writer")
            .shutdown()
            .await
            .expect("shutdown");

        println!(
            "crash_recovery_prefix_property: {} appended records, {} crashes, \
             {} clean-flush checks, seed {SEED:#x}",
            acknowledged.len(),
            crashes,
            clean_checks
        );
        assert!(acknowledged.len() >= 10_000);
        assert!(crashes > 0, "property test must exercise crashes");
        let _ = fs::remove_dir_all(dir);
    }
}
