//! Domain types: internal records (persisted, carrying `tenant_id`,
//! `project_id`, and `schema_version`) and API views mirroring
//! `docs/openapi.yaml` shapes so the REST layer wraps them 1:1.
//!
//! Timestamps are stored internally as milliseconds since the Unix epoch and
//! rendered RFC 3339 in API views.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::digest::{digest_bytes, format_timestamp, sha256_digest, GENESIS_HASH};

/// Schema version stamped on every persisted record.
pub const SCHEMA_VERSION: u32 = 1;

/// Tenant/project the calling API key is scoped to. Every ledger operation
/// takes one; resources outside it are invisible (reported as not found).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Scope {
    /// Tenant identifier.
    pub tenant_id: String,
    /// Project identifier.
    pub project_id: String,
}

// ---------------------------------------------------------------------------
// Effects
// ---------------------------------------------------------------------------

/// Internal effect state machine. `Claimed` has no OpenAPI `EffectState`
/// counterpart and is rendered as `in_progress` in API views (see crate
/// docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectStatus {
    /// Lease granted, tool invocation not yet explicitly started.
    Claimed,
    /// Worker explicitly started the tool invocation.
    InProgress,
    /// Result committed; replayable forever.
    Committed,
    /// Failed before the tool could have fired; safe to reacquire.
    FailedRetryable,
    /// Permanently failed; never reacquirable, replays the failure.
    FailedTerminal,
    /// Lease expired (or worker gave up) after the tool may have fired;
    /// requires manual reconciliation. Never auto-retried.
    OutcomeUnknown,
    /// Reconciled: the tool did fire; replays the reconciled receipt.
    ReconciledCommitted,
    /// Reconciled: the tool provably did not fire; behaves like
    /// `FailedTerminal`.
    ReconciledFailed,
}

impl EffectStatus {
    /// OpenAPI `EffectState` rendering of this internal state.
    pub fn api_state(self) -> EffectState {
        match self {
            Self::Claimed | Self::InProgress => EffectState::InProgress,
            Self::Committed => EffectState::Committed,
            Self::FailedRetryable => EffectState::FailedRetryable,
            Self::FailedTerminal => EffectState::FailedTerminal,
            Self::OutcomeUnknown => EffectState::OutcomeUnknown,
            Self::ReconciledCommitted => EffectState::ReconciledCommitted,
            Self::ReconciledFailed => EffectState::ReconciledFailed,
        }
    }

    /// Wire name, for events and error messages.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claimed => "claimed",
            Self::InProgress => "in_progress",
            Self::Committed => "committed",
            Self::FailedRetryable => "failed_retryable",
            Self::FailedTerminal => "failed_terminal",
            Self::OutcomeUnknown => "outcome_unknown",
            Self::ReconciledCommitted => "reconciled_committed",
            Self::ReconciledFailed => "reconciled_failed",
        }
    }
}

/// OpenAPI `EffectState` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectState {
    /// Lease held (claimed or started).
    InProgress,
    /// Result committed.
    Committed,
    /// Retryable failure; reacquirable.
    FailedRetryable,
    /// Terminal failure; replays the failure payload.
    FailedTerminal,
    /// Outcome cannot be proven; requires reconciliation.
    OutcomeUnknown,
    /// Reconciled as committed.
    ReconciledCommitted,
    /// Reconciled as failed.
    ReconciledFailed,
}

/// OpenAPI `EffectTransition` enum plus `Start`, which the OpenAPI enum
/// lacks; the brief requires an explicit `claimed -> in_progress` step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectTransition {
    /// `claimed -> in_progress` (worker, lease token + fencing token).
    Start,
    /// Extend the active lease (worker).
    Renew,
    /// Commit the result (worker).
    Commit,
    /// Record a retryable failure (worker).
    FailRetryable,
    /// Record a terminal failure (worker).
    FailTerminal,
    /// The worker can no longer prove the outcome (worker).
    MarkOutcomeUnknown,
    /// Reconcile: the tool did fire; record its receipt (reconciler).
    ReconcileCommitted,
    /// Reconcile: the tool provably did not fire (reconciler).
    ReconcileFailed,
}

