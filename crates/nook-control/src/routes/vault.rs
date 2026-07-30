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
pub async fn status(State(state): State<AppState>, auth: AuthCtx) -> ApiResult<Json<VaultStatus>> {
    // A node cannot enumerate the vault.
    auth.require_user()?;
    let created_at = state.vaults.app_password_set_at(auth.user_id).await?;
    let passkeys = state.vaults.passkey_count(auth.user_id).await?;
    Ok(Json(VaultStatus {
        configured: created_at.is_some(),
        created_at,
        passkeys,
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
    // The one that matters: a node token must never be able to set the
    // owner's app password and become able to seal (and later read) secrets.
    auth.require_user()?;
    if req.passphrase.chars().count() < 8 {
        return Err(ApiError::BadRequest(
            "app password must be at least 8 characters".into(),
        ));
    }
    if state.vaults.has_app_password(auth.user_id).await? {
        return Err(ApiError::Conflict(
            "an app password is already set and cannot be changed".into(),
        ));
    }

    // The passphrase never reaches the repository: only the derived, one-way
    // salt + verifier do.
    let (salt, verifier) = crate::crypto::passphrase_verifier(&req.passphrase);
    state
        .vaults
        .set_app_password(auth.user_id, auth.tenant_id, salt, verifier)
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
        passkeys: 0,
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
    // Refused for nodes: otherwise a machine with a stolen token gets an
    // unlimited offline-speed oracle for guessing the app password.
    auth.require_user()?;
    let challenge = state
        .vaults
        .app_password_challenge(auth.user_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if crate::crypto::verify_passphrase(&req.passphrase, &challenge.kdf_salt, &challenge.verifier) {
        Ok(axum::http::StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::Forbidden)
    }
}

// ── Passkeys ────────────────────────────────────────────────────────────────
//
// A passkey doesn't replace the app password — it carries it. The browser
// derives a key from the passkey (WebAuthn's PRF extension) and encrypts the
// app password with it; the server stores only that blob and hands it back to
// whoever is logged in. Without the passkey the blob is noise, so this adds a
// way to unlock without adding a way to be unlocked: the server still cannot
// read a secret, and typing the password still works if the passkey is lost.

/// Passkeys enrolled on this vault, with their wrapped secrets — the browser
/// needs the blob in hand before it can ask the authenticator to open it.
#[utoipa::path(get, path = "/api/v1/vault/passkeys",
    operation_id = "list_passkeys",
    responses((status = 200, body = [VaultPasskey])))]
pub async fn list_passkeys(
    State(state): State<AppState>,
    auth: AuthCtx,
) -> ApiResult<Json<Vec<VaultPasskey>>> {
    // Passkeys are the vault's other door.
    auth.require_user()?;
    Ok(Json(state.vaults.list_passkeys(auth.user_id).await?))
}

/// Enrol a passkey. Requires the app password, since that's what's being
/// wrapped — you can't hand out a key to a vault you can't open.
#[utoipa::path(post, path = "/api/v1/vault/passkeys",
    operation_id = "add_passkey",
    request_body = AddPasskeyRequest,
    responses((status = 200, body = VaultPasskey), (status = 404)))]
pub async fn add_passkey(
    State(state): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<AddPasskeyRequest>,
) -> ApiResult<Json<VaultPasskey>> {
    // Enrolling a passkey is enrolling a way in.
    auth.require_user()?;
    use base64::Engine;

    if req.credential_id.is_empty() || req.wrapped_secret.is_empty() {
        return Err(ApiError::BadRequest("incomplete passkey".into()));
    }
    // No vault, nothing to unlock.
    if !state.vaults.has_app_password(auth.user_id).await? {
        return Err(ApiError::SetupRequired(
            "set an app password before enrolling a passkey".into(),
        ));
    }
    let wrapped = base64::engine::general_purpose::STANDARD
        .decode(req.wrapped_secret.as_bytes())
        .map_err(|_| ApiError::BadRequest("wrapped secret is not base64".into()))?;

    let label = if req.label.trim().is_empty() {
        "passkey".to_string()
    } else {
        req.label.trim().to_string()
    };
    let (id, created_at) = state
        .vaults
        .upsert_passkey(crate::repo::notebook::NewPasskey {
            user: auth.user_id,
            tenant: auth.tenant_id,
            credential_id: req.credential_id.clone(),
            label: label.clone(),
            wrapped_secret: wrapped,
        })
        .await?;

    crate::events::record(
        &state,
        auth.tenant_id,
        crate::events::EventDraft::new("vault.passkey_added")
            .actor("user", auth.user_id.0)
            .payload(serde_json::json!({ "label": label })),
    )
    .await;

    Ok(Json(VaultPasskey {
        id,
        credential_id: req.credential_id,
        label,
        wrapped_secret: req.wrapped_secret,
        created_at,
        last_used_at: None,
    }))
}

/// Forget a passkey. The vault is untouched — the app password still opens it.
#[utoipa::path(delete, path = "/api/v1/vault/passkeys/{id}",
    operation_id = "delete_passkey",
    params(("id" = String, Path,)),
    responses((status = 204), (status = 404)))]
pub async fn delete_passkey(
    State(state): State<AppState>,
    auth: AuthCtx,
    axum::extract::Path(id): axum::extract::Path<uuid::Uuid>,
) -> ApiResult<axum::http::StatusCode> {
    // Removing someone's passkey is a lockout, not a node's job.
    auth.require_user()?;
    let done = state.vaults.delete_passkey(id, auth.user_id).await?;
    if done == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// Note that a passkey was just used, so the settings page can show it.
#[utoipa::path(post, path = "/api/v1/vault/passkeys/{id}/used",
    operation_id = "touch_passkey",
    params(("id" = String, Path,)),
    responses((status = 204)))]
pub async fn touch_passkey(
    State(state): State<AppState>,
    auth: AuthCtx,
    axum::extract::Path(id): axum::extract::Path<uuid::Uuid>,
) -> ApiResult<axum::http::StatusCode> {
    // Bookkeeping for a human's device.
    auth.require_user()?;
    state.vaults.touch_passkey(id, auth.user_id).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}
