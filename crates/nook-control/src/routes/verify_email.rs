//! Local-account email verification (MAIN-30).
//!
//! OIDC users are verified by their IdP; a local account has no way to prove its
//! address until now. The round-trip: a signed-in local user requests
//! verification → we issue a single-use expiring token, store only its hash, and
//! email the plaintext in a link → the link's `confirm` consumes the token and
//! records the verification through the verified-email model.
//!
//! `confirm` is token-authenticated, not session-authenticated: the link must
//! work when opened in any browser, and possession of the token (issued to that
//! user's address) is the proof.

use axum::extract::{Json, State};

use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::routes::join::random_token;
use crate::seed::hash_token;
use crate::services::identity::{email_is_verified, mark_local_email_verified};
use crate::state::AppState;
use nook_types::{
    ConfirmVerificationRequest, ConfirmVerificationResult, EmailVerificationStatus,
    RequestVerificationResult, UserId,
};

/// Load the signed-in user's email and whether they are a local account (a
/// password hash → local; OIDC users have none).
async fn user_email_and_local(state: &AppState, user_id: UserId) -> ApiResult<(String, bool)> {
    let (email, password_hash): (String, Option<String>) =
        sqlx::query_as("SELECT email, password_hash FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_one(&state.db)
            .await?;
    Ok((email, password_hash.is_some()))
}

/// `GET /api/v1/auth/verify-email/status` — is this user's email verified, and
/// can they request a local verification email? (AC-4)
#[utoipa::path(get, path = "/api/v1/auth/verify-email/status",
    operation_id = "email_verification_status",
    responses((status = 200, body = EmailVerificationStatus)))]
pub async fn status(
    State(state): State<AppState>,
    auth: AuthCtx,
) -> ApiResult<Json<EmailVerificationStatus>> {
    auth.require_user()?;
    let (email, is_local) = user_email_and_local(&state, auth.user_id).await?;
    let verified = email_is_verified(&state.db, auth.user_id).await?;
    Ok(Json(EmailVerificationStatus {
        email,
        verified,
        // Only an unverified local account can start the round-trip (NG-1).
        can_request: is_local && !verified,
    }))
}

/// `POST /api/v1/auth/verify-email/request` — issue a token and email the link.
/// Best-effort: a mail-transport failure is reported, never a 500 (AC-5).
#[utoipa::path(post, path = "/api/v1/auth/verify-email/request",
    operation_id = "request_email_verification",
    responses((status = 200, body = RequestVerificationResult), (status = 400)))]
pub async fn request(
    State(state): State<AppState>,
    auth: AuthCtx,
) -> ApiResult<Json<RequestVerificationResult>> {
    auth.require_user()?;
    Ok(Json(request_core(&state, auth.user_id).await?))
}

/// The request logic, split from the handler so it can be tested with a real
/// `AppState` (and its capture mailer) but no `AuthCtx`.
pub async fn request_core(
    state: &AppState,
    user_id: UserId,
) -> ApiResult<RequestVerificationResult> {
    let (email, is_local) = user_email_and_local(state, user_id).await?;
    if !is_local {
        return Err(ApiError::BadRequest(
            "email verification is for local accounts; your email is verified by your identity provider".into(),
        ));
    }
    if email_is_verified(&state.db, user_id).await? {
        return Ok(RequestVerificationResult {
            sent: false,
            message: "your email is already verified".into(),
        });
    }

    // One live token per user: drop any outstanding one first (AC-3).
    sqlx::query("DELETE FROM email_verification_tokens WHERE user_id = $1 AND consumed_at IS NULL")
        .bind(user_id)
        .execute(&state.db)
        .await?;

    let token = random_token("evr_", 32);
    sqlx::query(
        "INSERT INTO email_verification_tokens (id, user_id, email, token_hash, expires_at)
         VALUES ($1, $2, $3, $4, now() + interval '24 hours')",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(user_id)
    .bind(&email)
    .bind(hash_token(&token))
    .execute(&state.db)
    .await?;

    let link = format!(
        "{}/verify-email?token={token}",
        state.cfg.web_origin.trim_end_matches('/')
    );
    let body = format!(
        "Confirm your email for NookOS.\n\nOpen this link to verify {email}:\n\n{link}\n\nThe link expires in 24 hours. If you did not request this, ignore this email."
    );

    // Best-effort — a failed send is a reported state, not a crash (AC-5).
    match state
        .mailer
        .send(&email, "Verify your email — NookOS", &body, None)
        .await
    {
        Ok(()) => Ok(RequestVerificationResult {
            sent: true,
            message: format!("A verification link was sent to {email}."),
        }),
        Err(e) => {
            tracing::error!(error = %e, "verification email failed to send");
            Ok(RequestVerificationResult {
                sent: false,
                message: "Could not send the verification email — check the mail configuration and try again.".into(),
            })
        }
    }
}

/// `POST /api/v1/auth/verify-email/confirm` — consume a token and mark the
/// address verified. Token-authenticated (works from any browser). A consumed
/// or expired token is refused (AC-2); re-confirming an already-verified address
/// is a no-op success (AC-5).
#[utoipa::path(post, path = "/api/v1/auth/verify-email/confirm",
    operation_id = "confirm_email_verification",
    request_body = ConfirmVerificationRequest,
    responses((status = 200, body = ConfirmVerificationResult)))]
