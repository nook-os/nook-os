//! Personal access tokens: how a script, a CLI or an agent acts as *you*.
//!
//! A node token authenticates a machine and the control plane confines it to
//! that machine — which is right, and which is exactly why it can't be the
//! credential for `nook start --node other-box`. This is the other half: a
//! credential that stands in for a person, so tooling can drive the whole
//! fleet the way that person could from a browser.
//!
//! The plaintext is shown once, at creation, and never stored — only its
//! SHA-256. Losing one means minting another, not reading it back.

use axum::extract::{Path, State};
use axum::Json;
use chrono::{Duration, Utc};
use nook_types::*;
use rand::distr::Alphanumeric;
use rand::Rng;

use crate::auth::{AuthCtx, USER_TOKEN_PREFIX};
use crate::error::{ApiError, ApiResult};
use crate::events::{self, EventDraft};
use crate::seed::hash_token;
use crate::state::AppState;

/// Mint a token for the signed-in user.
///
/// Requires a *user* — a node token minting user tokens would be a machine
/// promoting itself to its owner, which is the one thing node confinement
/// exists to prevent.
#[utoipa::path(post, path = "/api/v1/tokens",
    operation_id = "create_user_token",
    request_body = CreateUserTokenRequest,
    responses((status = 200, body = CreateUserTokenResponse), (status = 403)))]
pub async fn create(
    State(state): State<AppState>,
    auth: AuthCtx,
    body: Option<Json<CreateUserTokenRequest>>,
) -> ApiResult<Json<CreateUserTokenResponse>> {
    auth.require_user()?;
    let req = body.map(|Json(r)| r).unwrap_or_default();

    let name = req.name.unwrap_or_default();
    let name = name.trim();
    if name.chars().count() > 80 {
        return Err(ApiError::BadRequest(
            "token name must be 80 characters or fewer".into(),
        ));
    }
    let expires_at = req.expires_in_days.map(|d| Utc::now() + Duration::days(d));

    let body_chars: String = rand::rng()
        .sample_iter(&Alphanumeric)
        .take(40)
        .map(char::from)
        .collect();
    let token = format!("{USER_TOKEN_PREFIX}{body_chars}");

    let id = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO user_tokens (id, tenant_id, user_id, token_hash, name, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(auth.tenant_id)
    .bind(auth.user_id)
    .bind(hash_token(&token))
    .bind(name)
    .bind(expires_at)
    .execute(&state.db)
    .await?;

    events::record(
        &state,
        auth.tenant_id,
        EventDraft::new("user.token_created")
            .actor("user", auth.user_id.0)
            .payload(serde_json::json!({ "name": name })),
    )
    .await;

    Ok(Json(CreateUserTokenResponse {
        // The only time this value exists anywhere but the caller's hands.
        token,
        id: id.to_string(),
        name: name.to_string(),
        expires_at,
    }))
}

/// List this user's tokens. Never the tokens themselves — the point of the
/// list is deciding which one to revoke.
#[utoipa::path(get, path = "/api/v1/tokens",
    operation_id = "list_user_tokens",
    responses((status = 200, body = [UserToken])))]
pub async fn list(State(state): State<AppState>, auth: AuthCtx) -> ApiResult<Json<Vec<UserToken>>> {
    auth.require_user()?;
    let rows: Vec<UserToken> = sqlx::query_as(
        "SELECT id::text, name, last_used_at, expires_at, created_at
         FROM user_tokens WHERE user_id = $1 ORDER BY created_at DESC",
    )
    .bind(auth.user_id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

/// Revoke one. Immediate: the next request carrying it is unauthorized.
#[utoipa::path(delete, path = "/api/v1/tokens/{id}",
    operation_id = "revoke_user_token",
    params(("id" = String, Path,)),
    responses((status = 204), (status = 404)))]
pub async fn revoke(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<String>,
) -> ApiResult<axum::http::StatusCode> {
    auth.require_user()?;
    let uuid: uuid::Uuid = id
        .parse()
        .map_err(|_| ApiError::BadRequest("not a token id".into()))?;

    // Scoped to the caller: one user revoking another's credential is an
    // administrative act, not a self-service one.
    let done = sqlx::query("DELETE FROM user_tokens WHERE id = $1 AND user_id = $2")
        .bind(uuid)
        .bind(auth.user_id)
        .execute(&state.db)
        .await?;
    if done.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }

    events::record(
        &state,
        auth.tenant_id,
        EventDraft::new("user.token_revoked").actor("user", auth.user_id.0),
    )
    .await;
    Ok(axum::http::StatusCode::NO_CONTENT)
}
