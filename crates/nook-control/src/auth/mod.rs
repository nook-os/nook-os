//! Authentication: generic OIDC (any standards-compliant IdP) + opaque
//! server-side sessions. Identity always belongs to the customer's IdP.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum_extra::extract::cookie::{Cookie, CookieJar, Key, SameSite};
use openidconnect::core::CoreProviderMetadata;
use openidconnect::IssuerUrl;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use nook_types::{AuthSessionId, TenantId, UserId};

use crate::error::ApiError;
use crate::state::AppState;

pub const SESSION_COOKIE: &str = "nook_session";
pub const FLOW_COOKIE: &str = "nook_oidc_flow";

/// Discovered IdP metadata, cached at startup. The OIDC client itself is
/// rebuilt per request from this (pure construction, no network).
pub struct OidcContext {
    pub metadata: CoreProviderMetadata,
    pub http: openidconnect::reqwest::Client,
}

impl OidcContext {
    pub async fn discover(issuer_url: &str) -> anyhow::Result<Self> {
        let http = openidconnect::reqwest::ClientBuilder::new()
            // Never follow redirects during token exchange (OIDC spec hygiene).
            .redirect(openidconnect::reqwest::redirect::Policy::none())
            .build()?;
        let metadata =
            CoreProviderMetadata::discover_async(IssuerUrl::new(issuer_url.to_string())?, &http)
                .await?;
        Ok(Self { metadata, http })
    }
}

/// In-flight OIDC login state, carried in an encrypted short-lived cookie.
#[derive(Serialize, Deserialize)]
pub struct FlowState {
    pub csrf: String,
    pub nonce: String,
    pub pkce_verifier: String,
    pub next: String,
}

/// The authenticated caller. Every tenant-scoped query takes its tenant_id
/// from here and nowhere else.
#[derive(Debug, Clone, Copy)]
pub struct AuthCtx {
    pub session_id: AuthSessionId,
    pub user_id: UserId,
    pub tenant_id: TenantId,
}

impl FromRequestParts<AppState> for AuthCtx {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // Browsers authenticate with the session cookie; the `nook` CLI (and
        // other local tooling) present a node token instead, which is already
        // provisioned on every machine that joined.
        if let Some(bearer) = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
        {
            return node_token_ctx(state, bearer).await;
        }

        let jar = CookieJar::from_request_parts(parts, state)
            .await
            .map_err(|_| ApiError::Unauthorized)?;
        let sid: Uuid = jar
            .get(SESSION_COOKIE)
            .and_then(|c| c.value().parse().ok())
            .ok_or(ApiError::Unauthorized)?;

        let row: Option<(Uuid, Uuid)> = sqlx::query_as(
            "SELECT user_id, tenant_id FROM sessions_auth WHERE id = $1 AND expires_at > now()",
        )
        .bind(sid)
        .fetch_optional(&state.db)
        .await?;

        let (user_id, tenant_id) = row.ok_or(ApiError::Unauthorized)?;
        Ok(AuthCtx {
            session_id: AuthSessionId(sid),
            user_id: UserId(user_id),
            tenant_id: TenantId(tenant_id),
        })
    }
}

/// Resolve a node token to its tenant. Acts as the tenant's owner so events
/// stay attributable and every tenant-scoped query keeps working unchanged.
async fn node_token_ctx(state: &AppState, token: &str) -> Result<AuthCtx, ApiError> {
    let hash = crate::seed::hash_token(token);
    let tenant: Option<(Uuid,)> =
        sqlx::query_as("SELECT tenant_id FROM nodes WHERE node_token_hash = $1")
            .bind(&hash)
            .fetch_optional(&state.db)
            .await?;
    let (tenant_id,) = tenant.ok_or(ApiError::Unauthorized)?;
    let owner: Option<(Uuid,)> = sqlx::query_as(
        "SELECT id FROM users WHERE tenant_id = $1
         ORDER BY (role = 'owner') DESC, created_at LIMIT 1",
    )
    .bind(tenant_id)
    .fetch_optional(&state.db)
    .await?;
    Ok(AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: UserId(owner.map(|(id,)| id).unwrap_or_else(Uuid::nil)),
        tenant_id: TenantId(tenant_id),
    })
}

pub fn session_cookie(state: &AppState, session_id: AuthSessionId) -> Cookie<'static> {
    Cookie::build((SESSION_COOKIE, session_id.to_string()))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .secure(state.cfg.public_base_url.starts_with("https"))
        .max_age(cookie::time::Duration::hours(state.cfg.session_ttl_hours))
        .build()
}

pub fn removal_cookie(name: &'static str) -> Cookie<'static> {
    Cookie::build((name, ""))
        .path("/")
        .max_age(cookie::time::Duration::ZERO)
        .build()
}

/// Create a server-side auth session and return its id (the cookie value).
pub async fn create_auth_session(
    state: &AppState,
    user_id: UserId,
    tenant_id: TenantId,
) -> Result<AuthSessionId, ApiError> {
    let id = AuthSessionId::new();
    sqlx::query(
        "INSERT INTO sessions_auth (id, user_id, tenant_id, expires_at)
         VALUES ($1, $2, $3, now() + make_interval(hours => $4))",
    )
    .bind(id)
    .bind(user_id)
    .bind(tenant_id)
    .bind(state.cfg.session_ttl_hours as i32)
    .execute(&state.db)
    .await?;
    Ok(id)
}

/// Key used by `PrivateCookieJar` (encrypted flow cookie).
pub fn cookie_key(secret: &str) -> Key {
    Key::derive_from(secret.as_bytes())
}
