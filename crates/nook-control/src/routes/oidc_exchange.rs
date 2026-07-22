//! `POST /api/v1/auth/oidc/exchange` — trade an identity provider's ID token
//! for a NookOS user token.
//!
//! This is the half of native sign-in that belongs here, and only this half.
//! The desktop app and the CLI run the device authorization grant against the
//! **identity provider** — it owns identity, it has the approval screen, and it
//! already implements RFC 8628. NookOS never sees a password and never runs the
//! ceremony.
//!
//! What it does is decide whether an assertion is one it trusts: signed by the
//! configured issuer's keys, issued to the client we advertise, unexpired. Then
//! it maps that to a user here and mints a credential of its own — revocable
//! from the tokens list, and identical to one minted in the browser, so nothing
//! downstream has to care which door somebody came through.

use axum::extract::State;
use axum::Json;
use nook_types::*;
use openidconnect::core::{CoreClient, CoreIdToken};
use openidconnect::{ClientId, IssuerUrl, Nonce};

use crate::error::{ApiError, ApiResult};
use crate::events::{self, EventDraft};
use crate::seed::hash_token;
use crate::services::identity::{login_identity, IdentityClaims};
use crate::state::AppState;

pub async fn exchange(
    State(state): State<AppState>,
    Json(req): Json<OidcExchangeRequest>,
) -> ApiResult<Json<OidcExchangeResponse>> {
    let oidc = state
        .oidc
        .as_ref()
        .ok_or_else(|| ApiError::BadRequest("this instance has no identity provider".into()))?;

    // Verify against the client the token was ISSUED to, which for a native
    // sign-in is the public device client — not the control plane's own
    // confidential client. Using the wrong one fails on `aud` and the error
    // says nothing useful about why.
    let audience = state
        .cfg
        .oidc_device_client_id
        .clone()
        .or_else(|| state.cfg.oidc_client_id.clone())
        .ok_or_else(|| ApiError::BadRequest("OIDC is not configured".into()))?;

    let client = CoreClient::from_provider_metadata(
        oidc.metadata.clone(),
        ClientId::new(audience),
        // No secret: a public client has none, and verification needs none.
        None,
    );

    let token: CoreIdToken = req
        .id_token
        .parse()
        .map_err(|e| ApiError::BadRequest(format!("that is not an ID token: {e}")))?;

    // The device grant has no nonce — there is no browser redirect to bind one
    // to, and RFC 8628 does not define one. Accepting its absence here is not a
    // weakening: the token is bound to the client by `aud` and to the issuer by
    // its signature, and the device code it came from was never in a URL.
    let claims = token
        .claims(&client.id_token_verifier(), |_: Option<&Nonce>| Ok(()))
        .map_err(|e| ApiError::BadRequest(format!("ID token rejected: {e}")))?;

    // Belt and braces: the verifier already checks this, but an issuer mismatch
    // is the one failure worth naming explicitly rather than leaving inside a
    // library's error string.
    let expected_issuer = state
        .cfg
        .oidc_issuer_url
        .clone()
        .ok_or_else(|| ApiError::BadRequest("OIDC is not configured".into()))?;
    let expected = IssuerUrl::new(expected_issuer)
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("bad OIDC_ISSUER_URL: {e}")))?;
    if claims.issuer() != &expected {
        return Err(ApiError::ForbiddenMsg(format!(
            "that token was issued by {}, not {}",
            claims.issuer().as_str(),
            expected.as_str()
        )));
    }

    let identity = IdentityClaims {
        issuer: claims.issuer().to_string(),
        subject: claims.subject().to_string(),
        email: claims.email().map(|e| e.to_string()),
        display_name: claims
            .name()
            .and_then(|n| n.get(None))
            .map(|n| n.to_string()),
        avatar_url: claims
            .picture()
            .and_then(|p| p.get(None))
            .map(|p| p.to_string()),
        raw_claims: serde_json::to_value(claims).unwrap_or_default(),
    };

    // Same path as the browser callback, so a person who signs in from the app
    // is the same user with the same tenant as when they sign in from a tab.
    let (user, tenant) = login_identity(&state, identity).await?;

    let token_value = crate::routes::join::random_token(crate::auth::USER_TOKEN_PREFIX, 32);
    sqlx::query(
        "INSERT INTO user_tokens (id, user_id, tenant_id, token_hash, name, expires_at)
         VALUES ($1, $2, $3, $4, $5, now() + interval '365 days')",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(user.id)
    .bind(tenant.id)
    .bind(hash_token(&token_value))
    .bind(req.client_name.unwrap_or_else(|| "native client".into()))
    .execute(&state.db)
    .await?;

    events::record(
        &state,
        tenant.id,
        EventDraft::new("user.login")
            .actor("user", user.id.0)
            .payload(serde_json::json!({ "via": "device" })),
    )
    .await;

    Ok(Json(OidcExchangeResponse {
        token: token_value,
        user,
        tenant,
    }))
}
