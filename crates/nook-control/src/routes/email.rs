//! The inbound-email door (MAIN-329).
//!
//! One route, and everything it does lives in
//! [`crate::services::email_inbound`] — this file is the HTTP shape and
//! nothing else, so a second source (IMAP, SMTP) reuses the pipeline without
//! reusing a handler.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use nook_types::*;

use crate::error::ApiResult;
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
