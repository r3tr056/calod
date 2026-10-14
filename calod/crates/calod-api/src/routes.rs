//! The `/v1` router: all 16 OpenAPI operations wrapping the ledger 1:1.

use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Extension, Path, State},
    http::{header, HeaderValue, Response, StatusCode, Uri},
    middleware,
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use serde::Serialize;
use serde_json::{json, Value};

use calod_core::store::api_keys::ApiKeyStore;
use calod_ledger::{
    AppendEventRequest, CreateCheckpointRequest, CreateRunRequest, EffectDecisionType,
    EffectFilter, EffectState, EventFilter, ForkRunRequest, Ledger, RunFilter, RunStatus,
    UpdateEffectRequest, UpdateRunRequest,
};

use crate::auth::{AuthContext, RECONCILE_SCOPE};
use crate::context::{self, RequestContext};
use crate::error::ApiError;
use crate::extract::{
    etag, parse_query, parse_rfc3339_ms, parse_uuid_path, IdempotencyKey, IfMatch, LimitedJson,
};

/// Shared state handed to every handler.
#[derive(Clone)]
pub struct AppState {
    ledger: Arc<Ledger>,
    api_keys: Arc<ApiKeyStore>,
    server_version: Arc<str>,
}

impl AppState {
    /// State over an open ledger, the server's API-key store, and the server
    /// version string reported by `/v1/version`.
    pub fn new(
        ledger: Arc<Ledger>,
        api_keys: Arc<ApiKeyStore>,
        server_version: impl Into<Arc<str>>,
    ) -> Self {
        Self {
            ledger,
            api_keys,
            server_version: server_version.into(),
        }
    }

    /// The scoped API-key store used for `X-Calod-Key` verification.
    pub fn api_keys(&self) -> &Arc<ApiKeyStore> {
        &self.api_keys
    }
}

/// Build the `/v1` router. [`context::middleware`] is layered over routes and
/// fallback so every response carries `X-Request-ID`.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/health/live", get(health_live))
        .route("/v1/health/ready", get(health_ready))
        .route("/v1/version", get(version))
        .route("/v1/runs", post(create_run).get(list_runs))
        .route("/v1/runs/{run_id}", get(get_run).patch(update_run))
        .route("/v1/runs/{run_id}/forks", post(fork_run))
        .route(
            "/v1/runs/{run_id}/events",
            post(append_run_event).get(list_run_events),
        )
        .route(
            "/v1/runs/{run_id}/checkpoints",
            post(create_checkpoint).get(list_checkpoints),
        )
        .route("/v1/effects", post(acquire_effect).get(list_effects))
        .route(
            "/v1/effects/{effect_id}",
            get(get_effect).patch(update_effect),
        )
        .fallback(not_found)
        .layer(middleware::from_fn(context::middleware))
        .with_state(state)
}

type ApiResult = Result<Response<Body>, ApiError>;