pub async fn confirm(
    State(state): State<AppState>,
    Json(req): Json<ConfirmVerificationRequest>,
) -> ApiResult<Json<ConfirmVerificationResult>> {
    Ok(Json(confirm_core(&state.db, &req.token).await?))
}

/// The confirm state machine, split from the handler for testing. Consumes a
/// live token and verifies; refuses a consumed/expired/unknown token; treats a
/// used link on an already-verified address as an idempotent success.
pub async fn confirm_core(db: &sqlx::PgPool, token: &str) -> ApiResult<ConfirmVerificationResult> {
    let decline = |msg: &str| {
        Ok(ConfirmVerificationResult {
            verified: false,
            message: msg.to_string(),
        })
    };

    // (id, user_id, email, consumed_at, expired)
    type TokenRow = (
        uuid::Uuid,
        UserId,
        String,
        Option<chrono::DateTime<chrono::Utc>>,
        bool,
    );
    let row: Option<TokenRow> = sqlx::query_as(
        "SELECT id, user_id, email, consumed_at, expires_at < now()
             FROM email_verification_tokens WHERE token_hash = $1",
    )
    .bind(hash_token(token))
    .fetch_optional(db)
    .await?;

    let Some((id, user_id, email, consumed_at, expired)) = row else {
        return decline("this verification link is not valid");
    };

    if consumed_at.is_some() {
        // Re-opening a used link after verifying is a no-op success (AC-5).
        return if email_is_verified(db, user_id).await? {
            Ok(ConfirmVerificationResult {
                verified: true,
                message: "your email is already verified".into(),
            })
        } else {
            decline("this verification link has already been used")
        };
    }
    if expired {
        return decline("this verification link has expired");
    }

    // Consume then verify, in that order — a replayed token finds it consumed.
    sqlx::query("UPDATE email_verification_tokens SET consumed_at = now() WHERE id = $1")
        .bind(id)
        .execute(db)
        .await?;
    mark_local_email_verified(db, user_id, &email).await?;

    Ok(ConfirmVerificationResult {
        verified: true,
        message: "your email is verified".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::{confirm_core, request_core};
    use crate::config::Config;
    use crate::seed::hash_token;
    use crate::services::identity::email_is_verified;
    use crate::state::AppState;
    use nook_types::UserId;
    use sqlx::postgres::PgPoolOptions;
    use sqlx::PgPool;
    use uuid::Uuid;

    async fn pool() -> Option<PgPool> {
        if std::env::var("NOOK_REQUIRE_DB").ok().as_deref() != Some("1") {
            return None;
        }
        let db = PgPoolOptions::new()
            .max_connections(2)
            .connect(&std::env::var("DATABASE_URL").ok()?)
            .await
            .ok()?;
        crate::MIGRATOR.run(&db).await.ok()?;
        Some(db)
    }

    async fn tenant(db: &PgPool, name: &str) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO tenants (id, name, slug) VALUES ($1,$2,$3)")
            .bind(id)
            .bind(name)
            .bind(format!("{name}-{id}"))
            .execute(db)
            .await
            .unwrap();
        id
    }

    /// A user row. `local` decides whether it has a password hash (local
    /// account) or none (OIDC-style).
    async fn user(db: &PgPool, tenant: Uuid, email: &str, local: bool) -> UserId {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO users (id, tenant_id, display_name, email, username, password_hash, role, person_id)
             VALUES ($1, $2, 'U', $3, $4, $5, 'member', gen_random_uuid())",
        )
        .bind(id)
        .bind(tenant)
        .bind(email)
        .bind(if local { Some(email) } else { None })
        .bind(if local { Some("argon2$hash") } else { None })
        .execute(db)
        .await
        .unwrap();
        UserId(id)
    }

    async fn live_tokens(db: &PgPool, uid: UserId) -> Vec<(String,)> {
        sqlx::query_as(
            "SELECT token_hash FROM email_verification_tokens WHERE user_id = $1 AND consumed_at IS NULL",
        )
        .bind(uid)
        .fetch_all(db)
        .await
        .unwrap()
    }

    async fn cleanup(db: &PgPool, tenants: &[Uuid]) {
        for t in tenants {
            // Child rows key off the user, which keys off the tenant.
            let _ = sqlx::query("DELETE FROM email_verification_tokens WHERE user_id IN (SELECT id FROM users WHERE tenant_id = $1)").bind(t).execute(db).await;
            let _ = sqlx::query("DELETE FROM identities WHERE user_id IN (SELECT id FROM users WHERE tenant_id = $1)").bind(t).execute(db).await;
            let _ = sqlx::query("DELETE FROM tenant_members WHERE tenant_id = $1")
                .bind(t)
                .execute(db)
                .await;
            let _ = sqlx::query("DELETE FROM users WHERE tenant_id = $1")
                .bind(t)
                .execute(db)
                .await;
            let _ = sqlx::query("DELETE FROM tenants WHERE id = $1")
                .bind(t)
                .execute(db)
                .await;
        }
    }

    #[tokio::test]
    async fn request_issues_a_hashed_token_sends_and_replaces() {
        let Some(db) = pool().await else { return };
        let state = AppState::new(db.clone(), Config::for_test(), None).await;
        let t = tenant(&db, "verify-req").await;
        let uid = user(&db, t, "local@vr.test", true).await;

        let r = request_core(&state, uid).await.unwrap();
        assert!(r.sent, "the capture mailer accepts the send");
        let tokens = live_tokens(&db, uid).await;
        assert_eq!(tokens.len(), 1, "one live token");
        // Stored hashed: a 64-char hex digest, never the `evr_` plaintext.
        assert_eq!(tokens[0].0.len(), 64);
        assert!(!tokens[0].0.starts_with("evr_"), "token is hashed at rest");

        // Re-request replaces rather than stacks (AC-3).
        request_core(&state, uid).await.unwrap();
        assert_eq!(
            live_tokens(&db, uid).await.len(),
            1,
            "still exactly one live token"
        );

        cleanup(&db, &[t]).await;
    }

    #[tokio::test]
    async fn request_refuses_a_non_local_account() {
        let Some(db) = pool().await else { return };
        let state = AppState::new(db.clone(), Config::for_test(), None).await;
        let t = tenant(&db, "verify-oidc").await;
        let uid = user(&db, t, "oidc@vr.test", false).await;

        let err = request_core(&state, uid).await.unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("local"),
            "OIDC users cannot request local verification (NG-1): {err}"
        );
        cleanup(&db, &[t]).await;
    }

    #[tokio::test]
    async fn confirm_consumes_verifies_refuses_reuse_and_is_idempotent() {
        let Some(db) = pool().await else { return };
        let t = tenant(&db, "verify-confirm").await;
        let uid = user(&db, t, "c@vr.test", true).await;

        // A live token inserted directly with a known plaintext.
        let token = "evr_known_token_value_0000000000";
        sqlx::query(
            "INSERT INTO email_verification_tokens (id, user_id, email, token_hash, expires_at)
             VALUES ($1, $2, 'c@vr.test', $3, now() + interval '1 hour')",
        )
        .bind(Uuid::now_v7())
        .bind(uid)
        .bind(hash_token(token))
        .execute(&db)
        .await
        .unwrap();

        assert!(
            !email_is_verified(&db, uid).await.unwrap(),
            "starts unverified"
        );

        // A wrong token is refused, leaving the address unverified.
        let bad = confirm_core(&db, "evr_nope").await.unwrap();
        assert!(!bad.verified);

        // The real token verifies.
        let ok = confirm_core(&db, token).await.unwrap();
        assert!(ok.verified);
        assert!(email_is_verified(&db, uid).await.unwrap(), "now verified");

        // Re-opening the now-consumed link is an idempotent success (AC-5).
        let again = confirm_core(&db, token).await.unwrap();
        assert!(again.verified && again.message.contains("already"));

        // An expired token is refused (AC-2).
        let expired_plain = "evr_expired_000000000000000000000";
        sqlx::query(
            "INSERT INTO email_verification_tokens (id, user_id, email, token_hash, expires_at)
             VALUES ($1, $2, 'c@vr.test', $3, now() - interval '1 hour')",
        )
        .bind(Uuid::now_v7())
        .bind(uid)
        .bind(hash_token(expired_plain))
        .execute(&db)
        .await
        .unwrap();
        let exp = confirm_core(&db, expired_plain).await.unwrap();
        assert!(!exp.verified && exp.message.contains("expired"));

        cleanup(&db, &[t]).await;
    }
}
