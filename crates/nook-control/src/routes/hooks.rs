//! Inbound forge webhooks (MAIN-554).
//!
//! The door GitHub knocks on. It records what arrived and acts on nothing
//! (NG-1); children 2-5 of the epic read the rows this writes.
//!
//! ## Why the workspace is in the PATH
//!
//! The obvious routing key — `repository.full_name` out of the payload — is not
//! one. `workspaces_remote_idx` is UNIQUE on `(tenant_id, git_remote_normalized)`,
//! so two tenants may legitimately hold the same repository and there is no
//! fleet-wide remote→workspace lookup to make. The workspace is therefore named
//! by whoever configured the hook, and the repository is checked against it as a
//! consistency assert.
//!
//! ## Why it lives under `/api/`
//!
//! `deploy/docker/nginx.conf.template` proxies only
//! `^/(api|mcp|healthz|\.well-known)/?` to the control plane; everything else
//! falls through to the SPA. A `/hooks/github` route would be answered with
//! `index.html` — a 200 that recorded nothing, which is the worst possible
//! failure for a receiver.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use nook_types::*;

use crate::error::{ApiError, ApiResult};
use crate::repo::forge_deliveries::NewDelivery;
use crate::services::forge_webhook as hook;
use crate::state::AppState;

/// `POST /api/v1/hooks/github/{workspace_id}` — one signed delivery.
///
/// **Takes no `AuthCtx`, which is how a route opts out of auth here** (the
/// precedent is `/invites/preview`). GitHub carries no session and no tenant;
/// the signature is the authentication, and the workspace in the path is what
/// names the tenant. Nothing else about the request is trusted.
///
/// The outcomes, each distinct and each meaning something an operator can act
/// on from GitHub's own delivery log:
///
/// | code | when |
/// |------|------|
/// | 202  | signed, consistent, recorded |
/// | 200  | a redelivery of something already recorded — no second row |
/// | 401  | no signature, or one that does not verify |
/// | 404  | no such workspace, or no secret configured on it |
/// | 413  | body over the route's 8 MiB limit — no row |
/// | 422  | the payload names a different repository — recorded as an error |
///
/// 404 rather than 401 for a workspace with no secret is deliberate: "there is
/// nothing here" is the true statement and the one that tells an operator to
/// generate a secret, where a 401 would send them to check the one they pasted.
/// (There is no archived state on a workspace today, so "unknown or archived"
/// reduces to the row not being there.)
#[utoipa::path(post, path = "/api/v1/hooks/github/{workspace_id}",
    operation_id = "receive_github_webhook",
    params(("workspace_id" = String, Path,)),
    request_body = serde_json::Value,
    responses(
        (status = 202, body = ForgeDeliveryAck),
        (status = 200, body = ForgeDeliveryAck),
        (status = 401), (status = 404), (status = 413),
        (status = 422, body = ForgeDeliveryAck)))]
pub async fn github(
    State(state): State<AppState>,
    Path(workspace): Path<WorkspaceId>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<ForgeDeliveryAck>)> {
    let Some(target) = state.workspaces.webhook_target(workspace).await? else {
        return Err(ApiError::NotFound);
    };
    let Some(sealed) = target.webhook_secret_enc else {
        return Err(ApiError::NotFound);
    };
    // A secret that will not unseal is this deployment's problem, not the
    // caller's — the wrong `SECRETS_KEY` — so it is a 500 and not a 401.
    let secret = state
        .vault
        .decrypt_string(&sealed)
        .map_err(|_| ApiError::Internal(anyhow::anyhow!("webhook secret would not unseal")))?;

    // Verified against the RAW bytes, BEFORE anything parses them: re-serializing
    // a parsed payload produces different bytes and would verify nothing.
    if !header(&headers, hook::SIGNATURE_HEADER)
        .is_some_and(|sig| hook::verify(&secret, &body, sig))
    {
        return Err(ApiError::Unauthorized);
    }

    let (Some(delivery_id), Some(event)) = (
        header(&headers, hook::DELIVERY_HEADER),
        header(&headers, hook::EVENT_HEADER),
    ) else {
        return Err(ApiError::BadRequest(
            "a delivery must carry X-GitHub-Delivery and X-GitHub-Event".into(),
        ));
    };
    let (delivery_id, event) = (delivery_id.to_string(), event.to_string());

    let payload: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|e| ApiError::BadRequest(format!("the delivery body is not JSON: {e}")))?;
    let repo_full_name = payload
        .pointer("/repository/full_name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let action = payload
        .get("action")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let mismatch = !hook::repo_matches(target.git_remote_normalized.as_deref(), &repo_full_name);
    let status = if mismatch {
        hook::STATUS_ERROR
    } else {
        hook::status_for(&event)
    };
    let error = mismatch.then(|| {
        format!(
            "the delivery names {repo_full_name}, but this workspace is a checkout of {}",
            target.git_remote_normalized.as_deref().unwrap_or("nothing")
        )
    });

    let recorded = state
        .forge_deliveries
        .record(NewDelivery {
            tenant: target.tenant_id,
            workspace,
            delivery_id: delivery_id.clone(),
            event: event.clone(),
            action,
            repo_full_name,
            payload,
            status,
            error,
        })
        .await?;

    tracing::info!(
        %workspace, %event, delivery = %delivery_id, status, recorded,
        "forge delivery"
    );

    let ack = ForgeDeliveryAck {
        delivery_id,
        event,
        status: status.to_string(),
        duplicate: !recorded,
    };
    // A mismatch stays a 422 whether or not it is the first time it arrived —
    // the outcome describes the delivery, not the bookkeeping. Otherwise a
    // redelivery is a 200 and a first delivery a 202, which is what makes
    // GitHub's **Redeliver** button a safe thing for an operator to press.
    let code = match (mismatch, ack.duplicate) {
        (true, _) => StatusCode::UNPROCESSABLE_ENTITY,
        (false, true) => StatusCode::OK,
        (false, false) => StatusCode::ACCEPTED,
    };
    Ok((code, Json(ack)))
}

fn header<'h>(headers: &'h HeaderMap, name: &str) -> Option<&'h str> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
}
