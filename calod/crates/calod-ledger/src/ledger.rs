//! The ledger itself: acquire/transition state machine, runs, events,
//! checkpoints, WAL-first durability, and recovery.
//!
//! # Locking and linearizability
//!
//! Three lock layers, always taken in this order (never the reverse):
//!
//! 1. **partition lock** — `(project_id, scope, idempotency_key)` hashes to
//!    one of [`DEFAULT_PARTITIONS`] partitions, each an async mutex. The
//!    entire acquire/transition decision for a key — read current state,
//!    decide, persist, apply — happens under its partition lock, so
//!    decisions for one key are linearizable.
//! 2. **run lock** — one async mutex per run, serializing event appends so
//!    `sequence` stays gap-free and the hash chain intact.
//! 3. **commit lock** — one global async mutex around *WAL append → index
//!    apply → watermark update*. This makes the snapshot watermark exact:
//!    every record at or before the watermark is already applied to the
//!    indexes, so a snapshot plus post-watermark replay reconstructs the
//!    precise pre-crash state.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use dashmap::DashMap;
use serde_json::{json, Value};
use sha2::Digest as _;
use tokio::sync::Mutex;
use tracing::{info, warn};
use uuid::Uuid;

use calod_core::store::error::PersistenceError;
use calod_storage::snapshot::SnapshotWalWatermark;
use calod_storage::wal::{RecordType, Replay, WalConfig, WalPosition, WalWriter};

use crate::clock::Clock;
use crate::digest::{input_digest, sha256_digest, GENESIS_HASH};
use crate::error::{LedgerError, Result};
use crate::model::*;
use crate::record::{self, RecordKind};
use crate::snapshot::{load_snapshot, save_snapshot, LedgerSnapshotManifest, PersistedState};

/// Number of acquire partitions.
pub const DEFAULT_PARTITIONS: usize = 64;

/// Point-in-time ledger counters/gauges for metrics export.
#[derive(Debug, Clone, Default)]
pub struct LedgerStats {
    /// Acquires that granted execution.
    pub acquires_execute: u64,
    /// Acquires that replayed a stored receipt.
    pub acquires_replay: u64,
    /// Acquires that asked the caller to wait.
    pub acquires_wait: u64,
    /// Acquires that directed the caller to reconcile.
    pub acquires_reconcile: u64,
    /// Successful effect transitions.
    pub transitions_total: u64,
    /// Runs removed by retention, cumulative.
    pub retention_runs_removed: u64,
    /// Effects removed by retention, cumulative.
    pub retention_effects_removed: u64,
    /// Current runs in the index.
    pub runs_current: usize,
    /// Current effects in the index.
    pub effects_current: usize,
    /// Current events across all runs.
    pub events_current: usize,
    /// Current effects by API state name.
    pub effects_by_state: std::collections::BTreeMap<String, usize>,
}

#[derive(Default)]
struct LedgerCounters {
    acquires_execute: std::sync::atomic::AtomicU64,
    acquires_replay: std::sync::atomic::AtomicU64,
    acquires_wait: std::sync::atomic::AtomicU64,
    acquires_reconcile: std::sync::atomic::AtomicU64,
    transitions_total: std::sync::atomic::AtomicU64,
    retention_runs_removed: std::sync::atomic::AtomicU64,
    retention_effects_removed: std::sync::atomic::AtomicU64,
}

impl LedgerCounters {
    fn bump_acquire(&self, decision: EffectDecisionType) {
        use std::sync::atomic::Ordering::Relaxed;
        let counter = match decision {
            EffectDecisionType::Execute => &self.acquires_execute,
            EffectDecisionType::Replay => &self.acquires_replay,
            EffectDecisionType::Wait => &self.acquires_wait,
            EffectDecisionType::Reconcile => &self.acquires_reconcile,
        };
        counter.fetch_add(1, Relaxed);
    }
}

const DEFAULT_LEASE: Duration = Duration::from_secs(30);
const MIN_LEASE: Duration = Duration::from_secs(5);
const MAX_LEASE: Duration = Duration::from_secs(3600);

/// Configuration for [`Ledger`].
#[derive(Debug, Clone)]
pub struct LedgerConfig {
    /// WAL configuration (directory, segment size, durability class).
    pub wal: WalConfig,
    /// Directory for index snapshots.
    pub snapshot_dir: PathBuf,
    /// Retention policy (Phase 1); `None` disables pruning. Manual
    /// application via [`Ledger::apply_retention`]; not run implicitly.
    pub retention: Option<RetentionPolicy>,
    /// Number of acquire partitions.
    pub partitions: usize,
    /// Lease duration used when a request does not specify one.
    pub default_lease: Duration,
    /// Minimum accepted lease duration.
    pub min_lease: Duration,
    /// Maximum accepted lease duration.
    pub max_lease: Duration,
}

impl LedgerConfig {
    /// Configuration rooted at `dir` (`dir/wal` for the WAL, `dir/snapshots`
    /// for snapshots) with defaults: 64 partitions, 30s default lease,
    /// 5s–3600s lease bounds.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        Self {
            wal: WalConfig::new(dir.join("wal")),
            snapshot_dir: dir.join("snapshots"),
            retention: None,
            partitions: DEFAULT_PARTITIONS,
            default_lease: DEFAULT_LEASE,
            min_lease: MIN_LEASE,
            max_lease: MAX_LEASE,
        }
    }

    /// Override the WAL durability class.
    pub fn with_wal_config(mut self, wal: WalConfig) -> Self {
        self.wal = wal;
        self
    }

    /// Set the retention policy (see [`RetentionPolicy`]).
    pub fn with_retention(mut self, retention: Option<RetentionPolicy>) -> Self {
        self.retention = retention;
        self
    }
}

/// Lookup key for the effect id index.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct EffectKey {
    tenant_id: String,
    project_id: String,
    scope: String,
    idempotency_key: String,
}

/// In-memory indexes; rebuilt from snapshot + WAL at open, mutated only
/// after WAL acknowledgment.
#[derive(Default)]
struct Indexes {
    effects_by_id: DashMap<Uuid, EffectRecord>,
    effect_ids: DashMap<EffectKey, Uuid>,
    runs: DashMap<Uuid, RunRecord>,
    run_locks: DashMap<Uuid, Arc<Mutex<()>>>,
    events: DashMap<Uuid, Vec<EventRecord>>,
    checkpoints: DashMap<Uuid, Vec<CheckpointRecord>>,
}

