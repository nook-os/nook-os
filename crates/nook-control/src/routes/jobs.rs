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

/// "Review this workspace now" (MAIN-455) — the manual counterpart to the
/// reconciler, and the SAME convergence: one directed run per pull request
/// that is owed one, same dedupe, same ceiling. `pr` narrows it to one PR;
/// `force` (MAIN-473) additionally overrules exactly ONE rule — the
/// verdicted-head rest — while the live-run dedupe and the workspace ceiling
/// (including `0 = off`) still stand and refuse by name. The response is what
/// actually happened — the runs raised, plus how many PRs were already
/// covered or held back — because "a job" stopped being the honest unit when
/// a workspace can owe several.
#[utoipa::path(post, path = "/api/v1/reviews",
    operation_id = "review_enqueue",
    request_body = CreateReviewJobRequest,
    responses((status = 200, body = ReviewRaiseResult)))]
pub async fn enqueue_review(
    State(state): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<CreateReviewJobRequest>,
) -> ApiResult<Json<ReviewRaiseResult>> {
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

    let c = jobs::enqueue_review(
        &state,
        auth.tenant_id,
        auth.user_id,
        workspace,
        req.seed,
        req.pr,
        req.force,
    )
    .await?;
    Ok(Json(ReviewRaiseResult {
        raised: c.jobs,
        live: c.live as u32,
        withheld: c.withheld as u32,
    }))
}

/// `POST /api/v1/builds` — build one card NOW (MAIN-458 AC-4): the same
/// convergence the reconciler runs, filtered to the named card — the manual
/// path cannot bypass the dedupe, the claim, or the ceiling.
#[utoipa::path(post, path = "/api/v1/builds",
    operation_id = "build_enqueue",
    request_body = EnqueueBuildRequest,
    responses((status = 200, body = ReviewRaiseResult), (status = 400), (status = 404)))]
pub async fn enqueue_build(
    State(state): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<EnqueueBuildRequest>,
) -> ApiResult<Json<ReviewRaiseResult>> {
    auth.require_user()?;
    let task =
        crate::services::tasks::resolve_id(state.tasks.as_ref(), auth.tenant_id, &req.task).await?;
    let row = state
        .tasks
        .get_row(auth.tenant_id, task)
        .await?
        .ok_or(crate::error::ApiError::NotFound)?;
    if !crate::services::tasks::visible_to(&row, auth.user_id) {
        return Err(crate::error::ApiError::NotFound);
    }
    let workspace = row.workspace_id.ok_or_else(|| {
        crate::error::ApiError::BadRequest(
            "this card has no workspace — a build needs a repo to run in".into(),
        )
    })?;
    let c =
        jobs::converge_builds(&state, auth.tenant_id, auth.user_id, workspace, Some(task)).await?;
    Ok(Json(ReviewRaiseResult {
        raised: c.jobs,
        live: c.live as u32,
        withheld: c.withheld as u32,
    }))
}

/// `POST /api/v1/jobs/{id}/outcome` — a build run reports its conclusion
/// (MAIN-458 AC-2/AC-3): the CP records it and mirrors it to the board, so
/// the agent's last act is one call instead of board mechanics it could
/// misperform.
#[utoipa::path(post, path = "/api/v1/jobs/{id}/outcome",
    operation_id = "job_outcome",
    params(("id" = String, Path,)),
    request_body = BuildOutcomeRequest,
    responses((status = 200, body = LoopJob), (status = 400), (status = 409)))]
pub async fn outcome(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<JobId>,
    Json(req): Json<BuildOutcomeRequest>,
) -> ApiResult<Json<LoopJob>> {
    Ok(Json(
        jobs::record_build_outcome(&state, auth.tenant_id, id, &req).await?,
    ))
}

/// `POST /api/v1/jobs/{id}/verdict` — a review run reports its conclusion
/// (MAIN-455). The run's own minted token authorises it, the same identity its
/// other writes travel as; the control plane posts the comment and labels, so
/// the agent's last act is one call instead of a sequence of `gh` commands it
/// could misperform.
#[utoipa::path(post, path = "/api/v1/jobs/{id}/verdict",
    operation_id = "job_verdict",
    params(("id" = String, Path,)),
    request_body = ReviewVerdictRequest,
    responses((status = 200, body = LoopJob), (status = 400), (status = 409)))]
pub async fn verdict(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<JobId>,
    Json(req): Json<ReviewVerdictRequest>,
) -> ApiResult<Json<LoopJob>> {
    Ok(Json(
        jobs::record_verdict(&state, auth.tenant_id, id, &req).await?,
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

/// `GET /api/v1/workspaces/{id}/reviews` — this repo's review runs, newest
/// first (MAIN-455 AC-5).
///
/// The workspace's own window onto work the control plane raised for it. Each
/// row is an ordinary loop job, so its transcript is read through the same
/// endpoint and the same view a spec run's is — there is no second transcript
/// mechanism to keep in step.
#[utoipa::path(get, path = "/api/v1/workspaces/{id}/reviews",
    operation_id = "list_workspace_reviews",
    params(("id" = String, Path,)),
    responses((status = 200, body = [LoopJob])))]
pub async fn list_reviews_for_workspace(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<WorkspaceId>,
) -> ApiResult<Json<Vec<LoopJob>>> {
    // A page's worth. A repo that gets pushed to all day accumulates one run
    // per push per PR, and none of the older ones tell you anything the newest
    // does not.
    const PAGE: i64 = 50;
    Ok(Json(
        state
            .jobs
            .list_reviews_for_workspace(auth.tenant_id, id, PAGE)
            .await?,
    ))
}

/// `GET /api/v1/workspaces/{id}/builds` — the Builds panel's rows (MAIN-461
/// AC-2): this repo's build runs, newest first, each naming its card by key.
#[utoipa::path(get, path = "/api/v1/workspaces/{id}/builds",
    operation_id = "list_builds_for_workspace",
    params(("id" = String, Path,)),
    responses((status = 200, body = [WorkspaceBuildRun]), (status = 404)))]
pub async fn list_builds_for_workspace(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<WorkspaceId>,
) -> ApiResult<Json<Vec<WorkspaceBuildRun>>> {
    const PAGE: i64 = 50;
    Ok(Json(
        state
            .jobs
            .list_builds_for_workspace(auth.tenant_id, auth.user_id, id, PAGE)
            .await?,
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