impl EffectTransition {
    /// Whether this is a worker-owned transition (requires the active lease
    /// token) as opposed to a reconciliation (requires the reconcile
    /// capability instead).
    pub fn is_worker_transition(self) -> bool {
        matches!(
            self,
            Self::Start
                | Self::Renew
                | Self::Commit
                | Self::FailRetryable
                | Self::FailTerminal
                | Self::MarkOutcomeUnknown
        )
    }

    /// Wire name, for events and error messages.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Renew => "renew",
            Self::Commit => "commit",
            Self::FailRetryable => "fail_retryable",
            Self::FailTerminal => "fail_terminal",
            Self::MarkOutcomeUnknown => "mark_outcome_unknown",
            Self::ReconcileCommitted => "reconcile_committed",
            Self::ReconcileFailed => "reconcile_failed",
        }
    }
}

/// OpenAPI `EffectDecisionType` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectDecisionType {
    /// Caller owns a new lease and may invoke the tool (HTTP 201).
    Execute,
    /// Stored result/failure returned without tool execution (HTTP 200).
    Replay,
    /// Another valid lease owns the effect; poll later (HTTP 202).
    Wait,
    /// Previous outcome is unknown; reconcile, never retry blindly
    /// (HTTP 200).
    Reconcile,
}

/// OpenAPI `EffectLease`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectLease {
    /// RFC 3339 lease expiry.
    pub expires_at: String,
}

/// OpenAPI `ExternalReceipt`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExternalReceipt {
    /// Provider name (e.g. `stripe`).
    pub provider: String,
    /// Provider-side object id.
    pub id: String,
    /// Optional provider metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

/// OpenAPI `EffectFailure`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EffectFailure {
    /// Failure kind (e.g. `timeout_after_send`).
    pub kind: String,
    /// Human-readable detail.
    pub message: String,
    /// Whether the worker considers the failure retryable.
    #[serde(default)]
    pub retryable: bool,
}

/// Persisted effect record. `lease_token_digest` stores only the SHA-256 of
/// the bearer lease token, so the secret never touches disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectRecord {
    /// Record schema version.
    pub schema_version: u32,
    /// Owning tenant.
    pub tenant_id: String,
    /// Owning project.
    pub project_id: String,
    /// UUIDv7 effect id.
    pub id: Uuid,
    /// Run that owns this effect's timeline events, when any.
    pub run_id: Option<Uuid>,
    /// Effect scope.
    pub scope: String,
    /// Idempotency key.
    pub idempotency_key: String,
    /// Tool to invoke.
    pub tool_name: String,
    /// `sha256:<hex>` of the canonical input JSON.
    pub input_digest: String,
    /// Current machine state.
    pub state: EffectStatus,
    /// Current fencing token; incremented on every lease grant.
    pub fencing_token: i64,
    /// Lease expiry (ms since epoch) while a lease is active.
    pub lease_expires_at_ms: Option<i64>,
    /// Duration of the most recent lease grant, reused by `renew` when the
    /// request carries no explicit duration.
    pub lease_duration_ms: i64,
    /// SHA-256 digest of the active bearer lease token.
    pub lease_token_digest: Option<String>,
    /// Committed (or reconciled) result.
    pub result: Option<Value>,
    /// Provider receipt, when supplied.
    pub external_receipt: Option<ExternalReceipt>,
    /// Failure payload for failed/outcome-unknown states.
    pub failure: Option<EffectFailure>,
    /// Creation time (ms since epoch).
    pub created_at_ms: i64,
    /// Last mutation time (ms since epoch).
    pub updated_at_ms: i64,
}

