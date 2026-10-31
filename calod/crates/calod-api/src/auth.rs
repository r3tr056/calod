//! `X-Calod-Key` authentication (OpenAPI `ApiKeyAuth`).
//!
//! The secret (`cld_<64 hex>`) is verified against the server's
//! [`ApiKeyStore`] — hash, expiry, and revocation are enforced there. The
//! verified key pins every ledger call to its tenant/project and grants
//! capabilities from its scope set (`effects:reconcile` → `can_reconcile`).

use axum::{extract::FromRequestParts, http::request::Parts};

use calod_ledger::{CallerCapabilities, Scope};

use crate::context::RequestContext;
use crate::error::ApiError;
use crate::routes::AppState;

/// The authenticated caller: tenant/project scope plus capabilities, derived
/// from the verified API key.
#[derive(Debug, Clone)]
pub struct AuthContext {
    /// Tenant/project the key is scoped to.
    pub scope: Scope,
    /// Capabilities granted by the key's scopes.
    pub capabilities: CallerCapabilities,
}

/// The scope string that authorizes reconcile transitions.
pub const RECONCILE_SCOPE: &str = "effects:reconcile";

/// The scope string that authorizes admin operations (retention).
pub const ADMIN_SCOPE: &str = "admin";

impl FromRequestParts<AppState> for AuthContext {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let ctx = RequestContext::from_extensions(&parts.extensions);
        let secret = parts
            .headers
            .get("x-calod-key")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
            .ok_or_else(|| ApiError::unauthorized(&ctx))?;

        // argon2 verification is CPU-bound; keep it off the async workers.
        let keys = state.api_keys().clone();
        let verified = tokio::task::spawn_blocking(move || keys.verify(&secret))
            .await
            .map_err(|_| {
                ApiError::service_unavailable(&ctx, "API-key verification is unavailable")
            })?;
        let metadata = verified.ok_or_else(|| ApiError::unauthorized(&ctx))?;

        Ok(Self {
            scope: Scope {
                tenant_id: metadata.tenant_id,
                project_id: metadata.project_id,
            },
            capabilities: CallerCapabilities {
                can_reconcile: metadata.scopes.contains(RECONCILE_SCOPE),
                can_admin: metadata.scopes.contains(ADMIN_SCOPE),
            },
        })
    }
}
