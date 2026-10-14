//! Per-request context: the `X-Request-ID` correlation id and the request
//! path used as the RFC 7807 `instance`.

use axum::{
    body::Body,
    extract::Request,
    http::{HeaderValue, Response},
    middleware::Next,
};
use uuid::Uuid;

/// Correlation context attached to every request by [`middleware`].
/// Handlers and error constructors read it from the request extensions; the
/// middleware stamps `X-Request-ID` on every response, including errors.
#[derive(Debug, Clone)]
pub struct RequestContext {
    /// `req_<uuidv7 simple>` correlation id.
    pub request_id: String,
    /// Request path (the RFC 7807 `instance` field).
    pub instance: String,
}

impl RequestContext {
    /// Read the context installed by [`middleware`], or mint a fresh one when
    /// the middleware did not run (unmatched fallback defense-in-depth).
    pub fn from_extensions(extensions: &axum::http::Extensions) -> Self {
        extensions.get::<Self>().cloned().unwrap_or_else(|| Self {
            request_id: new_request_id(),
            instance: String::new(),
        })
    }
}

/// `req_<uuidv7 simple>` — time-ordered correlation ids.
fn new_request_id() -> String {
    format!("req_{}", Uuid::now_v7().simple())
}

/// Outermost layer: install the [`RequestContext`] extension and stamp
/// `X-Request-ID` on the response, whatever the handler produced.
pub async fn middleware(mut request: Request, next: Next) -> Response<Body> {
    let request_id = new_request_id();
    let instance = request.uri().path().to_string();
    request.extensions_mut().insert(RequestContext {
        request_id: request_id.clone(),
        instance,
    });
    let mut response = next.run(request).await;
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response.headers_mut().insert(
            axum::http::header::HeaderName::from_static("x-request-id"),
            value,
        );
    }
    response
}
