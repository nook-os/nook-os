//! Authentication: generic OIDC (any standards-compliant IdP) + opaque
//! server-side sessions. Identity always belongs to the customer's IdP.

pub mod password;

pub mod perm;
pub mod session_guard;

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

/// What is calling: a person at a browser, or a machine presenting its node
/// token.
///
/// The distinction matters because a node token lives in a file on a machine
/// whose whole job is running other people's code. It is a service credential,
/// not a stand-in for the owner, and the things only a human should do —
/// setting the vault password, enrolling machines — are refused to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Principal {
    /// A signed-in person (session cookie).
    User,
    /// A joined machine. Authenticated by its client CERTIFICATE where the
    /// connection has one, and by its node token otherwise — the token path is
    /// transitional and disappears once every machine has enrolled.
    ///
    /// Either way the confinement is the same, which is the point: rebasing
    /// identity onto certificates must not change *what a machine may do*,
    /// only how it proves who it is.
    Node(nook_types::NodeId),
}

/// The authenticated caller. Every tenant-scoped query takes its tenant_id
/// from here and nowhere else.
#[derive(Debug, Clone, Copy)]
pub struct AuthCtx {
    pub session_id: AuthSessionId,
    pub user_id: UserId,
    pub tenant_id: TenantId,
    pub principal: Principal,
    /// True only when this caller is backed by a real `sessions_auth` row (a
    /// browser cookie session) — the kind a tenant switch can move. A user
    /// token is also `Principal::User` but sets this `false`, so `switch_tenant`
    /// can tell "structurally cannot switch" (token → 400) from "the session
    /// row vanished mid-request" (cookie → 401) without inferring it from
    /// `rows_affected`.
    pub cookie_session: bool,
}

impl AuthCtx {
    /// Build a caller from a verified client certificate.
    ///
    /// `verify_node_cert` has already checked the chain, validity, revocation
    /// and — critically — that the certificate's tenant matches the node
    /// record it names. This just carries that decision into the type the rest
    /// of the API authorises against, so `require_node_self` keeps working
    /// unchanged: the certificate proves *who*, tenant scoping decides *what*.
    pub async fn from_node_cert(state: &AppState, cert_der: &[u8]) -> Result<Self, ApiError> {
        let id = crate::ca::verify_node_cert(&state.db, cert_der)
            .await
            .map_err(|e| ApiError::ForbiddenMsg(e.to_string()))?;

        // A node acts as the tenant's owner for attribution, exactly as the
        // node-token path does — it is a machine credential, not a person, and
        // `require_user` is what keeps it from becoming one.
        let owner: Option<(Uuid,)> = sqlx::query_as(
            "SELECT id FROM users WHERE tenant_id = $1
             ORDER BY (role = 'owner') DESC, created_at LIMIT 1",
        )
        .bind(id.tenant_id)
        .fetch_optional(&state.db)
        .await?;

        Ok(AuthCtx {
            session_id: AuthSessionId(id.node_id),
            user_id: UserId(owner.map(|(u,)| u).unwrap_or(id.node_id)),
            tenant_id: TenantId(id.tenant_id),
            principal: Principal::Node(nook_types::NodeId(id.node_id)),
            cookie_session: false,
        })
    }

    /// Confine a machine credential to its own machine.
    ///
    /// A node token authenticates the machine it sits on. Letting it act on a
    /// *different* node turns one compromised box into every box: starting a
    /// session is running a command, and the fleet is exactly the set of
    /// machines you did not want that to reach. Humans are unrestricted —
    /// driving other nodes is the entire point of the control plane.
    pub fn require_node_self(&self, node_id: nook_types::NodeId) -> Result<(), ApiError> {
        match self.principal {
            Principal::User => Ok(()),
            Principal::Node(self_id) if self_id == node_id => Ok(()),
            Principal::Node(_) => Err(ApiError::ForbiddenMsg(
                "a node token can only act on its own machine — sign in as a user \
                 to drive another node"
                    .into(),
            )),
        }
    }