impl EffectRecord {
    /// OpenAPI `Effect` view of this record.
    pub fn view(&self) -> Effect {
        Effect {
            id: self.id,
            run_id: self.run_id,
            scope: self.scope.clone(),
            idempotency_key: self.idempotency_key.clone(),
            tool_name: self.tool_name.clone(),
            input_digest: self.input_digest.clone(),
            state: self.state.api_state(),
            fencing_token: self.fencing_token,
            lease: self.lease_expires_at_ms.map(|ms| EffectLease {
                expires_at: format_timestamp(ms),
            }),
            result: self.result.clone(),
            external_receipt: self.external_receipt.clone(),
            failure: self.failure.clone(),
            created_at: format_timestamp(self.created_at_ms),
            updated_at: format_timestamp(self.updated_at_ms),
        }
    }
}

/// OpenAPI `Effect`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Effect {
    /// Effect id.
    pub id: Uuid,
    /// Owning run, when any.
    pub run_id: Option<Uuid>,
    /// Effect scope.
    pub scope: String,
    /// Idempotency key.
    pub idempotency_key: String,
    /// Tool name.
    pub tool_name: String,
    /// Canonical input digest.
    pub input_digest: String,
    /// Current state.
    pub state: EffectState,
    /// Current fencing token.
    pub fencing_token: i64,
    /// Active lease, when any.
    pub lease: Option<EffectLease>,
    /// Stored result.
    pub result: Option<Value>,
    /// Provider receipt.
    pub external_receipt: Option<ExternalReceipt>,
    /// Failure payload.
    pub failure: Option<EffectFailure>,
    /// RFC 3339 creation time.
    pub created_at: String,
    /// RFC 3339 last-mutation time.
    pub updated_at: String,
}

/// OpenAPI `EffectDecision`: the outcome of [`crate::Ledger::acquire`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EffectDecision {
    /// What the caller may do next.
    pub decision: EffectDecisionType,
    /// Current effect view.
    pub effect: Effect,
    /// Secret bearer capability; present only for an `execute` decision.
    pub lease_token: Option<String>,
    /// Suggested poll delay; present only for a `wait` decision.
    pub poll_after_ms: Option<u64>,
}

/// OpenAPI `AcquireEffectRequest`. The idempotency key itself arrives in the
/// `Idempotency-Key` header at the REST layer and is a separate
/// [`crate::Ledger::acquire`] parameter here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcquireEffectRequest {
    /// Run the effect's timeline events belong to.
    #[serde(default)]
    pub run_id: Option<Uuid>,
    /// Effect scope.
    pub scope: String,
    /// Tool to invoke.
    pub tool_name: String,
    /// Canonical input; digested for idempotency conflict detection.
    pub input: Value,
    /// Requested lease duration; default 30s, range 5..=3600s.
    #[serde(default)]
    pub lease_duration_seconds: Option<u64>,
    /// Hint that the downstream system supports its own idempotency keys.
    #[serde(default)]
    pub downstream_idempotency_supported: bool,
}

/// OpenAPI `UpdateEffectRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateEffectRequest {
    /// Requested transition.
    pub transition: EffectTransition,
    /// Active lease token; required for worker transitions.
    #[serde(default)]
    pub lease_token: Option<String>,
    /// Fencing token the caller believes is current.
    pub fencing_token: i64,
    /// New lease duration for `renew`.
    #[serde(default)]
    pub lease_duration_seconds: Option<u64>,
    /// Result payload for `commit` / `reconcile_committed`.
    #[serde(default)]
    pub result: Option<Value>,
    /// Provider receipt for `commit` / `reconcile_committed`.
    #[serde(default)]
    pub external_receipt: Option<ExternalReceipt>,
    /// Failure payload for `fail_*` / `mark_outcome_unknown` /
    /// `reconcile_failed`.
    #[serde(default)]
    pub failure: Option<EffectFailure>,
}

/// Caller capabilities granted by the authenticated API key. The REST layer
/// maps `effects:reconcile` to `can_reconcile`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CallerCapabilities {
    /// Whether the caller may perform reconcile transitions.
    pub can_reconcile: bool,
}

// ---------------------------------------------------------------------------
// Runs
// ---------------------------------------------------------------------------

/// OpenAPI `RunStatus` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Actively producing events.
    Running,
    /// Temporarily halted.
    Suspended,
    /// Finished successfully (terminal).
    Completed,
    /// Finished with failure (terminal).
    Failed,
    /// Aborted by the caller (terminal).
    Cancelled,
}