impl Indexes {
    /// Apply one persisted mutation. Idempotent (safe for records already
    /// covered by a snapshot), fail-closed on event-chain gaps.
    fn apply(&self, kind: RecordKind) -> std::result::Result<(), PersistenceError> {
        match kind {
            RecordKind::RunCreated { run } | RecordKind::RunUpdated { run } => {
                self.run_locks
                    .entry(run.id)
                    .or_insert_with(|| Arc::new(Mutex::new(())));
                self.runs.insert(run.id, *run);
            }
            RecordKind::EffectApplied { effect, event } => {
                let key = effect_key_of(&effect);
                self.effect_ids.insert(key, effect.id);
                self.effects_by_id.insert(effect.id, *effect);
                if let Some(event) = event {
                    self.apply_event(*event)?;
                }
            }
            RecordKind::EventAppended { event } => {
                self.apply_event(*event)?;
            }
            RecordKind::CheckpointCreated { checkpoint } => {
                let mut checkpoints = self.checkpoints.entry(checkpoint.run_id).or_default();
                if !checkpoints.iter().any(|c| c.id == checkpoint.id) {
                    checkpoints.push(*checkpoint);
                }
            }
            RecordKind::RetentionApplied {
                run_ids,
                effect_ids,
                ..
            } => {
                for run_id in &run_ids {
                    self.runs.remove(run_id);
                    self.events.remove(run_id);
                    self.checkpoints.remove(run_id);
                    self.run_locks.remove(run_id);
                }
                for effect_id in &effect_ids {
                    if let Some((_, effect)) = self.effects_by_id.remove(effect_id) {
                        self.effect_ids.remove(&effect_key_of(&effect));
                    }
                }
            }
        }
        Ok(())
    }

    fn apply_event(&self, event: EventRecord) -> std::result::Result<(), PersistenceError> {
        let mut events = self.events.entry(event.run_id).or_default();
        if event.sequence <= events.len() as i64 {
            // Already covered (snapshot + replay overlap).
            return Ok(());
        }
        if event.sequence != events.len() as i64 + 1 {
            return Err(PersistenceError::Corruption(format!(
                "run {} event sequence gap: have {}, got {}",
                event.run_id,
                events.len(),
                event.sequence
            )));
        }
        let expected_previous = events
            .last()
            .map(|e| e.hash.clone())
            .unwrap_or_else(|| GENESIS_HASH.to_string());
        if event.previous_hash != expected_previous {
            return Err(PersistenceError::Corruption(format!(
                "run {} event {} breaks the hash chain",
                event.run_id, event.sequence
            )));
        }
        if let Some(mut run) = self.runs.get_mut(&event.run_id) {
            run.last_event_sequence = event.sequence;
            run.head_hash = event.hash.clone();
        }
        events.push(event);
        Ok(())
    }

    fn collect_state(&self) -> PersistedState {
        let mut events = Vec::new();
        for entry in &self.events {
            events.extend(entry.value().iter().cloned());
        }
        let mut checkpoints = Vec::new();
        for entry in &self.checkpoints {
            checkpoints.extend(entry.value().iter().cloned());
        }
        PersistedState {
            schema_version: SCHEMA_VERSION,
            runs: self.runs.iter().map(|r| r.value().clone()).collect(),
            effects: self
                .effects_by_id
                .iter()
                .map(|r| r.value().clone())
                .collect(),
            events,
            checkpoints,
        }
    }

    fn load_state(&self, state: PersistedState) {
        for run in state.runs {
            self.run_locks
                .entry(run.id)
                .or_insert_with(|| Arc::new(Mutex::new(())));
            self.runs.insert(run.id, run);
        }
        for effect in state.effects {
            self.effect_ids.insert(effect_key_of(&effect), effect.id);
            self.effects_by_id.insert(effect.id, effect);
        }
        let mut events_by_run: BTreeMap<Uuid, Vec<EventRecord>> = BTreeMap::new();
        for event in state.events {
            events_by_run.entry(event.run_id).or_default().push(event);
        }
        for events in events_by_run.values_mut() {
            events.sort_by_key(|e| e.sequence);
        }
        for (run_id, events) in events_by_run {
            self.events.insert(run_id, events);
        }
        let mut checkpoints_by_run: BTreeMap<Uuid, Vec<CheckpointRecord>> = BTreeMap::new();
        for checkpoint in state.checkpoints {
            checkpoints_by_run
                .entry(checkpoint.run_id)
                .or_default()
                .push(checkpoint);
        }
        for (run_id, checkpoints) in checkpoints_by_run {
            self.checkpoints.insert(run_id, checkpoints);
        }
    }
}

fn effect_key_of(effect: &EffectRecord) -> EffectKey {
    EffectKey {
        tenant_id: effect.tenant_id.clone(),
        project_id: effect.project_id.clone(),
        scope: effect.scope.clone(),
        idempotency_key: effect.idempotency_key.clone(),
    }
}

/// The durable effect ledger. See the crate and module docs for semantics.
///
/// Not `Clone`: there is exactly one writer per directory. Use
/// [`Ledger::shutdown`] for a graceful close; dropping the ledger aborts the
/// WAL writer without flushing, mimicking a crash.
pub struct Ledger {
    clock: Arc<dyn Clock>,
    wal: WalWriter,
    snapshot_dir: PathBuf,
    partitions: Vec<Arc<Mutex<()>>>,
    commit_lock: Mutex<()>,
    last_wal_position: StdMutex<Option<WalPosition>>,
    default_lease: Duration,
    min_lease: Duration,
    max_lease: Duration,
    indexes: Indexes,
    counters: LedgerCounters,
    retention: Option<RetentionPolicy>,
}

impl Ledger {
    /// Open (or create) a ledger: load the verified snapshot, replay WAL
    /// records past the snapshot watermark into the indexes, then resume
    /// appending. Fail-closed on any corruption.
    pub async fn open(config: LedgerConfig, clock: Arc<dyn Clock>) -> Result<Self> {
        let indexes = Indexes::default();
        let mut watermark: Option<WalPosition> = None;

        if let Some((state, manifest)) = load_snapshot(&config.snapshot_dir).await? {
            if state.schema_version != SCHEMA_VERSION {
                return Err(LedgerError::Persistence(PersistenceError::Corruption(
                    format!(
                        "snapshot schema version {} (expected {SCHEMA_VERSION})",
                        state.schema_version
                    ),
                )));
            }
            info!(
                effects = state.effects.len(),
                runs = state.runs.len(),
                "ledger: loaded verified snapshot"
            );
            watermark = manifest.wal_watermark.map(|w| WalPosition {
                segment_id: w.segment_id,
                offset: w.offset,
            });
            indexes.load_state(state);
        }

        let replay = Replay::open(&config.wal.dir)?;
        let truncated = replay.report().truncated;
        let mut replayed = 0u64;
        for record in replay {
            let record = record?;
            let covered = watermark
                .is_some_and(|w| (record.segment_id, record.offset) <= (w.segment_id, w.offset));
            if covered {
                continue;
            }
            let decoded = record::decode(&record.payload)?;
            indexes.apply(decoded.kind)?;
            replayed += 1;
        }
        if let Some(truncated) = truncated {
            warn!(
                segment_id = truncated.segment_id,
                "ledger: WAL crash tail truncated"
            );
        }
        if replayed > 0 {
            info!(replayed, "ledger: WAL replay complete");
        }

        let wal = WalWriter::open(config.wal).await?;
        let partitions = (0..config.partitions.max(1))
            .map(|_| Arc::new(Mutex::new(())))
            .collect();
        Ok(Self {
            clock,
            wal,
            snapshot_dir: config.snapshot_dir,
            partitions,
            commit_lock: Mutex::new(()),
            last_wal_position: StdMutex::new(watermark),
            default_lease: config.default_lease,
            min_lease: config.min_lease,
            max_lease: config.max_lease,
            indexes,
            counters: LedgerCounters::default(),
            retention: config.retention,
        })
    }