    /// Require a tenant owner or admin.
    ///
    /// CA lifecycle — generating, staging, promoting, retiring, revoking a node
    /// — is authority over every machine in a tenant, so it is gated on role
    /// rather than on merely being signed in. The role columns have existed
    /// since 0001 with a CHECK constraint and have never been enforced; this is
    /// where they start meaning something.
    ///
    /// Scoped to the caller's OWN tenant by construction: `tenant_id` comes
    /// from the authenticated context, never from the request, so a tenant
    /// admin has no way to name someone else's tenant.
    pub async fn require_tenant_admin(&self, state: &AppState) -> Result<(), ApiError> {
        self.require_user()?;
        let role: Option<(String,)> =
            sqlx::query_as("SELECT role FROM users WHERE id = $1 AND tenant_id = $2")
                .bind(self.user_id)
                .bind(self.tenant_id)
                .fetch_optional(&state.db)
                .await?;
        match role.as_ref().map(|(r,)| r.as_str()) {
            Some("owner") | Some("admin") => Ok(()),
            _ => Err(ApiError::ForbiddenMsg(
                "this needs tenant owner or admin".into(),
            )),
        }
    }

    /// Refuse machine credentials.
    ///
    /// For operations that grant lasting power rather than doing today's work:
    /// taking over the secret vault, enrolling more machines, removing a node.
    /// A stolen node token should let an attacker do what that node already
    /// does — not become the account.
    pub fn require_user(&self) -> Result<(), ApiError> {
        match self.principal {
            Principal::User => Ok(()),
            Principal::Node(_) => Err(ApiError::ForbiddenMsg(
                "a node token cannot do this — sign in as a user".into(),
            )),
        }
    }
}

impl FromRequestParts<AppState> for AuthCtx {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // A client certificate outranks everything else: it is the strongest
        // credential on offer and the only one that cannot be replayed from a
        // stolen file alone. Checked first so a machine that has enrolled is
        // identified by what it proved in the handshake, not by a token it
        // also happens to still carry.
        if let Some(cert) = parts.extensions.get::<crate::agent_tls::PeerCertificate>() {
            return AuthCtx::from_node_cert(state, &cert.0).await;
        }

        // Browsers authenticate with the session cookie; everything else
        // presents a bearer token. The prefix says which kind, because the two
        // are not interchangeable: a user token IS the person (it can drive any
        // machine they own), a node token is only the machine it sits on.
        if let Some(bearer) = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
        {
            return if bearer.starts_with(USER_TOKEN_PREFIX) {
                user_token_ctx(state, bearer).await
            } else {
                node_token_ctx(state, bearer).await
            };
        }

        // WebSockets cannot carry an Authorization header — the browser API has
        // no way to set one — and a cross-origin socket sends no cookie either.
        // The subprotocol field is the one header a client controls, so a token
        // rides there. Chosen over `?access_token=` deliberately: query strings
        // end up in access logs and proxy traces, and this is a credential that
        // can drive every machine its owner has.
        if let Some(token) = parts
            .headers
            .get("sec-websocket-protocol")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| {
                v.split(',')
                    .map(str::trim)
                    .skip_while(|p| *p != WS_BEARER_PROTOCOL)
                    .nth(1)
            })
        {
            return if token.starts_with(USER_TOKEN_PREFIX) {
                user_token_ctx(state, token).await
            } else {
                node_token_ctx(state, token).await
            };
        }

        let jar = CookieJar::from_request_parts(parts, state)
            .await
            .map_err(|_| ApiError::Unauthorized)?;
        let sid: Uuid = jar
            .get(SESSION_COOKIE)
            .and_then(|c| c.value().parse().ok())
            .ok_or(ApiError::Unauthorized)?;

        // One round-trip resolves the session AND its active-membership grant.
        // The membership `EXISTS` is a SELECT column, not a WHERE clause, so the
        // two failure modes stay distinct (folding it into WHERE would collapse
        // them into a single 401):
        //   - no row  → no valid `sessions_auth`           → 401 Unauthorized
        //   - row, is_member = false → grant revoked       → 403 Forbidden
        //
        // AC-7: `tenant_members` is the single source of truth for the LIFE of a
        // session, not just at switch time — a browser session scoped to a
        // tenant whose grant has since been revoked loses access on its very
        // next request. Cookie sessions only: node/user tokens are different
        // principals (a node token borrows the owner's id and legitimately has
        // no membership row), so this check lives on the sessions_auth path.
        let row: Option<(Uuid, Uuid, bool)> = sqlx::query_as(
            "SELECT sa.user_id, sa.tenant_id,
                    EXISTS(SELECT 1 FROM tenant_members m
                           WHERE m.tenant_id = sa.tenant_id
                             AND m.principal_type = 'user'
                             AND m.principal_id = sa.user_id) AS is_member
             FROM sessions_auth sa
             WHERE sa.id = $1 AND sa.expires_at > now()",
        )
        .bind(sid)
        .fetch_optional(&state.db)
        .await?;

        let (user_id, tenant_id, is_member) = row.ok_or(ApiError::Unauthorized)?;
        if !is_member {
            return Err(ApiError::Forbidden);
        }

        Ok(AuthCtx {
            session_id: AuthSessionId(sid),
            user_id: UserId(user_id),
            tenant_id: TenantId(tenant_id),
            principal: Principal::User,
            cookie_session: true,
        })
    }
}