impl RunStatus {
    /// Whether the status is terminal (no further transitions allowed).
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    /// Wire name, for error messages.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Suspended => "suspended",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Persisted run record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    /// Record schema version.
    pub schema_version: u32,
    /// Owning tenant.
    pub tenant_id: String,
    /// Owning project.
    pub project_id: String,
    /// UUIDv7 run id.
    pub id: Uuid,
    /// Caller-supplied external id.
    pub external_id: String,
    /// Producing agent name.
    pub agent_name: String,
    /// Current status.
    pub status: RunStatus,
    /// Optimistic-concurrency version (the ETag), starts at 1.
    pub version: i64,
    /// Sequence of the run's latest event (0 = none).
    pub last_event_sequence: i64,
    /// Hash of the run's latest event; [`GENESIS_HASH`] when empty.
    pub head_hash: String,
    /// Parent run for forks.
    pub parent_run_id: Option<Uuid>,
    /// Event sequence the fork started from.
    pub forked_from_event_sequence: Option<i64>,
    /// Caller metadata.
    pub metadata: Option<Value>,
    /// Creation time (ms since epoch).
    pub created_at_ms: i64,
    /// Last mutation time (ms since epoch).
    pub updated_at_ms: i64,
}

impl RunRecord {
    /// OpenAPI `Run` view of this record.
    pub fn view(&self) -> Run {
        Run {
            id: self.id,
            external_id: self.external_id.clone(),
            agent_name: self.agent_name.clone(),
            status: self.status,
            version: self.version,
            last_event_sequence: self.last_event_sequence,
            parent_run_id: self.parent_run_id,
            forked_from_event_sequence: self.forked_from_event_sequence,
            metadata: self.metadata.clone(),
            created_at: format_timestamp(self.created_at_ms),
            updated_at: format_timestamp(self.updated_at_ms),
        }
    }
}

/// OpenAPI `Run`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Run {
    /// Run id.
    pub id: Uuid,
    /// Caller-supplied external id.
    pub external_id: String,
    /// Producing agent name.
    pub agent_name: String,
    /// Current status.
    pub status: RunStatus,
    /// Optimistic-concurrency version (the ETag).
    pub version: i64,
    /// Sequence of the run's latest event.
    pub last_event_sequence: i64,
    /// Parent run for forks.
    pub parent_run_id: Option<Uuid>,
    /// Event sequence the fork started from.
    pub forked_from_event_sequence: Option<i64>,
    /// Caller metadata.
    pub metadata: Option<Value>,
    /// RFC 3339 creation time.
    pub created_at: String,
    /// RFC 3339 last-mutation time.
    pub updated_at: String,
}

/// OpenAPI `CreateRunRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateRunRequest {
    /// Caller-supplied external id.
    pub external_id: String,
    /// Producing agent name.
    pub agent_name: String,
    /// Optional metadata.
    #[serde(default)]
    pub metadata: Option<Value>,
}

/// OpenAPI `UpdateRunRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateRunRequest {
    /// New status.
    pub status: RunStatus,
}

/// OpenAPI `ForkRunRequest`: exactly one of `from_event_sequence` /
/// `from_checkpoint_id` must be set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForkRunRequest {
    /// Fork point as an event sequence of the parent run.
    #[serde(default)]
    pub from_event_sequence: Option<i64>,
    /// Fork point as a checkpoint id of the parent run.
    #[serde(default)]
    pub from_checkpoint_id: Option<Uuid>,
    /// Why the run is being forked.
    pub reason: String,
    /// Optional metadata for the forked run.
    #[serde(default)]
    pub metadata: Option<Value>,
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// Persisted run event; one link in the run's hash chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRecord {
    /// Record schema version.
    pub schema_version: u32,
    /// Owning tenant.
    pub tenant_id: String,
    /// Owning project.
    pub project_id: String,
    /// UUIDv7 event id.
    pub id: Uuid,
    /// Owning run.
    pub run_id: Uuid,
    /// Per-run sequence; starts at 1, strictly monotonic, gap-free.
    pub sequence: i64,
    /// Event type (`^[a-z0-9_.-]+$`).
    pub event_type: String,
    /// When the event occurred (client-supplied, ms since epoch).
    pub occurred_at_ms: i64,
    /// When the ledger recorded it (ms since epoch).
    pub recorded_at_ms: i64,
    /// Event payload.
    pub payload: Value,
    /// Hash of the previous event; [`GENESIS_HASH`] for sequence 1.
    pub previous_hash: String,
    /// `sha256(previous_hash || canonical payload)`; see
    /// [`compute_event_hash`].
    pub hash: String,
}

