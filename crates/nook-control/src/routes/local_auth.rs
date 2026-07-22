//! `/api/v1/auth/local/*` — sign in with a username and password.
//!
//! The endpoints are deliberately few. Registration is not open: the first
//! person to reach an unclaimed instance becomes its owner, and everybody after
//! that is created by an admin. A self-hosted control plane that anyone on the
//! network can sign themselves up to is not a feature.

use axum::extract::State;
use axum::response::IntoResponse;
use axum::Json;
use axum_extra::extract::CookieJar;
use nook_types::*;

use crate::auth::{create_auth_session, session_cookie, AuthCtx};
use crate::error::{ApiError, ApiResult};
use crate::events::{self, EventDraft};
use crate::services::identity::slugify;
use crate::services::local_auth::{self, AuthMode};
use crate::state::AppState;

/// Is this instance unclaimed? Used to decide whether `/bootstrap` is open.
async fn user_count(state: &AppState) -> Result<i64, sqlx::Error> {
    let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM users")
        .fetch_one(&state.db)
        .await?;
    Ok(n)
}

/// The tenant a local sign-in belongs to: the seeded default.
///
/// Local accounts are for the single-organisation case — someone running this
/// on their own hardware. Multi-tenant local sign-in needs a tenant selector in
/// the login form, which is a different product decision.
async fn default_tenant(state: &AppState) -> ApiResult<Tenant> {
    let slug = slugify(&state.cfg.default_tenant_name);
    let existing: Option<Tenant> = sqlx::query_as("SELECT * FROM tenants WHERE slug = $1")
        .bind(&slug)
        .fetch_optional(&state.db)
        .await?;
    if let Some(t) = existing {
        return Ok(t);
    }
    Ok(
        sqlx::query_as("INSERT INTO tenants (id, name, slug) VALUES ($1, $2, $3) RETURNING *")
            .bind(TenantId::new())
            .bind(&state.cfg.default_tenant_name)
            .bind(&slug)
            .fetch_one(&state.db)
            .await?,
    )
}

/// POST /api/v1/auth/local/bootstrap — claim an unclaimed instance.
///
/// Open only while there are no users at all. After that it is closed
/// permanently, so the window is "between `docker compose up` and the first
/// sign-in" rather than something an operator has to remember to turn off.
#[utoipa::path(post, path = "/api/v1/auth/local/bootstrap",
    request_body = LocalRegisterRequest,
    responses((status = 200, body = MeResponse), (status = 403, description = "already claimed")))]
pub async fn bootstrap(
    State(state): State<AppState>,
    jar: CookieJar,
    Json(req): Json<LocalRegisterRequest>,
) -> ApiResult<impl IntoResponse> {
    if user_count(&state).await? > 0 {
        return Err(ApiError::ForbiddenMsg(
            "this instance already has an account — ask an administrator to create yours".into(),
        ));
    }

    let tenant = default_tenant(&state).await?;
    // Claim the mode BEFORE creating anyone: if the tenant is already on OIDC,
    // this must fail without leaving a half-made local account behind.
    local_auth::claim_mode(&state.db, tenant.id, AuthMode::Local).await?;

    let email = req
        .email
        .unwrap_or_else(|| format!("{}@localhost", req.username));
    let display = req.display_name.unwrap_or_else(|| req.username.clone());
    let user = local_auth::create(
        &state.db,
        tenant.id,
        &req.username,
        &email,
        &display,
        &req.password,
        true,
    )
    .await?;

    let session_id = create_auth_session(&state, user.id, tenant.id).await?;
    events::record(
        &state,
        tenant.id,
        EventDraft::new("user.bootstrap")
            .actor("user", user.id.0)
            .payload(serde_json::json!({ "username": req.username, "via": "local" })),
    )
    .await;

    Ok((
        jar.add(session_cookie(&state, session_id)),
        Json(MeResponse { user, tenant }),
    ))
}

/// POST /api/v1/auth/local/login
#[utoipa::path(post, path = "/api/v1/auth/local/login",
    request_body = LocalLoginRequest,
    responses((status = 200, body = MeResponse), (status = 401, description = "bad credentials")))]
pub async fn login(
    State(state): State<AppState>,
    jar: CookieJar,
    Json(req): Json<LocalLoginRequest>,
) -> ApiResult<impl IntoResponse> {
    let tenant = default_tenant(&state).await?;
    let (user, tenant) =
        local_auth::login(&state.db, tenant.id, &req.username, &req.password).await?;

    let session_id = create_auth_session(&state, user.id, tenant.id).await?;
    events::record(
        &state,
        tenant.id,
        EventDraft::new("user.login")
            .actor("user", user.id.0)
            .payload(serde_json::json!({ "username": req.username, "via": "local" })),
    )
    .await;

    Ok((
        jar.add(session_cookie(&state, session_id)),
        Json(MeResponse { user, tenant }),
    ))
}

/// POST /api/v1/auth/local/users — an admin creates an account.
#[utoipa::path(post, path = "/api/v1/auth/local/users",
    request_body = LocalRegisterRequest,
    responses((status = 200, body = User), (status = 403, description = "not an admin")))]
pub async fn create_user(
    State(state): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<LocalRegisterRequest>,
) -> ApiResult<Json<User>> {
    auth.require_tenant_admin(&state).await?;
    local_auth::claim_mode(&state.db, auth.tenant_id, AuthMode::Local).await?;

    let email = req
        .email
        .unwrap_or_else(|| format!("{}@localhost", req.username));
    let display = req.display_name.unwrap_or_else(|| req.username.clone());
    let user = local_auth::create(
        &state.db,
        auth.tenant_id,
        &req.username,
        &email,
        &display,
        &req.password,
        false,
    )
    .await?;

    events::record(
        &state,
        auth.tenant_id,
        EventDraft::new("user.created")
            .actor("user", auth.user_id.0)
            .payload(serde_json::json!({ "username": req.username })),
    )
    .await;
    Ok(Json(user))
}

/// POST /api/v1/auth/local/password — change your own password.
#[utoipa::path(post, path = "/api/v1/auth/local/password",
    request_body = ChangePasswordRequest,
    responses((status = 200, description = "changed"), (status = 401, description = "wrong password")))]
pub async fn change_password(
    State(state): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<ChangePasswordRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    // Changing a password requires proving the current one, which is what
    // keeps a stolen session or user token from becoming permanent account
    // takeover — the thief has the credential but not the secret behind it.
    auth.require_user()?;
    local_auth::change_password(&state.db, auth.user_id, &req.current, &req.next).await?;
    events::record(
        &state,
        auth.tenant_id,
        EventDraft::new("user.password_changed").actor("user", auth.user_id.0),
    )
    .await;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// Extends `/auth/providers` with what local sign-in offers right now.
#[utoipa::path(get, path = "/api/v1/auth/local/status",
    responses((status = 200, body = LocalAuthStatus)))]
pub async fn status(State(state): State<AppState>) -> ApiResult<Json<LocalAuthStatus>> {
    let tenant = default_tenant(&state).await?;
    let mode = local_auth::mode_of(&state.db, tenant.id).await?;
    Ok(Json(LocalAuthStatus {
        // Undecided, or already committed to local.
        available: !matches!(mode, Some(AuthMode::Oidc)),
        // Nobody has claimed this instance yet: show the create-owner form.
        needs_bootstrap: user_count(&state).await? == 0,
        mode: mode.map(|m| m.as_str().to_string()),
    }))
}
