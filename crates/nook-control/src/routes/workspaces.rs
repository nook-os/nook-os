use axum::extract::{Path, State};
use axum::Json;
use nook_types::*;

use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::events::{self, EventDraft};
use crate::services::{core, identity::slugify};
use crate::state::AppState;
use nook_proto::ControlToNode;

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
        is_repo: payload.is_repo,
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

/// Rename a workspace — the label only.
///
/// Deliberately does NOT touch the slug, the checkouts on disk, or the git
/// remote: those are the workspace's identity, and rediscovery matches on
/// them. So calling a clone of `acme/services` "the flaky one" costs nothing
/// and breaks nothing — no directory moves, no session loses its path, and
/// the next heartbeat won't create a duplicate workspace.
#[utoipa::path(patch, path = "/api/v1/workspaces/{id}",
    operation_id = "rename_workspace",
    params(("id" = String, Path,)),
    request_body = RenameWorkspaceRequest,
    responses((status = 200, body = Workspace), (status = 400), (status = 404)))]
pub async fn rename(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<WorkspaceId>,
    Json(req): Json<RenameWorkspaceRequest>,
) -> ApiResult<Json<Workspace>> {
    let name = req.name.trim();
    if name.is_empty() {
        return Err(ApiError::BadRequest("a workspace needs a name".into()));
    }
    if name.chars().count() > 120 {
        return Err(ApiError::BadRequest(
            "workspace name must be 120 characters or fewer".into(),
        ));
    }

    let previous: Option<(String,)> =
        sqlx::query_as("SELECT name FROM workspaces WHERE id = $1 AND tenant_id = $2")
            .bind(id)
            .bind(auth.tenant_id)
            .fetch_optional(&state.db)
            .await?;
    let (previous,) = previous.ok_or(ApiError::NotFound)?;

    let workspace: Workspace = sqlx::query_as(
        "UPDATE workspaces SET name = $3, updated_at = now()
         WHERE id = $1 AND tenant_id = $2 RETURNING *",
    )
    .bind(id)
    .bind(auth.tenant_id)
    .bind(name)
    .fetch_one(&state.db)
    .await?;

    // A `workspace.*` event is what makes every other open tab redraw the new
    // name without a refresh.
    events::record(
        &state,
        auth.tenant_id,
        EventDraft::new("workspace.renamed")
            .actor("user", auth.user_id.0)
            .workspace(id)
            .payload(serde_json::json!({ "from": previous, "to": name })),
    )
    .await;

    Ok(Json(workspace))
}

#[utoipa::path(delete, path = "/api/v1/workspaces/{id}",
    operation_id = "delete_workspace",
    params(("id" = String, Path,)),
    request_body = DeleteWorkspaceRequest,
    responses(
        (status = 200, body = DeleteWorkspaceResponse),
        (status = 404),
        (status = 409, description = "the workspace still has live sessions")))]
pub async fn delete(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<WorkspaceId>,
    body: Option<Json<DeleteWorkspaceRequest>>,
) -> ApiResult<Json<DeleteWorkspaceResponse>> {
    let Json(req) = body.unwrap_or_default();

    let workspace: Option<Workspace> =
        sqlx::query_as("SELECT * FROM workspaces WHERE id = $1 AND tenant_id = $2")
            .bind(id)
            .bind(auth.tenant_id)
            .fetch_optional(&state.db)
            .await?;
    let workspace = workspace.ok_or(ApiError::NotFound)?;

    // Live sessions would be killed by the cascade with their tmux left
    // orphaned on the node — make the caller deal with them first.
    let (live,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM sessions
         WHERE workspace_id = $1 AND status IN ('starting', 'running', 'detached')",
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;
    if live > 0 {
        return Err(ApiError::Conflict(format!(
            "{live} live session(s) — kill them first"
        )));
    }

    let checkouts: Vec<(NodeId, String)> =
        sqlx::query_as("SELECT node_id, path FROM node_workspaces WHERE workspace_id = $1")
            .bind(id)
            .fetch_all(&state.db)
            .await?;
    let total = checkouts.len();
    let mut removed = 0usize;

    if req.delete_files {
        // Worktrees first: removing a primary clone out from under its linked
        // worktrees would leave them dangling.
        let mut ordered = checkouts.clone();
        ordered.sort_by_key(|(_, path)| path.matches('/').count());
        ordered.reverse();
        for (node_id, path) in ordered {
            let Some(rx) =
                state
                    .registry
                    .request_op(node_id, |request_id| ControlToNode::RemoveCheckout {
                        request_id,
                        path: path.clone(),
                    })
            else {
                continue; // node offline — the checkout stays
            };
            if let Ok(Ok(payload)) =
                tokio::time::timeout(std::time::Duration::from_secs(30), rx).await
            {
                if payload.ok {
                    removed += 1;
                } else {
                    tracing::warn!(%node_id, error = %payload.message, "checkout removal failed");
                }
            }
        }
    }

    // Cascades node_workspaces, sessions, notes and secrets; tasks and events
    // keep their history with a null workspace.
    sqlx::query("DELETE FROM workspaces WHERE id = $1 AND tenant_id = $2")
        .bind(id)
        .bind(auth.tenant_id)
        .execute(&state.db)
        .await?;

    events::record(
        &state,
        auth.tenant_id,
        EventDraft::new("workspace.deleted")
            .actor("user", auth.user_id.0)
            .payload(serde_json::json!({
                "name": workspace.name,
                "checkouts_removed": removed,
                "deleted_files": req.delete_files,
            })),
    )
    .await;

    let remaining = total - removed;
    let message = if remaining > 0 {
        format!(
            "deleted '{}' — {remaining} checkout(s) still on disk and will be \
             rediscovered until removed",
            workspace.name
        )
    } else {
        format!("deleted '{}'", workspace.name)
    };
    Ok(Json(DeleteWorkspaceResponse {
        deleted: true,
        checkouts_removed: removed,
        checkouts_remaining: remaining,
        message,
    }))
}