/// User tokens are self-describing, so the server never has to guess which
/// table to look in — and a leaked one is recognizable in a log or a paste.
pub const USER_TOKEN_PREFIX: &str = "nook_user_";

/// Names the WebSocket subprotocol that carries a bearer token. The client
/// sends `[WS_BEARER_PROTOCOL, <token>]`; the server must echo the first back
/// or the browser aborts the connection.
pub const WS_BEARER_PROTOCOL: &str = "nook.bearer";

/// Resolve a user token to the person who owns it.
///
/// The result is `Principal::User`: this credential stands in for a human, so
/// it may drive any machine in their tenant. That is the whole reason it
/// exists — a node token deliberately cannot, and scripts still need to.
/// Revocation is a row delete, and expiry (if set) is enforced here.
async fn user_token_ctx(state: &AppState, token: &str) -> Result<AuthCtx, ApiError> {
    let hash = crate::seed::hash_token(token);
    let row: Option<(Uuid, Uuid, Uuid)> = sqlx::query_as(
        "SELECT id, user_id, tenant_id FROM user_tokens
         WHERE token_hash = $1 AND (expires_at IS NULL OR expires_at > now())",
    )
    .bind(&hash)
    .fetch_optional(&state.db)
    .await?;
    let (token_id, user_id, tenant_id) = row.ok_or(ApiError::Unauthorized)?;

    // Best-effort: a token nobody can date is a token nobody dares revoke.
    let _ = sqlx::query("UPDATE user_tokens SET last_used_at = now() WHERE id = $1")
        .bind(token_id)
        .execute(&state.db)
        .await;

    Ok(AuthCtx {
        // No browser session behind this; reuse the token id so anything
        // keyed by session has something stable and unique to hold.
        session_id: AuthSessionId(token_id),
        user_id: UserId(user_id),
        tenant_id: TenantId(tenant_id),
        principal: Principal::User,
        // A bearer token, not a sessions_auth row: a switch cannot move it.
        cookie_session: false,
    })
}

/// Resolve a node token to its tenant.
///
/// It borrows the owner's user id so tenant-scoped queries and event
/// attribution keep working, but it is marked as a node — see
/// `AuthCtx::require_user`, which is what stops that borrowed identity from
/// becoming a way to take the account over.
async fn node_token_ctx(state: &AppState, token: &str) -> Result<AuthCtx, ApiError> {
    let hash = crate::seed::hash_token(token);
    let node: Option<(Uuid, Uuid)> =
        sqlx::query_as("SELECT id, tenant_id FROM nodes WHERE node_token_hash = $1")
            .bind(&hash)
            .fetch_optional(&state.db)
            .await?;
    let (node_id, tenant_id) = node.ok_or(ApiError::Unauthorized)?;
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
        principal: Principal::Node(nook_types::NodeId(node_id)),
        cookie_session: false,
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
