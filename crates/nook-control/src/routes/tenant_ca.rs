//! Tenant-facing CA lifecycle: see the trust bundle, and rotate it.
//!
//! Rotation you cannot observe is rotation nobody will run, so the listing
//! carries the numbers an admin actually needs to decide the next step: which
//! CA signs, which are merely trusted, and how many machines still hold a leaf
//! from each. The retirement guard reads the same number.
//!
//! Every action here is authenticated and role-gated rather than being a side
//! effect of touching a file or a row — and every one is recorded in `events`,
//! *including denials*, because "who rotated this CA, and when" is a question
//! a managed offering eventually has to answer.
//!
//! The private key is never exportable. Tenants control rotation and
//! revocation without ever holding the key; an operator who wants their own
//! key supplies one at generation time instead. Export is a one-way door —
//! once a key has left, you cannot claim it is confined.

use axum::extract::{Path, State};
use axum::Json;
use nook_types::*;

use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::events::{self, EventDraft};
use crate::state::AppState;

/// Record a refused attempt before returning it. A denied CA operation is
/// exactly the kind of thing you want in the log.
async fn deny(state: &AppState, auth: &AuthCtx, action: &str, why: ApiError) -> ApiError {
    events::record(
        state,
        auth.tenant_id,
        EventDraft::new("tenant.ca_denied")
            .actor("user", auth.user_id.0)
            .payload(serde_json::json!({ "action": action })),
    )
    .await;
    why
}

async fn gate(state: &AppState, auth: &AuthCtx, action: &str) -> Result<(), ApiError> {
    match auth.require_tenant_admin(state).await {
        Ok(()) => Ok(()),
        Err(e) => Err(deny(state, auth, action, e).await),
    }
}

/// What this tenant trusts, what signs, and how the rotation is progressing.
#[utoipa::path(get, path = "/api/v1/tenant/cas",
    operation_id = "list_tenant_cas",
    responses((status = 200, body = [TenantCaSummary]), (status = 403)))]
pub async fn list(
    State(state): State<AppState>,
    auth: AuthCtx,
) -> ApiResult<Json<Vec<TenantCaSummary>>> {
    gate(&state, &auth, "list").await?;
    let cas = crate::ca::trust_bundle(&state.db, auth.tenant_id)
        .await
        .map_err(ApiError::Internal)?;

    let mut out = Vec::with_capacity(cas.len());
    for ca in cas {
        let nodes = crate::ca::live_leaves(&state.db, auth.tenant_id, ca.id)
            .await
            .map_err(ApiError::Internal)?;
        out.push(TenantCaSummary {
            id: ca.id.to_string(),
            state: ca.state,
            fingerprint: ca.fingerprint,
            not_after: ca.not_after,
            created_at: ca.created_at,
            // The number that decides whether the old CA can go yet.
            nodes_holding_leaves: nodes,
        });
    }
    Ok(Json(out))
}

/// Stage a new CA: trusted immediately, signing nothing yet.
///
/// Step one of a rotation — distribute before switching, so machines learn the
/// new CA on their next renewal and nothing breaks when it starts signing.
#[utoipa::path(post, path = "/api/v1/tenant/cas",
    operation_id = "stage_tenant_ca",
    responses((status = 200, body = TenantCaSummary), (status = 403)))]
pub async fn stage(
    State(state): State<AppState>,
    auth: AuthCtx,
) -> ApiResult<Json<TenantCaSummary>> {
    gate(&state, &auth, "stage").await?;
    // Never implicitly active: an existing tenant already has a signer, and
    // silently switching would strand every node that hasn't renewed.
    let make_active = crate::ca::trust_bundle(&state.db, auth.tenant_id)
        .await
        .map_err(ApiError::Internal)?
        .is_empty();

    let ca = crate::ca::generate(&state.db, &state.vault, auth.tenant_id, make_active)
        .await
        .map_err(ApiError::Internal)?;

    events::record(
        &state,
        auth.tenant_id,
        EventDraft::new("tenant.ca_staged")
            .actor("user", auth.user_id.0)
            .payload(serde_json::json!({ "ca_id": ca.id, "fingerprint": ca.fingerprint })),
    )
    .await;

    Ok(Json(TenantCaSummary {
        id: ca.id.to_string(),
        state: ca.state,
        fingerprint: ca.fingerprint,
        not_after: ca.not_after,
        created_at: ca.created_at,
        nodes_holding_leaves: 0,
    }))
}

/// Make a staged CA the signer. The previous signer becomes `retiring` —
/// still trusted, no longer issuing.
#[utoipa::path(post, path = "/api/v1/tenant/cas/{id}/promote",
    operation_id = "promote_tenant_ca",
    params(("id" = String, Path,)),
    responses((status = 204), (status = 403), (status = 400)))]
pub async fn promote(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<String>,
) -> ApiResult<axum::http::StatusCode> {
    gate(&state, &auth, "promote").await?;
    let ca_id: uuid::Uuid = id
        .parse()
        .map_err(|_| ApiError::BadRequest("not a CA id".into()))?;

    crate::ca::promote(&state.db, auth.tenant_id, ca_id)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    events::record(
        &state,
        auth.tenant_id,
        EventDraft::new("tenant.ca_promoted")
            .actor("user", auth.user_id.0)
            .payload(serde_json::json!({ "ca_id": ca_id })),
    )
    .await;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// Drop a CA from the trust bundle. Refused while it still has live leaves.
#[utoipa::path(delete, path = "/api/v1/tenant/cas/{id}",
    operation_id = "retire_tenant_ca",
    params(("id" = String, Path,)),
    responses((status = 204), (status = 403), (status = 409)))]
pub async fn retire(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<String>,
) -> ApiResult<axum::http::StatusCode> {
    gate(&state, &auth, "retire").await?;
    let ca_id: uuid::Uuid = id
        .parse()
        .map_err(|_| ApiError::BadRequest("not a CA id".into()))?;

    // The guard lives in ca::retire so it cannot be skipped by another caller.
    crate::ca::retire(&state.db, auth.tenant_id, ca_id)
        .await
        .map_err(|e| ApiError::Conflict(e.to_string()))?;

    events::record(
        &state,
        auth.tenant_id,
        EventDraft::new("tenant.ca_retired")
            .actor("user", auth.user_id.0)
            .payload(serde_json::json!({ "ca_id": ca_id })),
    )
    .await;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// Cut a machine off.
///
/// Distinct from expiry on purpose: a certificate simply running out means
/// "renew me", while revocation means "never again". Collapsing the two would
/// let a compromised machine wait out its certificate and quietly come back.
#[utoipa::path(post, path = "/api/v1/nodes/{id}/revoke",
    operation_id = "revoke_node",
    params(("id" = String, Path,)),
    responses((status = 204), (status = 403), (status = 404)))]
pub async fn revoke_node(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<NodeId>,
) -> ApiResult<axum::http::StatusCode> {
    gate(&state, &auth, "revoke_node").await?;

    // Scoped to the caller's tenant: an admin cannot reach another tenant's
    // machines even by guessing an id.
    let done = sqlx::query(
        "UPDATE nodes SET revoked_at = now(), updated_at = now()
          WHERE id = $1 AND tenant_id = $2",
    )
    .bind(id)
    .bind(auth.tenant_id)
    .execute(&state.db)
    .await?;
    if done.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }

    events::record(
        &state,
        auth.tenant_id,
        EventDraft::new("node.revoked")
            .actor("user", auth.user_id.0)
            .node(id),
    )
    .await;
    Ok(axum::http::StatusCode::NO_CONTENT)
}
