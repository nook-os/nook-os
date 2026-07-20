//! Thin REST wrappers over the kanban work lifecycle (`services::taskwork`).

use axum::extract::{Path, State};
use axum::Json;
use nook_types::*;

use crate::auth::AuthCtx;
use crate::error::ApiResult;
use crate::services::taskwork;
use crate::state::AppState;

#[utoipa::path(post, path = "/api/v1/tasks/{id}/dispatch",
    operation_id = "task_dispatch",
    params(("id" = String, Path,)),
    responses((status = 200, body = TaskItem)))]
pub async fn dispatch(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<TaskId>,
) -> ApiResult<Json<TaskItem>> {
    // Placing work on a scheduler-chosen machine is an operator action; a node
    // token cannot constrain it to itself, so it does not get to do it.
    auth.require_user()?;
    Ok(Json(taskwork::dispatch(&state, auth.tenant_id, id).await?))
}

#[utoipa::path(post, path = "/api/v1/tasks/{id}/start-work",
    operation_id = "task_start_work",
    params(("id" = String, Path,)),
    request_body = StartWorkRequest,
    responses((status = 200, body = StartWorkResponse)))]
pub async fn start_work(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<TaskId>,
    Json(req): Json<StartWorkRequest>,
) -> ApiResult<Json<StartWorkResponse>> {
    // Creates a worktree and a session on whichever node the task names.
    auth.require_user()?;
    let (task, session) = taskwork::start_work(
        &state,
        auth.tenant_id,
        Some(auth.user_id),
        id,
        taskwork::StartWork {
            node_id: req.node_id,
            runtime: req.runtime,
            branch: req.branch,
            workspace_id: req.workspace_id,
        },
    )
    .await?;
    Ok(Json(StartWorkResponse { task, session }))
}

#[utoipa::path(post, path = "/api/v1/tasks/{id}/submit-pr",
    operation_id = "task_submit_pr",
    params(("id" = String, Path,)),
    request_body = SubmitPrRequest,
    responses((status = 200, body = TaskItem)))]
pub async fn submit_pr(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<TaskId>,
    Json(req): Json<SubmitPrRequest>,
) -> ApiResult<Json<TaskItem>> {
    Ok(Json(
        taskwork::submit_pr(&state, auth.tenant_id, id, req.pr_url).await?,
    ))
}

#[utoipa::path(post, path = "/api/v1/tasks/{id}/prune-worktree",
    operation_id = "task_prune_worktree",
    params(("id" = String, Path,)),
    responses((status = 200, body = TaskItem)))]
pub async fn prune_worktree(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<TaskId>,
) -> ApiResult<Json<TaskItem>> {
    Ok(Json(
        taskwork::prune_worktree(&state, auth.tenant_id, id).await?,
    ))
}

#[utoipa::path(post, path = "/api/v1/tasks/{id}/move",
    operation_id = "task_move",
    params(("id" = String, Path,)),
    request_body = MoveTaskRequest,
    responses((status = 200, body = TaskItem)))]
pub async fn move_task(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<TaskId>,
    Json(req): Json<MoveTaskRequest>,
) -> ApiResult<Json<TaskItem>> {
    Ok(Json(
        taskwork::move_task(&state, auth.tenant_id, id, &req.column).await?,
    ))
}