    /// Graceful close: drain and fsync the WAL, stop the writer task.
    pub async fn shutdown(self) -> Result<()> {
        self.wal.shutdown().await?;
        Ok(())
    }

    /// Write and fsync everything queued in the WAL so far.
    pub async fn flush(&self) -> Result<()> {
        self.wal.flush().await?;
        Ok(())
    }

    /// Snapshot the indexes and truncate WAL segments fully covered by the
    /// snapshot. Holds the commit lock for the whole save, so the recorded
    /// watermark is exact.
    pub async fn snapshot(&self) -> Result<LedgerSnapshotManifest> {
        let (watermark, manifest) = {
            let _commit = self.commit_lock.lock().await;
            self.wal.flush().await?;
            let watermark = *self
                .last_wal_position
                .lock()
                .expect("watermark mutex poisoned");
            let state = self.indexes.collect_state();
            let manifest = save_snapshot(
                &self.snapshot_dir,
                &state,
                watermark.map(|p| SnapshotWalWatermark {
                    segment_id: p.segment_id,
                    offset: p.offset,
                }),
                self.clock.now_ms(),
            )
            .await?;
            info!(files = manifest.files.len(), "ledger: snapshot saved");
            (watermark, manifest)
        };
        if let Some(watermark) = watermark {
            // The watermark record's own segment may still be the active one;
            // `truncate_before` never removes the active segment.
            self.wal.truncate_before(watermark.segment_id + 1).await?;
        }
        Ok(manifest)
    }

    // ------------------------------------------------------------------
    // Effects
    // ------------------------------------------------------------------

    /// Atomically create, reacquire, replay, or flag an effect for
    /// reconciliation. The whole decision runs under the owning partition
    /// lock; mutations are WAL-acknowledged before they become visible.
    pub async fn acquire(
        &self,
        scope: &Scope,
        idempotency_key: &str,
        req: AcquireEffectRequest,
    ) -> Result<EffectDecision> {
        validate_effect_fields(&req.scope, idempotency_key, &req.tool_name)?;
        let lease_ms =
            self.resolve_lease_ms(req.lease_duration_seconds, "lease_duration_seconds")?;
        let digest = input_digest(&req.input);
        let key = EffectKey {
            tenant_id: scope.tenant_id.clone(),
            project_id: scope.project_id.clone(),
            scope: req.scope.clone(),
            idempotency_key: idempotency_key.to_string(),
        };

        let partition = self.partition(&key);
        let _partition_guard = partition.lock().await;
        let now = self.clock.now_ms();

        /// What `acquire` decided, and what (if anything) must be persisted.
        enum Plan {
            /// Pure read decision (replay / wait / reconcile).
            Decide(EffectDecision),
            /// Persist a new effect state, then decide.
            Apply {
                effect: EffectRecord,
                event_type: &'static str,
                detail: Value,
                decision: EffectDecisionType,
                lease_token: Option<String>,
            },
        }

        let existing = self.indexes.effect_ids.get(&key).and_then(|id| {
            self.indexes
                .effects_by_id
                .get(id.value())
                .map(|e| e.clone())
        });

        let plan = match existing {
            None => {
                if let Some(run_id) = req.run_id {
                    self.run_in_scope(scope, run_id)?;
                }
                let lease_token = generate_lease_token();
                let effect = EffectRecord {
                    schema_version: SCHEMA_VERSION,
                    tenant_id: scope.tenant_id.clone(),
                    project_id: scope.project_id.clone(),
                    id: self.new_id(),
                    run_id: req.run_id,
                    scope: req.scope.clone(),
                    idempotency_key: idempotency_key.to_string(),
                    tool_name: req.tool_name.clone(),
                    input_digest: digest,
                    state: EffectStatus::Claimed,
                    fencing_token: 1,
                    lease_expires_at_ms: Some(now + lease_ms),
                    lease_duration_ms: lease_ms,
                    lease_token_digest: Some(sha256_digest(lease_token.as_bytes())),
                    result: None,
                    external_receipt: None,
                    failure: None,
                    created_at_ms: now,
                    updated_at_ms: now,
                };
                Plan::Apply {
                    effect,
                    event_type: "effect.acquired",
                    detail: json!({ "from": Value::Null, "to": "claimed" }),
                    decision: EffectDecisionType::Execute,
                    lease_token: Some(lease_token),
                }
            }
            Some(effect) => {
                if effect.input_digest != digest || effect.tool_name != req.tool_name {
                    return Err(LedgerError::IdempotencyConflict {
                        scope: req.scope.clone(),
                        key: idempotency_key.to_string(),
                        existing_digest: effect.input_digest.clone(),
                        requested_digest: digest,
                    });
                }
                match effect.state {
                    EffectStatus::Committed
                    | EffectStatus::ReconciledCommitted
                    | EffectStatus::FailedTerminal
                    | EffectStatus::ReconciledFailed => Plan::Decide(EffectDecision {
                        decision: EffectDecisionType::Replay,
                        effect: effect.view(),
                        lease_token: None,
                        poll_after_ms: None,
                    }),
                    EffectStatus::OutcomeUnknown => Plan::Decide(EffectDecision {
                        decision: EffectDecisionType::Reconcile,
                        effect: effect.view(),
                        lease_token: None,
                        poll_after_ms: None,
                    }),
                    EffectStatus::FailedRetryable => {
                        // The tool provably never fired: safe reacquire.
                        let (effect, lease_token) = reacquire(effect, &req, lease_ms, now);
                        Plan::Apply {
                            effect,
                            event_type: "effect.reacquired",
                            detail: json!({ "from": "failed_retryable", "to": "claimed" }),
                            decision: EffectDecisionType::Execute,
                            lease_token,
                        }
                    }
                    EffectStatus::Claimed => {
                        let expires = effect.lease_expires_at_ms.unwrap_or(now);
                        if now < expires {
                            Plan::Decide(wait_decision(&effect, expires, now))
                        } else {
                            // Claimed lease expired before the tool started:
                            // the tool never fired, safe to reacquire.
                            let (effect, lease_token) = reacquire(effect, &req, lease_ms, now);
                            Plan::Apply {
                                effect,
                                event_type: "effect.reacquired",
                                detail: json!({ "from": "claimed", "to": "claimed", "reason": "lease_expired" }),
                                decision: EffectDecisionType::Execute,
                                lease_token,
                            }
                        }
                    }
                    EffectStatus::InProgress => {
                        let expires = effect.lease_expires_at_ms.unwrap_or(now);
                        if now < expires {
                            Plan::Decide(wait_decision(&effect, expires, now))
                        } else {
                            // The tool may have fired. Never auto-retry, never
                            // hand out a new lease: flag for reconciliation.
                            let mut next = effect.clone();
                            next.state = EffectStatus::OutcomeUnknown;
                            next.lease_expires_at_ms = None;
                            next.lease_token_digest = None;
                            next.updated_at_ms = now;
                            if next.failure.is_none() {
                                next.failure = Some(EffectFailure {
                                    kind: "lease_expired_in_progress".to_string(),
                                    message:
                                        "Lease expired while in progress; outcome cannot be proven"
                                            .to_string(),
                                    retryable: false,
                                });
                            }
                            Plan::Apply {
                                effect: next,
                                event_type: "effect.outcome_unknown",
                                detail: json!({ "from": "in_progress", "to": "outcome_unknown", "reason": "lease_expired" }),
                                decision: EffectDecisionType::Reconcile,
                                lease_token: None,
                            }
                        }
                    }
                }
            }
        };

        match plan {
            Plan::Decide(decision) => {
                self.counters.bump_acquire(decision.decision);
                Ok(decision)
            }
            Plan::Apply {
                effect,
                event_type,
                detail,
                decision,
                lease_token,
            } => {
                let view = effect.view();
                let event = self
                    .build_effect_event(scope, &effect, event_type, detail, now)
                    .await?;
                self.persist(RecordKind::EffectApplied {
                    effect: Box::new(effect),
                    event: event.map(Box::new),
                })
                .await?;
                self.counters.bump_acquire(decision);
                Ok(EffectDecision {
                    decision,
                    effect: view,
                    lease_token,
                    poll_after_ms: None,
                })
            }
        }
    }

