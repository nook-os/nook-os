//! The user's app password: one passphrase, set once, that seals their
//! secrets.
//!
//! The server stores a salt and a verifier — never the password and never the
//! derived key — so it can tell you "that's the wrong password" without ever
//! being able to decrypt on its own. It cannot be changed, because changing it
//! would mean re-sealing every secret, which requires the old password anyway;
//! the UI says so plainly before it's set.

use axum::extract::State;
use axum::Json;
use nook_types::*;

use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Has this user set an app password yet?
#[utoipa::path(get, path = "/api/v1/vault/status",
    operation_id = "vault_status",
    responses((status = 200, body = VaultStatus)))]
pub async fn status(
    State(state): State<AppState>,
    auth: AuthCtx,
) -> ApiResult<Json<VaultStatus>> {
    let row: Option<(chrono::DateTime<chrono::Utc>,)> =
        sqlx::query_as("SELECT created_at FROM user_vaults WHERE user_id = $1")
            .bind(auth.user_id)
            .fetch_optional(&state.db)
            .await?;
    Ok(Json(VaultStatus {
        configured: row.is_some(),
        created_at: row.map(|(t,)| t),
    }))
}

/// Set the app password. Once only — a second attempt is a conflict, not an
/// overwrite, so a stray call can never orphan existing secrets.
#[utoipa::path(post, path = "/api/v1/vault/passphrase",
    operation_id = "set_vault_passphrase",
    request_body = SetVaultPassphraseRequest,
    responses((status = 200, body = VaultStatus), (status = 409)))]
pub async fn set_passphrase(
    State(state): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<SetVaultPassphraseRequest>,
) -> ApiResult<Json<VaultStatus>> {
    if req.passphrase.chars().count() < 8 {
        return Err(ApiError::BadRequest(
            "app password must be at least 8 characters".into(),
        ));
    }
    let existing: Option<(uuid::Uuid,)> =
        sqlx::query_as("SELECT user_id FROM user_vaults WHERE user_id = $1")
            .bind(auth.user_id)
            .fetch_optional(&state.db)
            .await?;
    if existing.is_some() {
        return Err(ApiError::Conflict(
            "an app password is already set and cannot be changed".into(),
        ));
    }

    let (salt, verifier) = crate::crypto::passphrase_verifier(&req.passphrase);
    sqlx::query(
        "INSERT INTO user_vaults (user_id, tenant_id, kdf_salt, verifier)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(auth.user_id)
    .bind(auth.tenant_id)
    .bind(&salt)
    .bind(&verifier)
    .execute(&state.db)
    .await?;

    crate::events::record(
        &state,
        auth.tenant_id,
        crate::events::EventDraft::new("vault.passphrase_set").actor("user", auth.user_id.0),
    )
    .await;

    Ok(Json(VaultStatus {
        configured: true,
        created_at: Some(chrono::Utc::now()),
    }))
}

/// Check a password without decrypting anything — lets the UI unlock (and
/// hold the password for syncing) with a clear yes/no.
#[utoipa::path(post, path = "/api/v1/vault/verify",
    operation_id = "verify_vault_passphrase",
    request_body = SetVaultPassphraseRequest,
    responses((status = 204), (status = 403), (status = 404)))]
pub async fn verify(
    State(state): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<SetVaultPassphraseRequest>,
) -> ApiResult<axum::http::StatusCode> {
    let row: Option<(Vec<u8>, Vec<u8>)> =
        sqlx::query_as("SELECT kdf_salt, verifier FROM user_vaults WHERE user_id = $1")
            .bind(auth.user_id)
            .fetch_optional(&state.db)
            .await?;
    let (salt, verifier) = row.ok_or(ApiError::NotFound)?;
    if crate::crypto::verify_passphrase(&req.passphrase, &salt, &verifier) {
        Ok(axum::http::StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::Forbidden)
    }
}
