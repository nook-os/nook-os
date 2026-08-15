//! The inbound-email door (MAIN-329).
//!
//! The delivery route's whole behaviour lives in
//! [`crate::services::email_inbound`] — this file is the HTTP shape and
//! nothing else, which is what let MAIN-333's IMAP source reuse the pipeline
//! without reusing a handler.
//!
//! The poller's own configuration is here too (MAIN-333). It is ordinary,
//! authenticated, tenant-scoped CRUD, and it is in this file rather than in
//! `settings` for one reason: the password. A settings value is plaintext JSON
//! written through a generic endpoint with no seam to seal a field on its way
//! in; this handler seals before it stores.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use nook_types::*;

use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::repo::email_pollers::{EmailPoller as StoredPoller, NewEmailPoller};
use crate::services::email_imap as poller;
use crate::services::email_inbound::{self as inbound, Disposition};
use crate::state::AppState;

/// `POST /api/v1/email/inbound` — one signed delivery from the mail provider's
/// inbound-parse webhook.
///
/// **Takes no `AuthCtx`, which is how a route opts out of auth here** (the
/// precedent is `/hooks/github/{workspace_id}`, and before it
/// `/invites/preview`). A mail provider carries no session and no tenant; the
/// signature over the raw body is the authentication, and the recipient
/// address is what names the tenant.
///
/// | code | when |
/// |------|------|
/// | 202  | signed and understood — filed, or dropped by the trust gate |
/// | 400  | signed, but not a payload this source can normalize — including one whose sender the provider did not verify, or did not report on |
/// | 401  | no signature, one that does not verify, or one too old to accept |
/// | 404  | this deployment has no inbound secret — it receives no mail |
/// | 413  | body over the route's 25 MiB limit |
///
/// A 400 is deliberately distinguishable from a drop: it says the *delivery*
/// was malformed, which is the operator's problem and not a fact about who is
/// on an allow-list. Only the gate's two verdicts are hidden behind the 202.
///
/// **A drop is a 202, exactly like an accept.** The provider must not retry a
/// message we have decided not to act on, and a distinguishable refusal would
/// turn this endpoint into a way to ask "does this deployment serve that
/// address" and "is that person support staff" — from the outside, for free.
/// The disposition is in the body, where the operator reading their provider's
/// delivery log can see it and an enumerator cannot use it.
#[utoipa::path(post, path = "/api/v1/email/inbound",
    operation_id = "receive_inbound_email",
    request_body = serde_json::Value,
    responses(
        (status = 202, body = InboundEmailAck),
        (status = 400), (status = 401), (status = 404), (status = 413)))]
pub async fn inbound(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<InboundEmailAck>)> {
    let signature = headers
        .get(inbound::SIGNATURE_HEADER)
        .and_then(|v| v.to_str().ok());

    let disposition = inbound::receive(
        &state,
        &inbound::ProviderWebhookSource,
        &body,
        signature,
        chrono::Utc::now(),
    )
    .await?;

    let ack = match disposition {
        Disposition::Filed { key, .. } => InboundEmailAck {
            status: "filed".into(),
            task_key: key,
            reason: None,
        },
        Disposition::Dropped(reason) => InboundEmailAck {
            status: "dropped".into(),
            task_key: None,
            reason: Some(reason.to_string()),
        },
    };
    Ok((StatusCode::ACCEPTED, Json(ack)))
}

/// `GET /api/v1/email/poller` — the tenant's IMAP poller, or a 404.
///
/// Never the password, and never the sealed bytes either: see
/// [`nook_types::EmailPoller`] for why the response type has no field they
/// could travel in.
#[utoipa::path(get, path = "/api/v1/email/poller",
    operation_id = "get_email_poller",
    responses((status = 200, body = EmailPoller), (status = 403), (status = 404)))]
pub async fn get_poller(
    State(state): State<AppState>,
    auth: AuthCtx,
) -> ApiResult<Json<EmailPoller>> {
    manage(&state, &auth).await?;
    state
        .email_pollers
        .get(auth.tenant_id)
        .await?
        .map(|p| Json(public(&p)))
        .ok_or(ApiError::NotFound)
}

/// `PUT /api/v1/email/poller` — configure the mailbox this tenant polls.
///
/// The password is sealed with the deployment's vault key HERE, before it
/// reaches the repository, and the repository takes only sealed bytes — so
/// there is no path that stores a plaintext one by forgetting to (AC-4).
///
/// Gated on `tenant.manage` like every other tenant-wide setting: a poller
/// files cards on the tenant's board and spends its investigate runs, and the
/// credential it holds is the tenant's, not the writer's.
#[utoipa::path(put, path = "/api/v1/email/poller",
    operation_id = "put_email_poller",
    request_body = UpdateEmailPollerRequest,
    responses((status = 200, body = EmailPoller), (status = 400), (status = 403),
              // 428: `email.inbound` is not set, so there is no allow-list to apply.
              (status = 428)))]
