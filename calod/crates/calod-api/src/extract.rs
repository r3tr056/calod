//! Request extractors: `If-Match` ETags, the `Idempotency-Key` header, and a
//! body-size-capped JSON extractor that renders 413 as `application/problem+json`.

use axum::{
    extract::FromRequestParts,
    http::{request::Parts, HeaderMap, Uri},
};
use serde::de::DeserializeOwned;

use crate::context::RequestContext;
use crate::error::ApiError;

/// Inline JSON bodies are capped at 1 MiB (OpenAPI `PayloadTooLarge`).
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Render an ETag for a resource version: strong, quoted (`"7"`).
pub fn etag(version: i64) -> String {
    format!("\"{version}\"")
}

/// Parsed `If-Match` precondition: the expected resource version.
///
/// Accepts the strong form (`"7"`), a weak tag (`W/"7"`), or a bare integer.
/// Missing or unparsable values are 400 `VALIDATION_ERROR` — the header is
/// `required: true` in the OpenAPI contract on every endpoint that takes it.
#[derive(Debug, Clone, Copy)]
pub struct IfMatch(pub i64);

impl<S> FromRequestParts<S> for IfMatch
where
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let ctx = RequestContext::from_extensions(&parts.extensions);
        parse_if_match(&parts.headers)
            .map(Self)
            .ok_or_else(|| ApiError::validation(&ctx, "If-Match", "missing or malformed ETag"))
    }
}

fn parse_if_match(headers: &HeaderMap) -> Option<i64> {
    let raw = headers
        .get(axum::http::header::IF_MATCH)?
        .to_str()
        .ok()?
        .trim();
    let token = raw.strip_prefix("W/").unwrap_or(raw).trim_matches('"');
    let version = token.parse::<i64>().ok()?;
    (version >= 1).then_some(version)
}

/// The validated `Idempotency-Key` header.
///
/// Required on every mutating POST in the contract; must match
/// `^[A-Za-z0-9._:-]+$` and be 8..=255 characters (the same rule the ledger
/// enforces for effect keys).
#[derive(Debug, Clone)]
pub struct IdempotencyKey(pub String);

impl<S> FromRequestParts<S> for IdempotencyKey
where
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let ctx = RequestContext::from_extensions(&parts.extensions);
        let key = parts
            .headers
            .get("idempotency-key")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
            .ok_or_else(|| ApiError::validation(&ctx, "Idempotency-Key", "header is required"))?;
        let ok = (8..=255).contains(&key.len())
            && key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'));
        if !ok {
            return Err(ApiError::validation(
                &ctx,
                "Idempotency-Key",
                "must be 8..=255 characters matching ^[A-Za-z0-9._:-]+$",
            ));
        }
        Ok(Self(key))
    }
}

/// JSON body with a hard size cap and problem+json rejections.
///
/// Unlike axum's `Json`, an over-limit or malformed body is a rendered
/// [`ApiError`] (413 / 400), never a plain-text rejection.
#[derive(Debug)]
pub struct LimitedJson<T>(pub T);

impl<S, T> axum::extract::FromRequest<S> for LimitedJson<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = ApiError;

    async fn from_request(
        req: axum::extract::Request,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        let ctx = RequestContext::from_extensions(req.extensions());
        if let Some(len) = req
            .headers()
            .get(axum::http::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<usize>().ok())
        {
            if len > MAX_BODY_BYTES {
                return Err(ApiError::payload_too_large(&ctx, MAX_BODY_BYTES));
            }
        }
        let bytes = axum::body::to_bytes(req.into_body(), MAX_BODY_BYTES)
            .await
            .map_err(|_| ApiError::payload_too_large(&ctx, MAX_BODY_BYTES))?;
        serde_json::from_slice(&bytes)
            .map_err(|err| ApiError::validation(&ctx, "body", format!("invalid JSON body: {err}")))
            .map(Self)
    }
}

/// Parse the query string into `T`, mapping any failure to a 400 problem.
pub fn parse_query<T: DeserializeOwned>(ctx: &RequestContext, uri: &Uri) -> Result<T, ApiError> {
    serde_urlencoded::from_str(uri.query().unwrap_or("")).map_err(|err| {
        ApiError::validation(ctx, "query", format!("invalid query parameters: {err}"))
    })
}

/// Parse an RFC 3339 timestamp into milliseconds since the Unix epoch.
pub fn parse_rfc3339_ms(ctx: &RequestContext, field: &str, value: &str) -> Result<i64, ApiError> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.timestamp_millis())
        .map_err(|_| ApiError::validation(ctx, field, "must be an RFC 3339 timestamp"))
}

/// Parse a UUID path segment, mapping failure to a 400 problem.
pub fn parse_uuid_path(
    ctx: &RequestContext,
    field: &str,
    value: &str,
) -> Result<uuid::Uuid, ApiError> {
    uuid::Uuid::parse_str(value).map_err(|_| ApiError::validation(ctx, field, "must be a UUID"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn etag_rendering_is_quoted_strong() {
        assert_eq!(etag(7), "\"7\"");
    }

    #[test]
    fn if_match_accepts_strong_weak_and_bare() {
        let parse = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(
                axum::http::header::IF_MATCH,
                HeaderValue::from_str(value).unwrap(),
            );
            parse_if_match(&headers)
        };
        assert_eq!(parse("\"7\""), Some(7));
        assert_eq!(parse("W/\"3\""), Some(3));
        assert_eq!(parse("12"), Some(12));
        assert_eq!(parse("\"0\""), None);
        assert_eq!(parse("\"abc\""), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn rfc3339_parsing() {
        let ctx = RequestContext {
            request_id: "req_test".to_string(),
            instance: "/v1".to_string(),
        };
        assert_eq!(
            parse_rfc3339_ms(&ctx, "occurred_at", "1970-01-01T00:00:00Z").unwrap(),
            0
        );
        assert_eq!(
            parse_rfc3339_ms(&ctx, "occurred_at", "2026-07-23T10:30:00.042Z").unwrap(),
            1_784_802_600_042
        );
        assert!(parse_rfc3339_ms(&ctx, "occurred_at", "not-a-time").is_err());
    }
}