    /// Transition, renew, or reconcile an effect. Worker transitions require
    /// the active lease token and the current fencing token; reconcile
    /// transitions require [`CallerCapabilities::can_reconcile`] and the
    /// current fencing token (no lease token).
    pub async fn transition(
        &self,
        scope: &Scope,
        effect_id: Uuid,
        req: UpdateEffectRequest,
        capabilities: CallerCapabilities,
    ) -> Result<Effect> {
        let current = self.effect_in_scope(scope, effect_id)?;
        let key = effect_key_of(&current);
        let partition = self.partition(&key);
        let _partition_guard = partition.lock().await;
        let effect = self
            .indexes
            .effects_by_id
            .get(&effect_id)
            .map(|e| e.clone())
            .ok_or_else(|| LedgerError::NotFound {
                resource: "effect",
                id: effect_id.to_string(),
            })?;
        let now = self.clock.now_ms();

        // 1. Fencing: a stale token can never transition the effect.
        if req.fencing_token != effect.fencing_token {
            return Err(LedgerError::StaleFencingToken {
                current: effect.fencing_token,
                got: req.fencing_token,
            });
        }

        // 2. State legality.
        let from = effect.state;
        let legal = matches!(
            (req.transition, from),
            (EffectTransition::Start, EffectStatus::Claimed)
                | (EffectTransition::Renew, EffectStatus::Claimed)
                | (EffectTransition::Renew, EffectStatus::InProgress)
                | (EffectTransition::Commit, EffectStatus::Claimed)
                | (EffectTransition::Commit, EffectStatus::InProgress)
                | (EffectTransition::FailRetryable, EffectStatus::Claimed)
                | (EffectTransition::FailRetryable, EffectStatus::InProgress)
                | (EffectTransition::FailTerminal, EffectStatus::Claimed)
                | (EffectTransition::FailTerminal, EffectStatus::InProgress)
                | (EffectTransition::MarkOutcomeUnknown, EffectStatus::Claimed)
                | (
                    EffectTransition::MarkOutcomeUnknown,
                    EffectStatus::InProgress
                )
                | (
                    EffectTransition::ReconcileCommitted,
                    EffectStatus::OutcomeUnknown
                )
                | (
                    EffectTransition::ReconcileFailed,
                    EffectStatus::OutcomeUnknown
                )
        );
        if !legal {
            return Err(LedgerError::InvalidTransition {
                from: from.as_str().to_string(),
                transition: req.transition.as_str().to_string(),
            });
        }

        // 3. Authorization.
        if req.transition.is_worker_transition() {
            let token = req.lease_token.as_deref().ok_or(LedgerError::Validation {
                field: "lease_token",
                message: "required for worker transitions".to_string(),
            })?;
            let digest = sha256_digest(token.as_bytes());
            if effect.lease_token_digest.as_deref() != Some(digest.as_str()) {
                return Err(LedgerError::Forbidden(
                    "lease token does not match the active lease".to_string(),
                ));
            }
        } else if !capabilities.can_reconcile {
            return Err(LedgerError::Forbidden(
                "reconcile transitions require the effects:reconcile capability".to_string(),
            ));
        }

        // 4. Build the new state.
        let mut next = effect.clone();
        next.updated_at_ms = now;
        let event_type: &'static str = match req.transition {
            EffectTransition::Start => {
                next.state = EffectStatus::InProgress;
                "effect.started"
            }
            EffectTransition::Renew => {
                let lease_ms = match req.lease_duration_seconds {
                    Some(secs) => self.resolve_lease_ms(Some(secs), "lease_duration_seconds")?,
                    None => effect.lease_duration_ms,
                };
                next.lease_expires_at_ms = Some(now + lease_ms);
                next.lease_duration_ms = lease_ms;
                "effect.renewed"
            }
            EffectTransition::Commit => {
                next.state = EffectStatus::Committed;
                next.result = req.result.clone();
                next.external_receipt = req.external_receipt.clone();
                next.failure = None;
                next.lease_expires_at_ms = None;
                next.lease_token_digest = None;
                "effect.committed"
            }
            EffectTransition::FailRetryable | EffectTransition::FailTerminal => {
                next.state = if req.transition == EffectTransition::FailRetryable {
                    EffectStatus::FailedRetryable
                } else {
                    EffectStatus::FailedTerminal
                };
                next.failure = Some(req.failure.clone().unwrap_or(EffectFailure {
                    kind: "failed".to_string(),
                    message: "worker reported failure".to_string(),
                    retryable: req.transition == EffectTransition::FailRetryable,
                }));
                next.lease_expires_at_ms = None;
                next.lease_token_digest = None;
                "effect.failed"
            }
            EffectTransition::MarkOutcomeUnknown => {
                next.state = EffectStatus::OutcomeUnknown;
                next.failure = req.failure.clone();
                next.lease_expires_at_ms = None;
                next.lease_token_digest = None;
                "effect.outcome_unknown"
            }
            EffectTransition::ReconcileCommitted => {
                next.state = EffectStatus::ReconciledCommitted;
                next.result = req.result.clone();
                next.external_receipt = req.external_receipt.clone();
                "effect.reconciled"
            }
            EffectTransition::ReconcileFailed => {
                next.state = EffectStatus::ReconciledFailed;
                next.failure = Some(req.failure.clone().unwrap_or(EffectFailure {
                    kind: "reconciled_failed".to_string(),
                    message: "reconciliation proved the tool did not fire".to_string(),
                    retryable: false,
                }));
                "effect.reconciled"
            }
        };
        let detail = json!({
            "from": from.as_str(),
            "to": next.state.as_str(),
            "transition": req.transition.as_str(),
        });
        let event = self
            .build_effect_event(scope, &next, event_type, detail, now)
            .await?;
        self.persist(RecordKind::EffectApplied {
            effect: Box::new(next.clone()),
            event: event.map(Box::new),
        })
        .await?;
        self.counters
            .transitions_total
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(next.view())
    }

    /// Get an effect by id (scoped; other tenants/projects see 404).
    pub async fn get_effect(&self, scope: &Scope, effect_id: Uuid) -> Result<Effect> {
        Ok(self.effect_in_scope(scope, effect_id)?.view())
    }

    /// Cursor-paginated effects, newest first (`created_at`, then id,
    /// descending).
    pub async fn list_effects(
        &self,
        scope: &Scope,
        filter: &EffectFilter,
        cursor: Option<&str>,
        limit: Option<u32>,
    ) -> Result<EffectList> {
        let limit = resolve_limit(limit)?;
        let after = decode_cursor(cursor)?;
        let mut effects: Vec<EffectRecord> = self
            .indexes
            .effects_by_id
            .iter()
            .map(|e| e.clone())
            .filter(|e| {
                e.tenant_id == scope.tenant_id
                    && e.project_id == scope.project_id
                    && filter.run_id.is_none_or(|r| e.run_id == Some(r))
                    && filter.state.is_none_or(|s| e.state.api_state() == s)
                    && filter.tool_name.as_ref().is_none_or(|t| &e.tool_name == t)
                    && filter.created_after_ms.is_none_or(|t| e.created_at_ms >= t)
            })
            .collect();
        effects.sort_by(|a, b| {
            (b.created_at_ms, b.id.as_u128()).cmp(&(a.created_at_ms, a.id.as_u128()))
        });
        if let Some((ms, id)) = after {
            effects.retain(|e| (e.created_at_ms, e.id.as_u128()) < (ms, id.as_u128()));
        }
        let (page, page_info) = paginate(effects, limit, |e| encode_cursor(e.created_at_ms, e.id));
        Ok(EffectList {
            data: page.iter().map(|e| e.view()).collect(),
            pagination: page_info,
        })
    }

    // ------------------------------------------------------------------
    // Runs
    // ------------------------------------------------------------------

    /// Create a run (`running`, version 1).
    pub async fn create_run(&self, scope: &Scope, req: CreateRunRequest) -> Result<Run> {
        if req.external_id.is_empty() || req.external_id.len() > 255 {
            return Err(LedgerError::Validation {
                field: "external_id",
                message: "must be 1..=255 characters".to_string(),
            });
        }
        if req.agent_name.is_empty() || req.agent_name.len() > 200 {
            return Err(LedgerError::Validation {
                field: "agent_name",
                message: "must be 1..=200 characters".to_string(),
            });
        }
        let now = self.clock.now_ms();
        let run = RunRecord {
            schema_version: SCHEMA_VERSION,
            tenant_id: scope.tenant_id.clone(),
            project_id: scope.project_id.clone(),
            id: self.new_id(),
            external_id: req.external_id,
            agent_name: req.agent_name,
            status: RunStatus::Running,
            version: 1,
            last_event_sequence: 0,
            head_hash: GENESIS_HASH.to_string(),
            parent_run_id: None,
            forked_from_event_sequence: None,
            metadata: req.metadata,
            created_at_ms: now,
            updated_at_ms: now,
        };
        let view = run.view();
        self.persist(RecordKind::RunCreated { run: Box::new(run) })
            .await?;
        Ok(view)
    }

    /// Get a run by id (scoped).
    pub async fn get_run(&self, scope: &Scope, run_id: Uuid) -> Result<Run> {
        Ok(self.run_in_scope(scope, run_id)?.view())
    }

    /// Transition a run's status. `expected_version` implements the
    /// `If-Match` optimistic-concurrency check. Terminal statuses
    /// (`completed`/`failed`/`cancelled`) are final.
    pub async fn update_run(
        &self,
        scope: &Scope,
        run_id: Uuid,
        req: UpdateRunRequest,
        expected_version: Option<i64>,
    ) -> Result<Run> {
        let run_lock = self.run_lock(run_id);
        let _run_guard = run_lock.lock().await;
        let run = self.run_in_scope(scope, run_id)?;
        if let Some(expected) = expected_version {
            if expected != run.version {
                return Err(LedgerError::PreconditionFailed {
                    expected,
                    current: run.version,
                });
            }
        }
        if run.status.is_terminal() && req.status != run.status {
            return Err(LedgerError::InvalidTransition {
                from: run.status.as_str().to_string(),
                transition: req.status.as_str().to_string(),
            });
        }
        let mut next = run;
        next.status = req.status;
        next.version += 1;
        next.updated_at_ms = self.clock.now_ms();
        let view = next.view();
        self.persist(RecordKind::RunUpdated {
            run: Box::new(next),
        })
        .await?;
        Ok(view)
    }

    /// Fork a run from an event sequence or a checkpoint. The fork starts
    /// empty (`running`, version 1) with lineage pointing at the parent.
    pub async fn fork_run(&self, scope: &Scope, run_id: Uuid, req: ForkRunRequest) -> Result<Run> {
        if req.reason.is_empty() || req.reason.len() > 500 {
            return Err(LedgerError::Validation {
                field: "reason",
                message: "must be 1..=500 characters".to_string(),
            });
        }
        let run_lock = self.run_lock(run_id);
        let _run_guard = run_lock.lock().await;
        let parent = self.run_in_scope(scope, run_id)?;
        let sequence = match (req.from_event_sequence, req.from_checkpoint_id) {
            (Some(seq), None) => {
                if seq < 0 || seq > parent.last_event_sequence {
                    return Err(LedgerError::Validation {
                        field: "from_event_sequence",
                        message: format!(
                            "must be within 0..={} for this run",
                            parent.last_event_sequence
                        ),
                    });
                }
                seq
            }
            (None, Some(checkpoint_id)) => self
                .indexes
                .checkpoints
                .get(&run_id)
                .and_then(|checkpoints| {
                    checkpoints
                        .iter()
                        .find(|c| c.id == checkpoint_id)
                        .map(|c| c.event_sequence)
                })
                .ok_or_else(|| LedgerError::NotFound {
                    resource: "checkpoint",
                    id: checkpoint_id.to_string(),
                })?,
            _ => {
                return Err(LedgerError::Validation {
                    field: "from_event_sequence",
                    message: "exactly one of from_event_sequence / from_checkpoint_id is required"
                        .to_string(),
                })
            }
        };
        let now = self.clock.now_ms();
        let fork = RunRecord {
            schema_version: SCHEMA_VERSION,
            tenant_id: scope.tenant_id.clone(),
            project_id: scope.project_id.clone(),
            id: self.new_id(),
            external_id: format!("{}:fork", parent.external_id),
            agent_name: parent.agent_name.clone(),
            status: RunStatus::Running,
            version: 1,
            last_event_sequence: 0,
            head_hash: GENESIS_HASH.to_string(),
            parent_run_id: Some(parent.id),
            forked_from_event_sequence: Some(sequence),
            metadata: req.metadata,
            created_at_ms: now,
            updated_at_ms: now,
        };
        let view = fork.view();
        self.persist(RecordKind::RunCreated {
            run: Box::new(fork),
        })
        .await?;
        Ok(view)
    }

    /// Cursor-paginated runs, newest first (`created_at`, then id,
    /// descending).
    pub async fn list_runs(
        &self,
        scope: &Scope,
        filter: &RunFilter,
        cursor: Option<&str>,
        limit: Option<u32>,
    ) -> Result<RunList> {
        let limit = resolve_limit(limit)?;
        let after = decode_cursor(cursor)?;
        let mut runs: Vec<RunRecord> = self
            .indexes
            .runs
            .iter()
            .map(|r| r.clone())
            .filter(|r| {
                r.tenant_id == scope.tenant_id
                    && r.project_id == scope.project_id
                    && filter.status.is_none_or(|s| r.status == s)
                    && filter
                        .agent_name
                        .as_ref()
                        .is_none_or(|a| &r.agent_name == a)
                    && filter.created_after_ms.is_none_or(|t| r.created_at_ms >= t)
            })
            .collect();
        runs.sort_by(|a, b| {
            (b.created_at_ms, b.id.as_u128()).cmp(&(a.created_at_ms, a.id.as_u128()))
        });
        if let Some((ms, id)) = after {
            runs.retain(|r| (r.created_at_ms, r.id.as_u128()) < (ms, id.as_u128()));
        }
        let (page, page_info) = paginate(runs, limit, |r| encode_cursor(r.created_at_ms, r.id));
        Ok(RunList {
            data: page.iter().map(|r| r.view()).collect(),
            pagination: page_info,
        })
    }

    // ------------------------------------------------------------------
    // Events
    // ------------------------------------------------------------------

    /// Append an immutable event to a run's timeline. Sequence assignment
    /// and hash chaining happen under the run lock.
    pub async fn append_event(
        &self,
        scope: &Scope,
        run_id: Uuid,
        req: AppendEventRequest,
    ) -> Result<Event> {
        validate_event_type(&req.event_type)?;
        let run_lock = self.run_lock(run_id);
        let _run_guard = run_lock.lock().await;
        let run = self.run_in_scope(scope, run_id)?;
        let now = self.clock.now_ms();
        let event = self.build_event(
            scope,
            &run,
            req.event_type,
            req.occurred_at_ms,
            req.payload,
            now,
        );
        let view = event.view();
        self.persist(RecordKind::EventAppended {
            event: Box::new(event),
        })
        .await?;
        Ok(view)
    }

    /// Verify a run's hash chain against the stored events.
    pub async fn verify_run_events(&self, scope: &Scope, run_id: Uuid) -> Result<bool> {
        self.run_in_scope(scope, run_id)?;
        let events = self
            .indexes
            .events
            .get(&run_id)
            .map(|e| e.clone())
            .unwrap_or_default();
        let views: Vec<Event> = events.iter().map(|e| e.view()).collect();
        Ok(verify_event_chain(&views))
    }

    /// Cursor-paginated events of a run, sequence ascending.
    pub async fn list_events(
        &self,
        scope: &Scope,
        run_id: Uuid,
        filter: &EventFilter,
        cursor: Option<&str>,
        limit: Option<u32>,
    ) -> Result<EventList> {
        let limit = resolve_limit(limit)?;
        self.run_in_scope(scope, run_id)?;
        let after = match cursor {
            None => None,
            Some(c) => Some(c.parse::<i64>().map_err(|_| LedgerError::Validation {
                field: "cursor",
                message: "invalid event cursor".to_string(),
            })?),
        };
        let mut events: Vec<EventRecord> = self
            .indexes
            .events
            .get(&run_id)
            .map(|e| e.clone())
            .unwrap_or_default()
            .into_iter()
            .filter(|e| {
                filter
                    .event_type
                    .as_ref()
                    .is_none_or(|t| &e.event_type == t)
                    && filter.after_sequence.is_none_or(|s| e.sequence > s)
                    && after.is_none_or(|s| e.sequence > s)
            })
            .collect();
        events.sort_by_key(|e| e.sequence);
        let (page, page_info) = paginate(events, limit, |e| e.sequence.to_string());
        Ok(EventList {
            data: page.iter().map(|e| e.view()).collect(),
            pagination: page_info,
        })
    }

    // ------------------------------------------------------------------
    // Checkpoints
    // ------------------------------------------------------------------

    /// Create an immutable checkpoint of a run at an event sequence.
    /// `expected_version` implements the `If-Match` check.
    pub async fn create_checkpoint(
        &self,
        scope: &Scope,
        run_id: Uuid,
        req: CreateCheckpointRequest,
        expected_version: Option<i64>,
    ) -> Result<Checkpoint> {
        let run_lock = self.run_lock(run_id);
        let _run_guard = run_lock.lock().await;
        let run = self.run_in_scope(scope, run_id)?;
        if let Some(expected) = expected_version {
            if expected != run.version {
                return Err(LedgerError::PreconditionFailed {
                    expected,
                    current: run.version,
                });
            }
        }
        if req.event_sequence < 0 || req.event_sequence > run.last_event_sequence {
            return Err(LedgerError::Validation {
                field: "event_sequence",
                message: format!(
                    "must be within 0..={} for this run",
                    run.last_event_sequence
                ),
            });
        }
        let checkpoint = CheckpointRecord {
            schema_version: SCHEMA_VERSION,
            tenant_id: scope.tenant_id.clone(),
            project_id: scope.project_id.clone(),
            id: self.new_id(),
            run_id,
            event_sequence: req.event_sequence,
            state_digest: sha256_digest(&crate::digest::canonical_json(&req.state)),
            state: req.state,
            created_at_ms: self.clock.now_ms(),
        };
        let view = checkpoint.view();
        self.persist(RecordKind::CheckpointCreated {
            checkpoint: Box::new(checkpoint),
        })
        .await?;
        Ok(view)
    }

    /// Cursor-paginated checkpoints of a run, `event_sequence` descending.
    pub async fn list_checkpoints(
        &self,
        scope: &Scope,
        run_id: Uuid,
        cursor: Option<&str>,
        limit: Option<u32>,
    ) -> Result<CheckpointList> {
        let limit = resolve_limit(limit)?;
        self.run_in_scope(scope, run_id)?;
        let after = decode_seq_id_cursor(cursor)?;
        let mut checkpoints: Vec<CheckpointRecord> = self
            .indexes
            .checkpoints
            .get(&run_id)
            .map(|c| c.clone())
            .unwrap_or_default();
        checkpoints.sort_by(|a, b| {
            (b.event_sequence, b.id.as_u128()).cmp(&(a.event_sequence, a.id.as_u128()))
        });
        if let Some((seq, id)) = after {
            checkpoints.retain(|c| (c.event_sequence, c.id.as_u128()) < (seq, id.as_u128()));
        }
        let (page, page_info) = paginate(checkpoints, limit, |c| {
            encode_cursor(c.event_sequence, c.id)
        });
        Ok(CheckpointList {
            data: page.iter().map(|c| c.view()).collect(),
            pagination: page_info,
        })
    }

    // ------------------------------------------------------------------
    // Retention, export, stats (Phase 1)
    // ------------------------------------------------------------------

    /// The configured retention policy, if any.
    pub fn retention_policy(&self) -> Option<RetentionPolicy> {
        self.retention
    }

    /// Prune retention-terminal data older than the given policy.
    ///
    /// Eligible: effects in a retention-terminal state (committed,
    /// terminally failed, reconciled) and runs in a terminal state,
    /// whose last update is at or before `now - max_age_ms`. Pruning
    /// a run removes its events and checkpoints too; effects of a
    /// pruned run are removed regardless of their own age. Active,
    /// retryable, and `outcome_unknown` data is never pruned. The
    /// prune is persisted as one WAL record before it is applied,
    /// so replay reproduces it exactly; call [`Ledger::snapshot`]
    /// afterwards to compact the WAL.
    pub async fn apply_retention(&self, policy: RetentionPolicy) -> Result<RetentionReport> {
        let now = self.clock.now_ms();
        let cutoff_ms = now.saturating_sub(policy.max_age_ms.max(0));
        let mut run_ids: Vec<Uuid> = self
            .indexes
            .runs
            .iter()
            .filter(|r| {
                let run = r.value();
                run.status.is_terminal() && run.updated_at_ms <= cutoff_ms
            })
            .map(|r| r.value().id)
            .collect();
        run_ids.sort();
        let pruned_runs: std::collections::BTreeSet<Uuid> = run_ids.iter().copied().collect();
        let mut effect_ids: Vec<Uuid> = self
            .indexes
            .effects_by_id
            .iter()
            .filter(|e| {
                let effect = e.value();
                (effect.state.is_retention_terminal() && effect.updated_at_ms <= cutoff_ms)
                    || effect.run_id.is_some_and(|r| pruned_runs.contains(&r))
            })
            .map(|e| e.value().id)
            .collect();
        effect_ids.sort();
        let events_removed: u64 = run_ids
            .iter()
            .map(|id| {
                self.indexes
                    .events
                    .get(id)
                    .map(|events| events.len() as u64)
                    .unwrap_or(0)
            })
            .sum();
        let report = RetentionReport {
            runs_removed: run_ids.len() as u64,
            effects_removed: effect_ids.len() as u64,
            events_removed,
            cutoff_ms,
        };
        if run_ids.is_empty() && effect_ids.is_empty() {
            return Ok(report);
        }
        self.persist(RecordKind::RetentionApplied {
            cutoff_ms,
            run_ids,
            effect_ids,
        })
        .await?;
        use std::sync::atomic::Ordering::Relaxed;
        self.counters
            .retention_runs_removed
            .fetch_add(report.runs_removed, Relaxed);
        self.counters
            .retention_effects_removed
            .fetch_add(report.effects_removed, Relaxed);
        Ok(report)
    }

    /// Prune with the configured policy, then snapshot to compact
    /// the WAL. Returns the default (zero) report when no policy is
    /// configured.
    pub async fn apply_configured_retention(&self) -> Result<RetentionReport> {
        let Some(policy) = self.retention else {
            return Ok(RetentionReport::default());
        };
        let report = self.apply_retention(policy).await?;
        if report.runs_removed > 0 || report.effects_removed > 0 {
            self.snapshot().await?;
        }
        Ok(report)
    }

    /// Gather everything an evidence export needs for one run.
    /// Scoped: other tenants/projects see 404, exactly like
    /// [`Ledger::get_run`].
    pub async fn export_run(&self, scope: &Scope, run_id: Uuid) -> Result<RunExport> {
        let run = self.run_in_scope(scope, run_id)?;
        let mut effects: Vec<Effect> = self
            .indexes
            .effects_by_id
            .iter()
            .filter(|e| e.value().run_id == Some(run_id))
            .map(|e| e.value().view())
            .collect();
        effects.sort_by_key(|e| (e.created_at.clone(), e.id));
        let events: Vec<Event> = self
            .indexes
            .events
            .get(&run_id)
            .map(|events| events.iter().map(|e| e.view()).collect())
            .unwrap_or_default();
        let mut checkpoints: Vec<Checkpoint> = self
            .indexes
            .checkpoints
            .get(&run_id)
            .map(|cps| cps.iter().map(|c| c.view()).collect())
            .unwrap_or_default();
        checkpoints.sort_by_key(|c| c.event_sequence);
        Ok(RunExport {
            run: run.view(),
            effects,
            events,
            checkpoints,
        })
    }

    /// Point-in-time counters and gauges for metrics export.
    pub fn stats(&self) -> LedgerStats {
        use std::sync::atomic::Ordering::Relaxed;
        let mut effects_by_state: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        for entry in self.indexes.effects_by_id.iter() {
            let state = serde_json::to_string(&entry.value().state.api_state())
                .unwrap_or_else(|_| "\"unknown\"".to_string())
                .trim_matches('"')
                .to_string();
            *effects_by_state.entry(state).or_default() += 1;
        }
        LedgerStats {
            acquires_execute: self.counters.acquires_execute.load(Relaxed),
            acquires_replay: self.counters.acquires_replay.load(Relaxed),
            acquires_wait: self.counters.acquires_wait.load(Relaxed),
            acquires_reconcile: self.counters.acquires_reconcile.load(Relaxed),
            transitions_total: self.counters.transitions_total.load(Relaxed),
            retention_runs_removed: self.counters.retention_runs_removed.load(Relaxed),
            retention_effects_removed: self.counters.retention_effects_removed.load(Relaxed),
            runs_current: self.indexes.runs.len(),
            effects_current: self.indexes.effects_by_id.len(),
            events_current: self.indexes.events.iter().map(|e| e.value().len()).sum(),
            effects_by_state,
        }
    }

    // ------------------------------------------------------------------
    // Internals
    // ------------------------------------------------------------------

    /// Append the record to the WAL, await acknowledgment per the configured
    /// durability class, then apply it to the indexes — all under the commit
    /// lock so the snapshot watermark is exact. Callers hold any
    /// partition/run locks needed to serialize the decision itself.
    async fn persist(&self, kind: RecordKind) -> Result<()> {
        let payload = record::encode(&kind)?;
        let _commit_guard = self.commit_lock.lock().await;
        let position = self.wal.append(RecordType::Put, &payload).await?;
        self.indexes.apply(kind)?;
        *self
            .last_wal_position
            .lock()
            .expect("watermark mutex poisoned") = Some(position);
        Ok(())
    }

    /// Build the next event on an effect's run timeline (run lock held
    /// inside). Returns `None` for runless effects.
    async fn build_effect_event(
        &self,
        scope: &Scope,
        effect: &EffectRecord,
        event_type: &'static str,
        detail: Value,
        now: i64,
    ) -> Result<Option<EventRecord>> {
        let Some(run_id) = effect.run_id else {
            return Ok(None);
        };
        let run_lock = self.run_lock(run_id);
        let _run_guard = run_lock.lock().await;
        let run = self.run_in_scope(scope, run_id)?;
        let payload = json!({
            "effect_id": effect.id,
            "scope": effect.scope,
            "idempotency_key": effect.idempotency_key,
            "tool_name": effect.tool_name,
            "fencing_token": effect.fencing_token,
            "detail": detail,
        });
        Ok(Some(self.build_event(
            scope,
            &run,
            event_type.to_string(),
            now,
            payload,
            now,
        )))
    }

    /// Construct the next event record for a run; caller must hold the run
    /// lock. The hash chain covers the OpenAPI view fields.
    fn build_event(
        &self,
        scope: &Scope,
        run: &RunRecord,
        event_type: String,
        occurred_at_ms: i64,
        payload: Value,
        now: i64,
    ) -> EventRecord {
        let mut event = EventRecord {
            schema_version: SCHEMA_VERSION,
            tenant_id: scope.tenant_id.clone(),
            project_id: scope.project_id.clone(),
            id: self.new_id(),
            run_id: run.id,
            sequence: run.last_event_sequence + 1,
            event_type,
            occurred_at_ms,
            recorded_at_ms: now,
            payload,
            previous_hash: run.head_hash.clone(),
            hash: String::new(),
        };
        event.hash = crate::model::compute_event_hash(&event.view());
        event
    }

    fn partition(&self, key: &EffectKey) -> &Arc<Mutex<()>> {
        let bytes = format!("{}\0{}\0{}", key.project_id, key.scope, key.idempotency_key);
        let hash = sha2::Sha256::digest(bytes.as_bytes());
        let index = u64::from_le_bytes(hash[..8].try_into().expect("8 bytes")) as usize;
        &self.partitions[index % self.partitions.len()]
    }

    fn new_id(&self) -> Uuid {
        let ms = self.clock.now_ms();
        let secs = u64::try_from(ms.div_euclid(1000)).unwrap_or(0);
        let nanos = u32::try_from(ms.rem_euclid(1000)).unwrap_or(0) * 1_000_000;
        Uuid::new_v7(uuid::Timestamp::from_unix(uuid::NoContext, secs, nanos))
    }

    fn run_lock(&self, run_id: Uuid) -> Arc<Mutex<()>> {
        self.indexes
            .run_locks
            .entry(run_id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    fn run_in_scope(&self, scope: &Scope, run_id: Uuid) -> Result<RunRecord> {
        self.indexes
            .runs
            .get(&run_id)
            .map(|r| r.clone())
            .filter(|r| r.tenant_id == scope.tenant_id && r.project_id == scope.project_id)
            .ok_or_else(|| LedgerError::NotFound {
                resource: "run",
                id: run_id.to_string(),
            })
    }

    fn effect_in_scope(&self, scope: &Scope, effect_id: Uuid) -> Result<EffectRecord> {
        self.indexes
            .effects_by_id
            .get(&effect_id)
            .map(|e| e.clone())
            .filter(|e| e.tenant_id == scope.tenant_id && e.project_id == scope.project_id)
            .ok_or_else(|| LedgerError::NotFound {
                resource: "effect",
                id: effect_id.to_string(),
            })
    }

    fn resolve_lease_ms(&self, seconds: Option<u64>, field: &'static str) -> Result<i64> {
        let duration = seconds.map_or(self.default_lease, Duration::from_secs);
        if duration < self.min_lease || duration > self.max_lease {
            return Err(LedgerError::Validation {
                field,
                message: format!(
                    "must be between {} and {} seconds",
                    self.min_lease.as_secs(),
                    self.max_lease.as_secs()
                ),
            });
        }
        Ok(i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
    }
}

/// New lease epoch for a safe reacquire: `claimed`, fence + 1, fresh token.
fn reacquire(
    mut effect: EffectRecord,
    req: &AcquireEffectRequest,
    lease_ms: i64,
    now: i64,
) -> (EffectRecord, Option<String>) {
    let lease_token = generate_lease_token();
    effect.state = EffectStatus::Claimed;
    effect.fencing_token += 1;
    effect.lease_expires_at_ms = Some(now + lease_ms);
    effect.lease_duration_ms = lease_ms;
    effect.lease_token_digest = Some(sha256_digest(lease_token.as_bytes()));
    effect.tool_name = req.tool_name.clone();
    effect.updated_at_ms = now;
    (effect, Some(lease_token))
}

fn wait_decision(effect: &EffectRecord, expires_ms: i64, now: i64) -> EffectDecision {
    EffectDecision {
        decision: EffectDecisionType::Wait,
        effect: effect.view(),
        lease_token: None,
        poll_after_ms: Some(u64::try_from(expires_ms - now).unwrap_or(0).max(1)),
    }
}

/// 256 random bits rendered `lease_<64 hex>`; only its SHA-256 is stored.
fn generate_lease_token() -> String {
    format!(
        "lease_{}{}",
        Uuid::new_v4().simple(),
        Uuid::new_v4().simple()
    )
}

fn validate_effect_fields(scope: &str, key: &str, tool: &str) -> Result<()> {
    if scope.is_empty() || scope.len() > 500 {
        return Err(LedgerError::Validation {
            field: "scope",
            message: "must be 1..=500 characters".to_string(),
        });
    }
    let key_ok = (8..=255).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'));
    if !key_ok {
        return Err(LedgerError::Validation {
            field: "idempotency_key",
            message: "must be 8..=255 characters matching ^[A-Za-z0-9._:-]+$".to_string(),
        });
    }
    if tool.is_empty() || tool.len() > 200 {
        return Err(LedgerError::Validation {
            field: "tool_name",
            message: "must be 1..=200 characters".to_string(),
        });
    }
    Ok(())
}

fn validate_event_type(event_type: &str) -> Result<()> {
    let ok = !event_type.is_empty()
        && event_type.len() <= 200
        && event_type.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-')
        });
    if !ok {
        return Err(LedgerError::Validation {
            field: "type",
            message: "must be 1..=200 characters matching ^[a-z0-9_.-]+$".to_string(),
        });
    }
    Ok(())
}