impl EventRecord {
    /// OpenAPI `Event` view of this record.
    pub fn view(&self) -> Event {
        Event {
            id: self.id,
            run_id: self.run_id,
            sequence: self.sequence,
            event_type: self.event_type.clone(),
            occurred_at: format_timestamp(self.occurred_at_ms),
            recorded_at: format_timestamp(self.recorded_at_ms),
            payload: self.payload.clone(),
            previous_hash: self.previous_hash.clone(),
            hash: self.hash.clone(),
        }
    }
}

/// OpenAPI `Event`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Event {
    /// Event id.
    pub id: Uuid,
    /// Owning run.
    pub run_id: Uuid,
    /// Per-run sequence (starts at 1, gap-free).
    pub sequence: i64,
    /// Event type.
    #[serde(rename = "type")]
    pub event_type: String,
    /// RFC 3339 occurrence time (client-supplied).
    pub occurred_at: String,
    /// RFC 3339 recording time.
    pub recorded_at: String,
    /// Event payload.
    pub payload: Value,
    /// Hash of the previous event.
    pub previous_hash: String,
    /// This event's hash.
    pub hash: String,
}

/// The canonical bytes an event hash commits to: the raw 32 bytes of the
/// previous hash followed by the canonical JSON of
/// `{id, run_id, sequence, type, occurred_at, recorded_at, payload}` — the
/// OpenAPI `Event` fields minus the hash fields, with timestamps in their
/// RFC 3339 rendering. Defined over the *view* fields so verification works
/// on API views directly.
pub(crate) fn compute_event_hash(event: &Event) -> String {
    let input = json!({
        "id": event.id,
        "run_id": event.run_id,
        "sequence": event.sequence,
        "type": event.event_type,
        "occurred_at": event.occurred_at,
        "recorded_at": event.recorded_at,
        "payload": event.payload,
    });
    let previous = digest_bytes(&event.previous_hash).unwrap_or([0u8; 32]);
    let mut bytes = Vec::with_capacity(32 + 128);
    bytes.extend_from_slice(&previous);
    crate::digest::canonical_json_into(&input, &mut bytes);
    sha256_digest(&bytes)
}

/// Verify a run's event chain: sequences are 1..=N gap-free, hashes link,
/// and every event's hash recomputes. Returns `false` on any deviation
/// (tamper-evidence, not a legal claim).
pub fn verify_event_chain(events: &[Event]) -> bool {
    let mut expected_previous = GENESIS_HASH.to_string();
    for (index, event) in events.iter().enumerate() {
        if event.sequence != index as i64 + 1 {
            return false;
        }
        if event.previous_hash != expected_previous {
            return false;
        }
        if event.hash != compute_event_hash(event) {
            return false;
        }
        expected_previous = event.hash.clone();
    }
    true
}

/// OpenAPI `AppendEventRequest`. Timestamps arrive as RFC 3339 at the REST
/// layer; the library takes milliseconds since the epoch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppendEventRequest {
    /// Event type (`^[a-z0-9_.-]+$`).
    #[serde(rename = "type")]
    pub event_type: String,
    /// When the event occurred (ms since epoch).
    pub occurred_at_ms: i64,
    /// Event payload.
    pub payload: Value,
}

// ---------------------------------------------------------------------------
// Checkpoints
// ---------------------------------------------------------------------------

