use axum::extract::{Path, State};
use axum::Json;
use nook_types::*;

use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::services::core;
use crate::state::AppState;

/// Which tenant owns a node.
///
/// Authorization for anything done TO a node is scoped to the node's tenant,
/// not the caller's — otherwise an operator could only ever act on machines in
/// their own tenant, which is the opposite of the job.
pub(crate) async fn node_tenant(state: &AppState, id: NodeId) -> ApiResult<nook_types::TenantId> {
    let row: Option<(nook_types::TenantId,)> =
        sqlx::query_as("SELECT tenant_id FROM nodes WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.db)
            .await?;
    row.map(|(t,)| t).ok_or(ApiError::NotFound)
}

#[utoipa::path(get, path = "/api/v1/nodes",
    operation_id = "list_nodes", responses((status = 200, body = [Node])))]
pub async fn list(State(state): State<AppState>, auth: AuthCtx) -> ApiResult<Json<Vec<Node>>> {
    Ok(Json(core::list_nodes(&state.db, auth.tenant_id).await?))
}

#[utoipa::path(get, path = "/api/v1/nodes/{id}",
    operation_id = "get_node",
    params(("id" = String, Path,)),
    responses((status = 200, body = Node), (status = 404)))]
pub async fn get_one(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<NodeId>,
) -> ApiResult<Json<Node>> {
    let node: Option<Node> = sqlx::query_as(
        "SELECT id, tenant_id, name, hostname, platform, capabilities, resources, status,
                last_seen_at, created_at, updated_at
         FROM nodes WHERE tenant_id = $1 AND id = $2",
    )
    .bind(auth.tenant_id)
    .bind(id)
    .fetch_optional(&state.db)
    .await?;
    node.map(Json).ok_or(ApiError::NotFound)
}

#[utoipa::path(delete, path = "/api/v1/nodes/{id}",
    operation_id = "delete_node",
    params(("id" = String, Path,)),
    responses((status = 204), (status = 404)))]
pub async fn delete(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<NodeId>,
) -> ApiResult<axum::http::StatusCode> {
    // `node.manage` on the node's OWN tenant, not merely "is a person".
    // `require_user` let any signed-in account delete a node in its tenant,
    // which was fine when a tenant was one person and is not a permission
    // model. A tenant admin holds this for their own tenant; an operator holds
    // it everywhere, which is how they can evict a machine they do not own.
    let tenant = node_tenant(&state, id).await?;
    auth.require(
        &state,
        crate::auth::perm::Permission::NodeManage,
        crate::auth::perm::Scope::Tenant(tenant),
    )
    .await?;
    let res = sqlx::query("DELETE FROM nodes WHERE tenant_id = $1 AND id = $2")
        .bind(tenant)
        .bind(id)
        .execute(&state.db)
        .await?;
    if res.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// Ask a node to rescan its workspace roots now, instead of waiting for the
/// periodic sweep. Backs `nook import`.
#[utoipa::path(post, path = "/api/v1/nodes/{id}/rescan",
    operation_id = "rescan_node",
    params(("id" = String, Path,)),
    responses((status = 202), (status = 404)))]
pub async fn rescan(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<NodeId>,
) -> ApiResult<axum::http::StatusCode> {
    // A node rescans itself; asking another to is an instruction, not a report.
    auth.require_node_self(id)?;
    let owned: Option<(NodeId,)> =
        sqlx::query_as("SELECT id FROM nodes WHERE id = $1 AND tenant_id = $2")
            .bind(id)
            .bind(auth.tenant_id)
            .fetch_optional(&state.db)
            .await?;
    if owned.is_none() {
        return Err(ApiError::NotFound);
    }
    if !state
        .registry
        .send_to_node(id, nook_proto::ControlToNode::RescanWorkspaces)
    {
        return Err(ApiError::BadRequest("node is offline".into()));
    }
    Ok(axum::http::StatusCode::ACCEPTED)
}

/// POST /api/v1/nodes/{id}/update — tell a node to replace its agent and restart.
///
/// A person asking, rather than the automatic path: a node already updates
/// itself on reconnect when its version differs from what this control plane
/// expects. This is for the case where you do not want to wait for a
/// reconnect, or where the automatic path declined and you want to see why.
///
/// The node decides whether it can. It knows whether anything would restart it
/// and refuses if not, because an agent that replaces its binary and exits
/// unsupervised simply goes offline — and doing that across a fleet takes every
/// machine at once.
#[utoipa::path(post, path = "/api/v1/nodes/{id}/update",
    operation_id = "update_node_agent",
    params(("id" = String, Path,)),
    responses((status = 202, description = "asked"), (status = 400, description = "node is offline")))]
pub async fn update(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<NodeId>,
) -> ApiResult<axum::http::StatusCode> {
    // Replacing a machine's binary is an act on the fleet, not a self-report:
    // `node.manage` on the node's own tenant, so a node token cannot trigger it
    // on a peer and an operator can trigger it anywhere.
    let tenant = node_tenant(&state, id).await?;
    auth.require(
        &state,
        crate::auth::perm::Permission::NodeManage,
        crate::auth::perm::Scope::Tenant(tenant),
    )
    .await?;
    let owned: Option<(NodeId,)> =
        sqlx::query_as("SELECT id FROM nodes WHERE id = $1 AND tenant_id = $2")
            .bind(id)
            .bind(tenant)
            .fetch_optional(&state.db)
            .await?;
    if owned.is_none() {
        return Err(ApiError::NotFound);
    }
    if !state
        .registry
        .send_to_node(id, nook_proto::ControlToNode::UpdateAgent)
    {
        return Err(ApiError::BadRequest(
            "that node is offline — it will update itself when it reconnects".into(),
        ));
    }
    crate::events::record(
        &state,
        auth.tenant_id,
        crate::events::EventDraft::new("node.update_requested")
            .actor("user", auth.user_id.0)
            .node(id),
    )
    .await;
    Ok(axum::http::StatusCode::ACCEPTED)
}
