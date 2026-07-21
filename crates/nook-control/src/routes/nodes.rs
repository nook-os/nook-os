use axum::extract::{Path, State};
use axum::Json;
use nook_types::*;

use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::services::core;
use crate::state::AppState;

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
    // One machine should not be able to evict another.
    auth.require_user()?;
    let res = sqlx::query("DELETE FROM nodes WHERE tenant_id = $1 AND id = $2")
        .bind(auth.tenant_id)
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
