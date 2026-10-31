//! Calod effect ledger: the durable domain core for idempotent AI-agent
//! action coordination.
//!
//! An **effect** is one idempotent external action coordinated by
//! `(tenant_id, project_id, scope, idempotency_key)`. [`Ledger::acquire`]
//! atomically decides whether the caller may `execute` (new lease), must
//! `wait` (live foreign lease), gets a `replay` (committed or terminally
//! failed receipt), or must `reconcile` (`outcome_unknown` — the tool may
//! have fired; never auto-retried).
//!
//! # Correctness invariants
//!
//! - **Fencing**: every lease grant increments the effect's fencing token.
//!   Worker transitions present the active lease token and the current
//!   fencing token; a stale fencing token can never move the state machine.
//! - **Per-partition linearizability**: `(project_id, scope,
//!   idempotency_key)` hashes to one of 64 partitions guarded by an async
//!   mutex; the whole acquire/transition decision for a key happens under
//!   its partition lock.
//! - **WAL-first durability**: a state mutation is appended to the WAL and
//!   acknowledged per the configured durability class *before* the
//!   in-memory indexes are touched. An effect update and its run-timeline
//!   event travel in **one WAL record**, so a crash cannot tear them.
//! - **Tamper-evident run timelines**: events form a per-run hash chain
//!   (`hash = sha256(previous_hash || canonical payload)`), gap-free and
//!   strictly monotonic in `sequence`.
//!
//! Time is injected via [`Clock`] so every lease-expiry branch is
//! deterministically testable.
//!
//! The public shapes mirror `docs/openapi.yaml` so the REST layer wraps
//! this crate 1:1. Two documented deviations, forced by the product
//! semantics:
//!
//! - the OpenAPI `EffectState` enum has no `claimed`; the internal machine
//!   state [`EffectStatus::Claimed`] is rendered as
//!   [`EffectState::InProgress`] in API views (matching the OpenAPI `201
//!   execute` example, which shows `in_progress`);
//! - the OpenAPI `EffectTransition` enum has no `start`; the crate adds
//!   [`EffectTransition::Start`] because the brief requires an explicit
//!   `claimed -> in_progress` step to keep the claimed-expiry (safe
//!   reacquire) vs in-progress-expiry (reconcile) distinction truthful.

mod clock;
mod digest;
mod error;
mod ledger;
mod model;
mod record;
mod snapshot;

pub use clock::{Clock, ManualClock, SystemClock};
pub use digest::{canonical_json, input_digest, sha256_digest, GENESIS_HASH};
pub use error::{LedgerError, Result};
pub use ledger::{Ledger, LedgerConfig, LedgerStats, DEFAULT_PARTITIONS};
pub use model::{
    verify_event_chain, AcquireEffectRequest, AppendEventRequest, CallerCapabilities, Checkpoint,
    CheckpointList, CreateCheckpointRequest, CreateRunRequest, CursorPage, Effect, EffectDecision,
    EffectDecisionType, EffectFailure, EffectFilter, EffectLease, EffectList, EffectState,
    EffectStatus, EffectTransition, Event, EventFilter, EventList, ExternalReceipt, ForkRunRequest,
    RetentionPolicy, RetentionReport, Run, RunExport, RunFilter, RunList, RunStatus, Scope,
    UpdateEffectRequest, UpdateRunRequest, SCHEMA_VERSION,
};
pub use snapshot::LedgerSnapshotManifest;
