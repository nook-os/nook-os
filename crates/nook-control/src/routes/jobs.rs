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

/// Raise a review job by hand (MAIN-408 AC-2) — the manual counterpart to the
/// board-signal sweep, for reviewing something without waiting for the tick.
///
/// Deliberately NOT gated on `reviews.sweep.enabled`: that switch governs the
/// automatic sweep, and a person asking for one review is not the thing an
/// operator turns off when they say "stop sweeping". (An operator who wants no
/// reviews at all removes the ability to reach this route, as with any other
/// endpoint.)
///
/// **Dedupe is shared with the sweep, not reimplemented** (AC-3): a workspace
/// that already has a review in flight returns that existing job with 200,
/// because "already covered" is success from the caller's point of view and a
/// 409 would push every caller into writing its own retry rule.
#[utoipa::path(post, path = "/api/v1/reviews",
    operation_id = "review_enqueue",
    request_body = CreateReviewJobRequest,
    responses((status = 200, body = LoopJobDetail)))]
pub async fn enqueue_review(
    State(state): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<CreateReviewJobRequest>,
) -> ApiResult<Json<LoopJobDetail>> {
    // A person's action, like every other enqueue — a node token cannot raise
    // reviews on the tenant's behalf.
    auth.require_user()?;
    let workspace = crate::services::workspace_queries::resolve_by_key(
        &*state.workspaces,
        auth.tenant_id,
        &req.workspace_id,
    )
    .await
    .map_err(|e| crate::error::ApiError::BadRequest(e.to_string()))?;

    match jobs::enqueue_review(&state, auth.tenant_id, auth.user_id, workspace, req.seed).await? {
        Some(detail) => Ok(Json(detail)),
        // Already in flight: hand back the existing run rather than a second one.
        None => {
            let existing = state
                .jobs
                .active_review_for_workspace(auth.tenant_id, workspace)
                .await?
                .ok_or(crate::error::ApiError::NotFound)?;
            Ok(Json(
                jobs::get(&state, auth.tenant_id, auth.user_id, existing).await?,
            ))
        }
    }
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
    Path(task_id): Path<String>,
) -> ApiResult<Json<Vec<LoopJob>>> {
    // Accept a UUID or a board key (MAIN-209) — the Loop panel opens by key, so
    // the list GET must resolve it like every other task-addressed route.
    let task_id =
        crate::services::tasks::resolve_id(state.tasks.as_ref(), auth.tenant_id, &task_id).await?;
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

#[utoipa::path(post, path = "/api/v1/jobs/{id}/messages",
    operation_id = "job_message",
    params(("id" = String, Path,)),
    request_body = CreateJobMessageRequest,
    responses((status = 200, body = LoopJobTranscriptEntry)))]
pub async fn message(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<JobId>,
    Json(req): Json<CreateJobMessageRequest>,
) -> ApiResult<Json<LoopJobTranscriptEntry>> {
    // Steering a run is a person's action: the transcript line is attributed to
    // a human, and a node has no business volunteering one.
    auth.require_user()?;
    Ok(Json(
        jobs::post_message(&state, auth.tenant_id, auth.user_id, id, &req.body).await?,
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
