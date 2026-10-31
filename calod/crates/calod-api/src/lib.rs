//! REST API for the Calod action ledger (phase P2).
//!
//! An axum HTTP layer that wraps [`calod_ledger::Ledger`] 1:1 according to
//! `docs/openapi.yaml`: all 16 operations live under `/v1`, errors are RFC
//! 7807 `application/problem+json` bodies with stable `type` URIs and a
//! per-request `X-Request-ID`, optimistic concurrency rides on ETag/`If-Match`,
//! and list endpoints use the ledger's cursor pagination unchanged.
//!
//! # Authentication and scoping
//!
//! Every ledger endpoint requires the `X-Calod-Key` header (OpenAPI
//! `ApiKeyAuth`). The secret is verified against
//! [`calod_core::store::api_keys::ApiKeyStore`]; the verified key's
//! tenant/project is injected as the [`calod_ledger::Scope`] of every ledger
//! call, so cross-tenant resources are invisible (404). The
//! `effects:reconcile` key scope maps to
//! [`calod_ledger::CallerCapabilities::can_reconcile`]; reconcile transitions
//! without it get 403. `/v1/health/live`, `/v1/health/ready`, and
//! `/v1/version` are unauthenticated.
//!
//! # Error mapping
//!
//! [`calod_ledger::LedgerError::status_code`] / `code()` are mapped
//! faithfully: 404 `RESOURCE_NOT_FOUND`, 409 `IDEMPOTENCY_CONFLICT` /
//! `LEASE_LOST`, 403 `INSUFFICIENT_SCOPE`, 422 `INVALID_TRANSITION`, 400
//! `VALIDATION_ERROR`, 412 `PRECONDITION_FAILED`, 503 `SERVICE_UNAVAILABLE`.
//! The API layer adds 401 `INVALID_API_KEY` and 413 `PAYLOAD_TOO_LARGE`
//! (inline bodies are capped at 1 MiB).

mod auth;
mod context;
mod error;
mod export;
mod extract;
mod routes;
mod server;

pub use auth::AuthContext;
pub use calod_ledger::RetentionPolicy;
pub use context::RequestContext;
pub use error::ApiError;
pub use export::load_or_create_export_key;
pub use routes::{router, AppState};
pub use server::{open_ledger, open_ledger_with_retention, serve};
