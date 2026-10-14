//! RFC 7807 `application/problem+json` errors.
//!
//! Every error the API returns — auth failures, validation, ledger errors —
//! is rendered as the OpenAPI `Problem` schema with a stable
//! `https://calod.dev/problems/<code>` type URI.

use axum::{
    body::Body,
    http::{header, HeaderValue, Response, StatusCode},
};
use serde::Serialize;

use calod_ledger::LedgerError;

use crate::context::RequestContext;

/// Problem `errors[]` entry (OpenAPI `ProblemField`).
#[derive(Debug, Clone, Serialize)]
pub struct ProblemField {
    /// Offending field or header, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// Machine-readable sub-code, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Human-readable explanation.
    pub message: String,
}

/// Problem `retry` advice (OpenAPI `RetryAdvice`).
#[derive(Debug, Clone, Serialize)]
pub struct RetryAdvice {
    /// Whether retrying may succeed.
    pub retryable: bool,
    /// Suggested delay before retrying.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,
}

/// The rendered `application/problem+json` body (OpenAPI `Problem`).
#[derive(Debug, Serialize)]
struct ProblemBody {
    #[serde(rename = "type")]
    type_uri: String,
    title: &'static str,
    status: u16,
    detail: String,
    instance: String,
    code: &'static str,
    request_id: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    errors: Vec<ProblemField>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retry: Option<RetryAdvice>,
}

/// Heap-allocated error payload. Kept behind a `Box` so `Result<T, ApiError>`
/// stays small on the happy path.
#[derive(Debug)]
struct ErrorState {
    detail: String,
    fields: Vec<ProblemField>,
    retry: Option<RetryAdvice>,
    request_id: String,
    instance: String,
}

/// An API error carrying everything needed to render the problem body.
/// Constructed with the [`RequestContext`] so the response `request_id` and
/// `instance` match the request.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    title: &'static str,
    /// `Retry-After` header (whole seconds), when set.
    retry_after: Option<u64>,
    state: Box<ErrorState>,
}

/// Human-readable title per stable problem code.
fn title_for(code: &str) -> &'static str {
    match code {
        "VALIDATION_ERROR" => "Validation error",
        "INVALID_API_KEY" => "Authentication failed",
        "INSUFFICIENT_SCOPE" => "Insufficient scope",
        "RESOURCE_NOT_FOUND" => "Resource not found",
        "IDEMPOTENCY_CONFLICT" => "Idempotency key conflict",
        "LEASE_LOST" => "Lease lost",
        "PRECONDITION_FAILED" => "Precondition failed",
        "INVALID_TRANSITION" => "Invalid effect transition",
        "PAYLOAD_TOO_LARGE" => "Payload too large",
        "SERVICE_UNAVAILABLE" => "Service unavailable",
        _ => "Internal error",
    }
}

/// `https://calod.dev/problems/<kebab-case-code>`.
fn type_uri_for(code: &str) -> String {
    format!(
        "https://calod.dev/problems/{}",
        code.to_lowercase().replace('_', "-")
    )
}

impl axum::response::IntoResponse for ApiError {
    fn into_response(self) -> Response<Body> {
        self.render()
    }
}

impl ApiError {
    fn new(ctx: &RequestContext, status: StatusCode, code: &'static str, detail: String) -> Self {
        Self {
            status,
            code,
            title: title_for(code),
            retry_after: None,
            state: Box::new(ErrorState {
                detail,
                fields: Vec::new(),
                retry: None,
                request_id: ctx.request_id.clone(),
                instance: ctx.instance.clone(),
            }),
        }
    }

    /// 400 `VALIDATION_ERROR` with a single `errors[]` field entry.
    pub fn validation(ctx: &RequestContext, field: &str, message: impl Into<String>) -> Self {
        let mut err = Self::new(
            ctx,
            StatusCode::BAD_REQUEST,
            "VALIDATION_ERROR",
            "The request contains invalid fields.".to_string(),
        );
        err.state.fields.push(ProblemField {
            field: Some(field.to_string()),
            code: None,
            message: message.into(),
        });
        err
    }