fn resolve_limit(limit: Option<u32>) -> Result<u32> {
    let limit = limit.unwrap_or(50);
    if !(1..=100).contains(&limit) {
        return Err(LedgerError::Validation {
            field: "limit",
            message: "must be between 1 and 100".to_string(),
        });
    }
    Ok(limit)
}

/// Cursor for `(created_at_ms | sequence, id)` orderings: `{sort_key:x}_{id}`.
fn encode_cursor(sort_key: i64, id: Uuid) -> String {
    format!("{sort_key:016x}_{}", id.simple())
}

fn decode_cursor(cursor: Option<&str>) -> Result<Option<(i64, Uuid)>> {
    decode_seq_id_cursor(cursor)
}

fn decode_seq_id_cursor(cursor: Option<&str>) -> Result<Option<(i64, Uuid)>> {
    let Some(cursor) = cursor else {
        return Ok(None);
    };
    let Some((key_hex, id_hex)) = cursor.split_once('_') else {
        return Err(LedgerError::Validation {
            field: "cursor",
            message: "malformed cursor".to_string(),
        });
    };
    let key = i64::from_str_radix(key_hex, 16).map_err(|_| LedgerError::Validation {
        field: "cursor",
        message: "malformed cursor".to_string(),
    })?;
    let id = Uuid::parse_str(id_hex).map_err(|_| LedgerError::Validation {
        field: "cursor",
        message: "malformed cursor".to_string(),
    })?;
    Ok(Some((key, id)))
}

/// Split `items` (already sorted) into one page plus pagination metadata.
fn paginate<T>(
    items: Vec<T>,
    limit: u32,
    cursor_of: impl Fn(&T) -> String,
) -> (Vec<T>, CursorPage) {
    let has_more = items.len() > limit as usize;
    let page: Vec<T> = items.into_iter().take(limit as usize).collect();
    let next_cursor = if has_more {
        page.last().map(&cursor_of)
    } else {
        None
    };
    (
        page,
        CursorPage {
            next_cursor,
            has_more,
            limit,
        },
    )
}