/// Persisted run checkpoint; immutable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointRecord {
    /// Record schema version.
    pub schema_version: u32,
    /// Owning tenant.
    pub tenant_id: String,
    /// Owning project.
    pub project_id: String,
    /// UUIDv7 checkpoint id.
    pub id: Uuid,
    /// Owning run.
    pub run_id: Uuid,
    /// Event sequence the checkpoint covers.
    pub event_sequence: i64,
    /// Opaque run state.
    pub state: Value,
    /// `sha256:<hex>` over the canonical bytes of `state`.
    pub state_digest: String,
    /// Creation time (ms since epoch).
    pub created_at_ms: i64,
}

impl CheckpointRecord {
    /// OpenAPI `Checkpoint` view of this record.
    pub fn view(&self) -> Checkpoint {
        Checkpoint {
            id: self.id,
            run_id: self.run_id,
            event_sequence: self.event_sequence,
            state: self.state.clone(),
            state_ref: None,
            state_digest: self.state_digest.clone(),
            created_at: format_timestamp(self.created_at_ms),
        }
    }
}

/// OpenAPI `Checkpoint`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Checkpoint {
    /// Checkpoint id.
    pub id: Uuid,
    /// Owning run.
    pub run_id: Uuid,
    /// Event sequence the checkpoint covers.
    pub event_sequence: i64,
    /// Opaque run state.
    pub state: Value,
    /// External state reference; always `null` for inline state.
    pub state_ref: Option<String>,
    /// Canonical state digest.
    pub state_digest: String,
    /// RFC 3339 creation time.
    pub created_at: String,
}

/// OpenAPI `CreateCheckpointRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateCheckpointRequest {
    /// Event sequence the checkpoint covers (must not exceed the run's
    /// `last_event_sequence`).
    pub event_sequence: i64,
    /// Run state to checkpoint.
    pub state: Value,
}

// ---------------------------------------------------------------------------
// Pagination and list filters
// ---------------------------------------------------------------------------

/// OpenAPI `CursorPage`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CursorPage {
    /// Opaque cursor for the next page; `null` when `has_more` is false.
    pub next_cursor: Option<String>,
    /// Whether more items exist beyond this page.
    pub has_more: bool,
    /// Page size used.
    pub limit: u32,
}

/// OpenAPI `RunList`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunList {
    /// Page of runs.
    pub data: Vec<Run>,
    /// Pagination metadata.
    pub pagination: CursorPage,
}

/// OpenAPI `EventList`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EventList {
    /// Page of events.
    pub data: Vec<Event>,
    /// Pagination metadata.
    pub pagination: CursorPage,
}

/// OpenAPI `CheckpointList`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CheckpointList {
    /// Page of checkpoints.
    pub data: Vec<Checkpoint>,
    /// Pagination metadata.
    pub pagination: CursorPage,
}

/// OpenAPI `EffectList`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EffectList {
    /// Page of effects.
    pub data: Vec<Effect>,
    /// Pagination metadata.
    pub pagination: CursorPage,
}

/// Filters for [`crate::Ledger::list_runs`]; `None` fields match anything.
#[derive(Debug, Clone, Default)]
pub struct RunFilter {
    /// Filter by run status.
    pub status: Option<RunStatus>,
    /// Filter by exact agent name.
    pub agent_name: Option<String>,
    /// Runs created at or after this time (ms since epoch).
    pub created_after_ms: Option<i64>,
}

/// Filters for [`crate::Ledger::list_effects`]; `None` fields match anything.
#[derive(Debug, Clone, Default)]
pub struct EffectFilter {
    /// Filter by run id.
    pub run_id: Option<Uuid>,
    /// Filter by effect state (API view semantics).
    pub state: Option<EffectState>,
    /// Filter by exact tool name.
    pub tool_name: Option<String>,
    /// Effects created at or after this time (ms since epoch).
    pub created_after_ms: Option<i64>,
}

/// Filters for [`crate::Ledger::list_events`]; `None` fields match anything.
#[derive(Debug, Clone, Default)]
pub struct EventFilter {
    /// Filter by exact event type.
    pub event_type: Option<String>,
    /// Only events with sequence strictly greater than this.
    pub after_sequence: Option<i64>,
}