    /// 401 `INVALID_API_KEY` — missing, invalid, expired, or revoked key.
    pub fn unauthorized(ctx: &RequestContext) -> Self {
        Self::new(
            ctx,
            StatusCode::UNAUTHORIZED,
            "INVALID_API_KEY",
            "Supply a valid project key in X-Calod-Key.".to_string(),
        )
    }

    /// 403 `INSUFFICIENT_SCOPE` — the key lacks `effects:reconcile`.
    pub fn insufficient_scope(ctx: &RequestContext, detail: impl Into<String>) -> Self {
        Self::new(
            ctx,
            StatusCode::FORBIDDEN,
            "INSUFFICIENT_SCOPE",
            detail.into(),
        )
    }

    /// 404 `RESOURCE_NOT_FOUND` — unmatched routes.
    pub fn not_found(ctx: &RequestContext) -> Self {
        Self::new(
            ctx,
            StatusCode::NOT_FOUND,
            "RESOURCE_NOT_FOUND",
            "The requested resource does not exist.".to_string(),
        )
    }

    /// 413 `PAYLOAD_TOO_LARGE` — inline body over the configured limit.
    pub fn payload_too_large(ctx: &RequestContext, limit_bytes: usize) -> Self {
        Self::new(
            ctx,
            StatusCode::PAYLOAD_TOO_LARGE,
            "PAYLOAD_TOO_LARGE",
            format!(
                "Inline payloads are limited to {} MiB.",
                limit_bytes / (1024 * 1024)
            ),
        )
    }

    /// 503 `SERVICE_UNAVAILABLE` — durable writes are temporarily down.
    pub fn service_unavailable(ctx: &RequestContext, detail: impl Into<String>) -> Self {
        let mut err = Self::new(
            ctx,
            StatusCode::SERVICE_UNAVAILABLE,
            "SERVICE_UNAVAILABLE",
            detail.into(),
        );
        err.state.retry = Some(RetryAdvice {
            retryable: true,
            retry_after_seconds: Some(1),
        });
        err.retry_after = Some(1);
        err
    }

    /// Map a ledger error faithfully: [`LedgerError::status_code`] and
    /// [`LedgerError::code`] drive the response; 503s carry retry advice and
    /// a `Retry-After` header per the OpenAPI contract.
    pub fn from_ledger(ctx: &RequestContext, err: LedgerError) -> Self {
        let status =
            StatusCode::from_u16(err.status_code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let code = err.code();
        let mut api = Self::new(ctx, status, code, err.to_string());
        if status == StatusCode::SERVICE_UNAVAILABLE {
            api.state.retry = Some(RetryAdvice {
                retryable: true,
                retry_after_seconds: Some(1),
            });
            api.retry_after = Some(1);
        }
        api
    }

    /// Render the RFC 7807 response.
    fn render(self) -> Response<Body> {
        let body = ProblemBody {
            type_uri: type_uri_for(self.code),
            title: self.title,
            status: self.status.as_u16(),
            detail: self.state.detail,
            instance: self.state.instance,
            code: self.code,
            request_id: self.state.request_id.clone(),
            errors: self.state.fields,
            retry: self.state.retry,
        };
        let payload = serde_json::to_vec(&body)
            .unwrap_or_else(|_| b"{\"type\":\"about:blank\",\"title\":\"Internal error\",\"status\":500,\"detail\":\"failed to serialize problem\",\"instance\":\"\",\"code\":\"INTERNAL\",\"request_id\":\"\"}".to_vec());
        let mut builder = Response::builder()
            .status(self.status)
            .header(header::CONTENT_TYPE, "application/problem+json");
        if let Ok(value) = HeaderValue::from_str(&self.state.request_id) {
            builder = builder.header(header::HeaderName::from_static("x-request-id"), value);
        }
        if let Some(retry_after) = self.retry_after {
            builder = builder.header(header::RETRY_AFTER, retry_after.to_string());
        }
        builder
            .body(Body::from(payload))
            .unwrap_or_else(|_| Response::new(Body::empty()))
    }
}