/// JSON response with extra headers (Location/ETag/Retry-After).
fn respond<T: Serialize>(
    status: StatusCode,
    body: &T,
    extra_headers: &[(&'static str, String)],
) -> Response<Body> {
    let payload = serde_json::to_vec(body).unwrap_or_else(|_| b"null".to_vec());
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json");
    for (name, value) in extra_headers {
        if let Ok(value) = HeaderValue::from_str(value) {
            builder = builder.header(*name, value);
        }
    }
    builder
        .body(Body::from(payload))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

fn run_etag_headers(version: i64) -> [(&'static str, String); 1] {
    [(header::ETAG.as_str(), etag(version))]
}

// ---------------------------------------------------------------------------
// System
// ---------------------------------------------------------------------------

/// `GET /v1/health/live` — process liveness; no auth, no ledger access.
async fn health_live() -> impl IntoResponse {
    respond(StatusCode::OK, &json!({ "status": "ok" }), &[])
}

/// `GET /v1/health/ready` — readiness means the ledger is open and its WAL
/// accepts durable writes (probed with a real flush).
async fn health_ready(
    State(state): State<AppState>,
    Extension(ctx): Extension<RequestContext>,
) -> ApiResult {
    state
        .ledger
        .flush()
        .await
        .map_err(|err| ApiError::from_ledger(&ctx, err))?;
    Ok(respond(StatusCode::OK, &json!({ "status": "ok" }), &[]))
}

/// `GET /v1/version` — API and server versions.
async fn version(State(state): State<AppState>) -> impl IntoResponse {
    respond(
        StatusCode::OK,
        &json!({
            "api_version": "v1",
            "server_version": state.server_version.as_ref(),
            "status": "alpha",
        }),
        &[],
    )
}

/// Fallback: unknown routes are 404 problems, not empty 404s.
async fn not_found(Extension(ctx): Extension<RequestContext>) -> ApiResult {
    Err(ApiError::not_found(&ctx))
}

// ---------------------------------------------------------------------------
// Runs
// ---------------------------------------------------------------------------

/// `POST /v1/runs` — create a run (201 + Location + ETag).
async fn create_run(
    State(state): State<AppState>,
    Extension(ctx): Extension<RequestContext>,
    auth: AuthContext,
    _key: IdempotencyKey,
    LimitedJson(req): LimitedJson<CreateRunRequest>,
) -> ApiResult {
    let run = state
        .ledger
        .create_run(&auth.scope, req)
        .await
        .map_err(|err| ApiError::from_ledger(&ctx, err))?;
    Ok(respond(
        StatusCode::CREATED,
        &run,
        &[
            (header::LOCATION.as_str(), format!("/v1/runs/{}", run.id)),
            (header::ETAG.as_str(), etag(run.version)),
        ],
    ))
}

#[derive(Debug, Default, serde::Deserialize)]
struct ListRunsQuery {
    cursor: Option<String>,
    limit: Option<u32>,
    status: Option<RunStatus>,
    agent_name: Option<String>,
    created_after: Option<String>,
}

/// `GET /v1/runs` — cursor-paginated, newest first.
async fn list_runs(
    State(state): State<AppState>,
    Extension(ctx): Extension<RequestContext>,
    auth: AuthContext,
    uri: Uri,
) -> ApiResult {
    let query: ListRunsQuery = parse_query(&ctx, &uri)?;
    let filter = RunFilter {
        status: query.status,
        agent_name: query.agent_name,
        created_after_ms: query
            .created_after
            .map(|value| parse_rfc3339_ms(&ctx, "created_after", &value))
            .transpose()?,
    };
    let list = state
        .ledger
        .list_runs(&auth.scope, &filter, query.cursor.as_deref(), query.limit)
        .await
        .map_err(|err| ApiError::from_ledger(&ctx, err))?;
    Ok(respond(StatusCode::OK, &list, &[]))
}

/// `GET /v1/runs/{run_id}` — 200 + ETag.
async fn get_run(
    State(state): State<AppState>,
    Extension(ctx): Extension<RequestContext>,
    auth: AuthContext,
    Path(run_id): Path<String>,
) -> ApiResult {
    let run_id = parse_uuid_path(&ctx, "run_id", &run_id)?;
    let run = state
        .ledger
        .get_run(&auth.scope, run_id)
        .await
        .map_err(|err| ApiError::from_ledger(&ctx, err))?;
    Ok(respond(
        StatusCode::OK,
        &run,
        &run_etag_headers(run.version),
    ))
}

/// `PATCH /v1/runs/{run_id}` — optimistic-concurrency transition; `If-Match`
/// is the expected run version (mismatch → 412).
async fn update_run(
    State(state): State<AppState>,
    Extension(ctx): Extension<RequestContext>,
    auth: AuthContext,
    Path(run_id): Path<String>,
    if_match: IfMatch,
    LimitedJson(req): LimitedJson<UpdateRunRequest>,
) -> ApiResult {
    let run_id = parse_uuid_path(&ctx, "run_id", &run_id)?;
    let run = state
        .ledger
        .update_run(&auth.scope, run_id, req, Some(if_match.0))
        .await
        .map_err(|err| ApiError::from_ledger(&ctx, err))?;
    Ok(respond(
        StatusCode::OK,
        &run,
        &run_etag_headers(run.version),
    ))
}

/// `POST /v1/runs/{run_id}/forks` — fork from an event sequence or a
/// checkpoint (201 + Location + ETag).
async fn fork_run(
    State(state): State<AppState>,
    Extension(ctx): Extension<RequestContext>,
    auth: AuthContext,
    Path(run_id): Path<String>,
    _key: IdempotencyKey,
    LimitedJson(req): LimitedJson<ForkRunRequest>,
) -> ApiResult {
    let run_id = parse_uuid_path(&ctx, "run_id", &run_id)?;
    let run = state
        .ledger
        .fork_run(&auth.scope, run_id, req)
        .await
        .map_err(|err| ApiError::from_ledger(&ctx, err))?;
    Ok(respond(
        StatusCode::CREATED,
        &run,
        &[
            (header::LOCATION.as_str(), format!("/v1/runs/{}", run.id)),
            (header::ETAG.as_str(), etag(run.version)),
        ],
    ))
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// Wire shape of `AppendEventRequest`: `occurred_at` arrives as RFC 3339; the
/// ledger takes milliseconds since the epoch.
#[derive(Debug, serde::Deserialize)]
struct AppendEventBody {
    #[serde(rename = "type")]
    event_type: String,
    occurred_at: String,
    payload: Value,
}

/// `POST /v1/runs/{run_id}/events` — append an immutable event (201).
async fn append_run_event(
    State(state): State<AppState>,
    Extension(ctx): Extension<RequestContext>,
    auth: AuthContext,
    Path(run_id): Path<String>,
    _key: IdempotencyKey,
    LimitedJson(body): LimitedJson<AppendEventBody>,
) -> ApiResult {
    let run_id = parse_uuid_path(&ctx, "run_id", &run_id)?;
    let occurred_at_ms = parse_rfc3339_ms(&ctx, "occurred_at", &body.occurred_at)?;
    let event = state
        .ledger
        .append_event(
            &auth.scope,
            run_id,
            AppendEventRequest {
                event_type: body.event_type,
                occurred_at_ms,
                payload: body.payload,
            },
        )
        .await
        .map_err(|err| ApiError::from_ledger(&ctx, err))?;
    Ok(respond(
        StatusCode::CREATED,
        &event,
        &[(
            header::LOCATION.as_str(),
            format!("/v1/runs/{run_id}/events/{}", event.sequence),
        )],
    ))
}

#[derive(Debug, Default, serde::Deserialize)]
struct ListEventsQuery {
    cursor: Option<String>,
    limit: Option<u32>,
    #[serde(rename = "type")]
    event_type: Option<String>,
    after_sequence: Option<i64>,
}

/// `GET /v1/runs/{run_id}/events` — cursor-paginated, sequence ascending.
async fn list_run_events(
    State(state): State<AppState>,
    Extension(ctx): Extension<RequestContext>,
    auth: AuthContext,
    Path(run_id): Path<String>,
    uri: Uri,
) -> ApiResult {
    let run_id = parse_uuid_path(&ctx, "run_id", &run_id)?;
    let query: ListEventsQuery = parse_query(&ctx, &uri)?;
    let filter = EventFilter {
        event_type: query.event_type,
        after_sequence: query.after_sequence,
    };
    let list = state
        .ledger
        .list_events(
            &auth.scope,
            run_id,
            &filter,
            query.cursor.as_deref(),
            query.limit,
        )
        .await
        .map_err(|err| ApiError::from_ledger(&ctx, err))?;
    Ok(respond(StatusCode::OK, &list, &[]))
}

// ---------------------------------------------------------------------------
// Checkpoints
// ---------------------------------------------------------------------------

/// `POST /v1/runs/{run_id}/checkpoints` — create an immutable checkpoint
/// (201 + Location + ETag of the run version the checkpoint was checked
/// against via `If-Match`).
async fn create_checkpoint(
    State(state): State<AppState>,
    Extension(ctx): Extension<RequestContext>,
    auth: AuthContext,
    Path(run_id): Path<String>,
    if_match: IfMatch,
    _key: IdempotencyKey,
    LimitedJson(req): LimitedJson<CreateCheckpointRequest>,
) -> ApiResult {
    let run_id = parse_uuid_path(&ctx, "run_id", &run_id)?;
    let checkpoint = state
        .ledger
        .create_checkpoint(&auth.scope, run_id, req, Some(if_match.0))
        .await
        .map_err(|err| ApiError::from_ledger(&ctx, err))?;
    // The checkpoint itself is immutable and versionless; the ETag echoes the
    // run's current version (the resource `If-Match` guards).
    let run = state
        .ledger
        .get_run(&auth.scope, run_id)
        .await
        .map_err(|err| ApiError::from_ledger(&ctx, err))?;
    Ok(respond(
        StatusCode::CREATED,
        &checkpoint,
        &[
            (
                header::LOCATION.as_str(),
                format!("/v1/runs/{run_id}/checkpoints/{}", checkpoint.id),
            ),
            (header::ETAG.as_str(), etag(run.version)),
        ],
    ))
}

#[derive(Debug, Default, serde::Deserialize)]
struct ListCheckpointsQuery {
    cursor: Option<String>,
    limit: Option<u32>,
}

/// `GET /v1/runs/{run_id}/checkpoints` — cursor-paginated, event sequence
/// descending.
async fn list_checkpoints(
    State(state): State<AppState>,
    Extension(ctx): Extension<RequestContext>,
    auth: AuthContext,
    Path(run_id): Path<String>,
    uri: Uri,
) -> ApiResult {
    let run_id = parse_uuid_path(&ctx, "run_id", &run_id)?;
    let query: ListCheckpointsQuery = parse_query(&ctx, &uri)?;
    let list = state
        .ledger
        .list_checkpoints(&auth.scope, run_id, query.cursor.as_deref(), query.limit)
        .await
        .map_err(|err| ApiError::from_ledger(&ctx, err))?;
    Ok(respond(StatusCode::OK, &list, &[]))
}

// ---------------------------------------------------------------------------
// Effects
// ---------------------------------------------------------------------------

/// `POST /v1/effects` — acquire or inspect an idempotent effect.
///
/// Decision → status mapping per the contract: `execute` → 201 (+ Location),
/// `replay`/`reconcile` → 200, `wait` → 202 with `Retry-After` derived from
/// `poll_after_ms` (whole seconds, minimum 1).
async fn acquire_effect(
    State(state): State<AppState>,
    Extension(ctx): Extension<RequestContext>,
    auth: AuthContext,
    key: IdempotencyKey,
    LimitedJson(req): LimitedJson<calod_ledger::AcquireEffectRequest>,
) -> ApiResult {
    let decision = state
        .ledger
        .acquire(&auth.scope, &key.0, req)
        .await
        .map_err(|err| ApiError::from_ledger(&ctx, err))?;
    let etag = etag(decision.effect.fencing_token);
    let response = match decision.decision {
        EffectDecisionType::Execute => respond(
            StatusCode::CREATED,
            &decision,
            &[
                (
                    header::LOCATION.as_str(),
                    format!("/v1/effects/{}", decision.effect.id),
                ),
                (header::ETAG.as_str(), etag),
            ],
        ),
        EffectDecisionType::Replay | EffectDecisionType::Reconcile => {
            respond(StatusCode::OK, &decision, &[(header::ETAG.as_str(), etag)])
        }
        EffectDecisionType::Wait => {
            let retry_after = decision
                .poll_after_ms
                .map(|ms| ms.div_ceil(1000).max(1))
                .unwrap_or(1);
            respond(
                StatusCode::ACCEPTED,
                &decision,
                &[(header::RETRY_AFTER.as_str(), retry_after.to_string())],
            )
        }
    };
    Ok(response)
}

#[derive(Debug, Default, serde::Deserialize)]
struct ListEffectsQuery {
    cursor: Option<String>,
    limit: Option<u32>,
    run_id: Option<String>,
    state: Option<EffectState>,
    tool_name: Option<String>,
    created_after: Option<String>,
}

/// `GET /v1/effects` — cursor-paginated, newest first.
async fn list_effects(
    State(state): State<AppState>,
    Extension(ctx): Extension<RequestContext>,
    auth: AuthContext,
    uri: Uri,
) -> ApiResult {
    let query: ListEffectsQuery = parse_query(&ctx, &uri)?;
    let filter = EffectFilter {
        run_id: query
            .run_id
            .map(|value| parse_uuid_path(&ctx, "run_id", &value))
            .transpose()?,
        state: query.state,
        tool_name: query.tool_name,
        created_after_ms: query
            .created_after
            .map(|value| parse_rfc3339_ms(&ctx, "created_after", &value))
            .transpose()?,
    };
    let list = state
        .ledger
        .list_effects(&auth.scope, &filter, query.cursor.as_deref(), query.limit)
        .await
        .map_err(|err| ApiError::from_ledger(&ctx, err))?;
    Ok(respond(StatusCode::OK, &list, &[]))
}

/// `GET /v1/effects/{effect_id}` — 200 + ETag (the fencing token is the
/// effect's version).
async fn get_effect(
    State(state): State<AppState>,
    Extension(ctx): Extension<RequestContext>,
    auth: AuthContext,
    Path(effect_id): Path<String>,
) -> ApiResult {
    let effect_id = parse_uuid_path(&ctx, "effect_id", &effect_id)?;
    let effect = state
        .ledger
        .get_effect(&auth.scope, effect_id)
        .await
        .map_err(|err| ApiError::from_ledger(&ctx, err))?;
    Ok(respond(
        StatusCode::OK,
        &effect,
        &[(header::ETAG.as_str(), etag(effect.fencing_token))],
    ))
}

/// `PATCH /v1/effects/{effect_id}` — transition, renew, or reconcile.
///
/// The ledger's `transition` has no `expected_version` parameter, so the
/// `If-Match` precondition is enforced at the API layer against the current
/// fencing token (the effect's version); a mismatch is 412 before any state
/// change. Reconcile transitions are pre-checked against the key's
/// `effects:reconcile` scope (403) and re-checked inside the ledger via
/// [`calod_ledger::CallerCapabilities`].
async fn update_effect(
    State(state): State<AppState>,
    Extension(ctx): Extension<RequestContext>,
    auth: AuthContext,
    Path(effect_id): Path<String>,
    if_match: IfMatch,
    LimitedJson(req): LimitedJson<UpdateEffectRequest>,
) -> ApiResult {
    let effect_id = parse_uuid_path(&ctx, "effect_id", &effect_id)?;
    if !req.transition.is_worker_transition() && !auth.capabilities.can_reconcile {
        return Err(ApiError::insufficient_scope(
            &ctx,
            format!("This transition requires {RECONCILE_SCOPE}."),
        ));
    }
    let current = state
        .ledger
        .get_effect(&auth.scope, effect_id)
        .await
        .map_err(|err| ApiError::from_ledger(&ctx, err))?;
    if current.fencing_token != if_match.0 {
        return Err(ApiError::from_ledger(
            &ctx,
            calod_ledger::LedgerError::PreconditionFailed {
                expected: if_match.0,
                current: current.fencing_token,
            },
        ));
    }
    let effect = state
        .ledger
        .transition(&auth.scope, effect_id, req, auth.capabilities)
        .await
        .map_err(|err| ApiError::from_ledger(&ctx, err))?;
    Ok(respond(
        StatusCode::OK,
        &effect,
        &[(header::ETAG.as_str(), etag(effect.fencing_token))],
    ))
}
