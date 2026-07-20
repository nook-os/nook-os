use axum::extract::{Path, State};
use axum::Json;
use nook_types::*;

use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::services::{core, identity::slugify};
use crate::state::AppState;

#[utoipa::path(get, path = "/api/v1/workspaces",
    operation_id = "list_workspaces",
    responses((status = 200, body = [WorkspaceDetail])))]
pub async fn list(
    State(state): State<AppState>,
    auth: AuthCtx,
) -> ApiResult<Json<Vec<WorkspaceDetail>>> {
    Ok(Json(
        core::list_workspaces(&state.db, auth.tenant_id).await?,
    ))
}

#[utoipa::path(get, path = "/api/v1/workspaces/{id}",
    operation_id = "get_workspace",
    params(("id" = String, Path,)),
    responses((status = 200, body = WorkspaceDetail), (status = 404)))]
pub async fn get_one(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<WorkspaceId>,
) -> ApiResult<Json<WorkspaceDetail>> {
    core::get_workspace(&state.db, auth.tenant_id, id)
        .await?
        .map(Json)
        .ok_or(ApiError::NotFound)
}

#[derive(serde::Deserialize, utoipa::IntoParams)]
pub struct GitQuery {
    pub node_id: NodeId,
}

/// Live git status + working-tree diff, relayed from the node.
#[utoipa::path(get, path = "/api/v1/workspaces/{id}/git",
    operation_id = "workspace_git_status",
    params(("id" = String, Path,), GitQuery),
    responses((status = 200, body = GitStatusResponse), (status = 404)))]
pub async fn git_status(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<WorkspaceId>,
    axum::extract::Query(q): axum::extract::Query<GitQuery>,
) -> ApiResult<Json<GitStatusResponse>> {
    let path: Option<(String,)> = sqlx::query_as(
        "SELECT path FROM node_workspaces
         WHERE tenant_id = $1 AND workspace_id = $2 AND node_id = $3",
    )
    .bind(auth.tenant_id)
    .bind(id)
    .bind(q.node_id)
    .fetch_optional(&state.db)
    .await?;
    let Some((path,)) = path else {
        return Err(ApiError::NotFound);
    };

    let rx = state
        .registry
        .request_git_status(q.node_id, path)
        .ok_or_else(|| ApiError::BadRequest("node is offline".into()))?;
    let payload = tokio::time::timeout(std::time::Duration::from_secs(10), rx)
        .await
        .map_err(|_| ApiError::BadRequest("node did not answer in time".into()))?
        .map_err(|_| ApiError::BadRequest("node disconnected".into()))?;

    Ok(Json(GitStatusResponse {
        branch: payload.branch,
        dirty: !payload.files.is_empty(),
        files: payload.files,
        diff: payload.diff,
    }))
}

#[utoipa::path(post, path = "/api/v1/workspaces",
    operation_id = "create_workspace",
    request_body = CreateWorkspaceRequest,
    responses((status = 200, body = Workspace)))]
pub async fn create(
    State(state): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<CreateWorkspaceRequest>,
) -> ApiResult<Json<Workspace>> {
    let workspace: Workspace = sqlx::query_as(
        "INSERT INTO workspaces (id, tenant_id, name, slug, description)
         VALUES ($1, $2, $3, $4, $5) RETURNING *",
    )
    .bind(WorkspaceId::new())
    .bind(auth.tenant_id)
    .bind(&req.name)
    .bind(slugify(&req.name))
    .bind(&req.description)
    .fetch_one(&state.db)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(d) if d.is_unique_violation() => {
            ApiError::Conflict("a workspace with that name already exists".into())
        }
        _ => e.into(),
    })?;
    Ok(Json(workspace))
}
