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
    EffectFilter, EffectState, EventFilter, ForkRunRequest, Ledger, RetentionPolicy, RunFilter,
    RunStatus, UpdateEffectRequest, UpdateRunRequest,
};

use crate::auth::{AuthContext, RECONCILE_SCOPE};
use crate::context::{self, RequestContext};
use crate::error::ApiError;
use crate::export;
use crate::extract::{
    etag, parse_query, parse_rfc3339_ms, parse_uuid_path, IdempotencyKey, IfMatch, LimitedJson,
};

/// Shared state handed to every handler.
#[derive(Clone)]
pub struct AppState {
    ledger: Arc<Ledger>,
    api_keys: Arc<ApiKeyStore>,
    server_version: Arc<str>,
    export_key: Arc<Vec<u8>>,
}

impl AppState {
    /// State over an open ledger, the server's API-key store, and the server
    /// version string reported by `/v1/version`.
    pub fn new(
        ledger: Arc<Ledger>,
        api_keys: Arc<ApiKeyStore>,
        server_version: impl Into<Arc<str>>,
    ) -> Self {
        // Ephemeral export signing key unless `with_export_key`
        // installs the persisted one; signatures from an ephemeral
        // key cannot be verified after a restart.
        let mut ephemeral = Vec::with_capacity(32);
        ephemeral.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
        ephemeral.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
        Self {
            ledger,
            api_keys,
            server_version: server_version.into(),
            export_key: Arc::new(ephemeral),
        }
    }

    /// Install the persisted export signing key (see
    /// [`crate::export::load_or_create_export_key`]).
    pub fn with_export_key(mut self, key: Vec<u8>) -> Self {
        self.export_key = Arc::new(key);
        self
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
        .route("/v1/runs/{run_id}/export", get(export_run))
        .route("/v1/admin/retention", post(apply_retention))
        .route("/metrics", get(metrics))
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

#[derive(Debug, Default, serde::Deserialize)]
struct ExportQuery {
    format: Option<String>,
}

/// `GET /v1/runs/{run_id}/export?format=json|csv` — signed evidence
/// export (Phase 1). JSON is the full bundle with an HMAC-signed
/// manifest; CSV is the event timeline with digest/signature in
/// response headers.
async fn export_run(
    State(state): State<AppState>,
    Extension(ctx): Extension<RequestContext>,
    auth: AuthContext,
    Path(raw_id): Path<String>,
    uri: Uri,
) -> ApiResult {
    let run_id = parse_uuid_path(&ctx, "run_id", &raw_id)?;
    let query: ExportQuery = parse_query(&ctx, &uri)?;
    let export = state
        .ledger
        .export_run(&auth.scope, run_id)
        .await
        .map_err(|err| ApiError::from_ledger(&ctx, err))?;
    match query.format.as_deref().unwrap_or("json") {
        "json" => {
            let exported_at = {
                let secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64;
                chrono::DateTime::from_timestamp(secs, 0)
                    .map(|dt| dt.to_rfc3339())
                    .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string())
            };
            let bundle = export::render_json_export(&export, &state.export_key, &exported_at);
            Ok(respond(StatusCode::OK, &bundle, &[]))
        }
        "csv" => {
            let csv = export::render_events_csv(&export);
            let (digest, signature) = export::csv_manifest(&csv, &state.export_key);
            let builder = Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/csv")
                .header("X-Calod-Bundle-Sha256", digest)
                .header("X-Calod-Export-Signature", signature);
            builder
                .body(Body::from(csv))
                .map_err(|_| ApiError::service_unavailable(&ctx, "export render failed"))
        }
        other => Err(ApiError::validation(
            &ctx,
            "format",
            format!("unsupported export format '{other}' (expected json or csv)"),
        )),
    }
}

#[derive(Debug, Default, serde::Deserialize)]
struct RetentionBody {
    max_age_days: Option<u64>,
    #[serde(default = "default_true")]
    compact: bool,
}

fn default_true() -> bool {
    true
}

/// `POST /v1/admin/retention` — apply retention (requires the
/// `admin` key scope), then snapshot to compact the WAL when
/// `compact` is set. Prunes across all tenants; handle with care.
async fn apply_retention(
    State(state): State<AppState>,
    Extension(ctx): Extension<RequestContext>,
    auth: AuthContext,
    LimitedJson(req): LimitedJson<RetentionBody>,
) -> ApiResult {
    if !auth.capabilities.can_admin {
        return Err(ApiError::insufficient_scope(
            &ctx,
            "retention requires the admin API-key scope",
        ));
    }
    let policy = match req.max_age_days {
        Some(days) => RetentionPolicy {
            max_age_ms: (days * 86_400_000) as i64,
        },
        None => state.ledger.retention_policy().ok_or_else(|| {
            ApiError::validation(
                &ctx,
                "max_age_days",
                "no retention policy configured; supply max_age_days",
            )
        })?,
    };
    let report = state
        .ledger
        .apply_retention(policy)
        .await
        .map_err(|err| ApiError::from_ledger(&ctx, err))?;
    if req.compact && (report.runs_removed > 0 || report.effects_removed > 0) {
        state
            .ledger
            .snapshot()
            .await
            .map_err(|err| ApiError::from_ledger(&ctx, err))?;
    }
    Ok(respond(StatusCode::OK, &report, &[]))
}

/// `GET /metrics` — Prometheus text exposition for the ledger and
/// API (unauthenticated, like the health endpoints).
async fn metrics(State(state): State<AppState>) -> impl IntoResponse {
    let stats = state.ledger.stats();
    let mut out = String::new();
    out.push_str(
        "# HELP calod_api_up API server is up.\n# TYPE calod_api_up gauge\ncalod_api_up 1\n",
    );
    let counters = [
        ("execute", stats.acquires_execute),
        ("replay", stats.acquires_replay),
        ("wait", stats.acquires_wait),
        ("reconcile", stats.acquires_reconcile),
    ];
    out.push_str("# HELP calod_acquires_total Effect acquires by decision.\n# TYPE calod_acquires_total counter\n");
    for (decision, value) in counters {
        out.push_str(&format!(
            "calod_acquires_total{{decision=\"{decision}\"}} {value}\n"
        ));
    }
    out.push_str(&format!(
        "# HELP calod_transitions_total Successful effect transitions.\n# TYPE calod_transitions_total counter\ncalod_transitions_total {}\n",
        stats.transitions_total
    ));
    out.push_str(&format!(
        "# HELP calod_retention_removed_total Records removed by retention.\n# TYPE calod_retention_removed_total counter\ncalod_retention_removed_total{{kind=\"runs\"}} {}\ncalod_retention_removed_total{{kind=\"effects\"}} {}\n",
        stats.retention_runs_removed, stats.retention_effects_removed
    ));
    out.push_str(&format!(
        "# HELP calod_runs_current Runs currently indexed.\n# TYPE calod_runs_current gauge\ncalod_runs_current {}\n",
        stats.runs_current
    ));
    out.push_str(
        "# HELP calod_effects_current Effects currently indexed by state.\n# TYPE calod_effects_current gauge\n",
    );
    for (state_name, value) in &stats.effects_by_state {
        out.push_str(&format!(
            "calod_effects_current{{state=\"{state_name}\"}} {value}\n"
        ));
    }
    out.push_str(&format!(
        "# HELP calod_events_current Events currently indexed.\n# TYPE calod_events_current gauge\ncalod_events_current {}\n",
        stats.events_current
    ));
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/plain; version=0.0.4")
        .body(Body::from(out))
        .unwrap_or_else(|_| Response::new(Body::empty()))
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