pub async fn put_poller(
    State(state): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<UpdateEmailPollerRequest>,
) -> ApiResult<Json<EmailPoller>> {
    manage(&state, &auth).await?;

    let host = req.host.trim().to_string();
    if host.is_empty() {
        return Err(ApiError::BadRequest("a poller needs a host".into()));
    }
    if req.password.is_empty() {
        return Err(ApiError::BadRequest("a poller needs a password".into()));
    }
    let port = req.port.unwrap_or(poller::DEFAULT_PORT);
    if !(1..=65535).contains(&port) {
        return Err(ApiError::BadRequest(format!("{port} is not a port")));
    }
    let interval = req
        .poll_interval_secs
        .unwrap_or(poller::DEFAULT_POLL_INTERVAL_SECS);
    if interval < poller::MIN_POLL_INTERVAL_SECS {
        return Err(ApiError::BadRequest(format!(
            "the poll interval must be at least {}s",
            poller::MIN_POLL_INTERVAL_SECS
        )));
    }
    let mailbox = req
        .mailbox
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| poller::DEFAULT_MAILBOX.to_string());

    // Refused at the door rather than at the first poll: a control character
    // cannot be escaped into an IMAP quoted string, and a credential carrying
    // one is a command the poller would otherwise send on the operator's behalf
    // (see `services::imap::quoted`).
    for value in [&host, &req.username, &req.password, &mailbox] {
        if value.chars().any(char::is_control) {
            return Err(ApiError::BadRequest(
                "an IMAP host, mailbox or credential may not contain a control character".into(),
            ));
        }
    }

    // Refused rather than stored: a poller with no `email.inbound` allow-list
    // trusts nobody, so it would pull every message in the mailbox and drop
    // each one. Saying so here is the difference between an operator seeing a
    // 428 and an operator watching a correctly-configured mailbox file nothing.
    let configured = state
        .settings
        .tenant_value(auth.tenant_id, inbound::SETTING_KEY)
        .await?
        .and_then(|v| {
            v.get("allow_from")
                .and_then(|a| a.as_array())
                .map(|a| !a.is_empty())
        })
        .unwrap_or(false);
    if !configured {
        return Err(ApiError::SetupRequired(format!(
            "set {} first — it holds the allow-list this poller applies, and without one \
             every message it pulls is dropped",
            inbound::SETTING_KEY
        )));
    }

    let password_enc = state
        .vault
        .encrypt(req.password.as_bytes())
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("sealing the IMAP password: {e}")))?;

    let stored = state
        .email_pollers
        .put(NewEmailPoller {
            tenant: auth.tenant_id,
            host,
            port,
            username: req.username.trim().to_string(),
            password_enc,
            mailbox,
            poll_interval_secs: interval,
            enabled: req.enabled.unwrap_or(true),
        })
        .await?;
    Ok(Json(public(&stored)))
}

/// `DELETE /api/v1/email/poller` — stop polling and forget the credential.
#[utoipa::path(delete, path = "/api/v1/email/poller",
    operation_id = "delete_email_poller",
    responses((status = 204), (status = 403), (status = 404)))]
pub async fn delete_poller(State(state): State<AppState>, auth: AuthCtx) -> ApiResult<StatusCode> {
    manage(&state, &auth).await?;
    match state.email_pollers.delete(auth.tenant_id).await? {
        true => Ok(StatusCode::NO_CONTENT),
        false => Err(ApiError::NotFound),
    }
}

async fn manage(state: &AppState, auth: &AuthCtx) -> ApiResult<()> {
    auth.require(
        state,
        crate::auth::perm::Permission::TenantManage,
        crate::auth::perm::Scope::Tenant(auth.tenant_id),
    )
    .await
}

fn public(poller: &StoredPoller) -> EmailPoller {
    EmailPoller {
        host: poller.host.clone(),
        port: poller.port,
        username: poller.username.clone(),
        has_password: !poller.password_enc.is_empty(),
        mailbox: poller.mailbox.clone(),
        poll_interval_secs: poller.poll_interval_secs,
        enabled: poller.enabled,
        last_polled_at: poller.last_polled_at,
        last_error: poller.last_error.clone(),
    }
}
