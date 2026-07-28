//! REST surface for loop jobs (MAIN-127): create from a ticket/epic, read with
//! transcript, cancel, and re-run. Thin wrappers over `services::jobs`.

use axum::extract::{Path, State};
use axum::Json;
use nook_types::*;

use crate::auth::AuthCtx;
use crate::error::ApiResult;
use crate::services::jobs;
use crate::state::AppState;

#[utoipa::path(post, path = "/api/v1/jobs",
    operation_id = "job_create",
    request_body = CreateLoopJobRequest,
    responses((status = 200, body = LoopJobDetail)))]
pub async fn create(
    State(state): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<CreateLoopJobRequest>,
) -> ApiResult<Json<LoopJobDetail>> {
    // Creating detached work is a person's action, not a machine's — a node
    // token cannot enqueue loop jobs on the tenant's behalf.
    auth.require_user()?;
    Ok(Json(
        jobs::create(&state, auth.tenant_id, auth.user_id, req).await?,
    ))
}

#[utoipa::path(get, path = "/api/v1/jobs/{id}",
    operation_id = "job_get",
    params(("id" = String, Path,)),
    responses((status = 200, body = LoopJobDetail)))]
pub async fn get(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<JobId>,
) -> ApiResult<Json<LoopJobDetail>> {
    Ok(Json(
        jobs::get(&state, auth.tenant_id, auth.user_id, id).await?,
    ))
}

#[utoipa::path(get, path = "/api/v1/tasks/{task_id}/jobs",
    operation_id = "task_jobs",
    params(("task_id" = String, Path,)),
    responses((status = 200, body = [LoopJob])))]
pub async fn list_for_task(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(task_id): Path<TaskId>,
) -> ApiResult<Json<Vec<LoopJob>>> {
    Ok(Json(
        jobs::list_for_task(&state, auth.tenant_id, auth.user_id, task_id).await?,
    ))
}

#[utoipa::path(post, path = "/api/v1/jobs/{id}/cancel",
    operation_id = "job_cancel",
    params(("id" = String, Path,)),
    responses((status = 200, body = LoopJob)))]
pub async fn cancel(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<JobId>,
) -> ApiResult<Json<LoopJob>> {
    auth.require_user()?;
    Ok(Json(
        jobs::cancel(&state, auth.tenant_id, auth.user_id, id).await?,
    ))
}

#[utoipa::path(post, path = "/api/v1/jobs/{id}/rerun",
    operation_id = "job_rerun",
    params(("id" = String, Path,)),
    responses((status = 200, body = LoopJobDetail)))]
pub async fn rerun(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<JobId>,
) -> ApiResult<Json<LoopJobDetail>> {
    auth.require_user()?;
    Ok(Json(
        jobs::rerun(&state, auth.tenant_id, auth.user_id, id).await?,
    ))
}
