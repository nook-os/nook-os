//! REST surface for loop jobs (MAIN-127): create from a ticket/epic, read with
//! transcript, cancel, and re-run. Thin wrappers over `services::jobs`.

use axum::extract::{Path, Query, State};
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

/// `GET /api/v1/jobs/{id}/email/message` — the investigate run reads the
/// message it was seeded from, decrypted (MAIN-331 AC-4).
///
/// The only route in this deployment that returns the plaintext of somebody's
/// support mail, which is why it is job-scoped and kind-checked rather than
/// hung off the link: the caller must BE the run seeded for that message.
#[utoipa::path(get, path = "/api/v1/jobs/{id}/email/message",
    operation_id = "job_email_message",
    params(("id" = String, Path,)),
    responses((status = 200, body = DecryptedMessage), (status = 400), (status = 404)))]
pub async fn email_message(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<JobId>,
) -> ApiResult<Json<DecryptedMessage>> {
    Ok(Json(
        crate::services::email_links::investigation::message(
            &state,
            auth.tenant_id,
            auth.user_id,
            id,
        )
        .await?,
    ))
}

/// `POST /api/v1/jobs/{id}/email/investigation` — the investigate run reports
/// what it found (MAIN-331 AC-2), onto the chain it was seeded from.
///
/// `record_build_outcome`'s twin for the read-only kind: one call as the run's
/// last act, so the agent performs no board mechanics of its own. The draft
/// reply is sealed here and never comes back out through the read model.
#[utoipa::path(post, path = "/api/v1/jobs/{id}/email/investigation",
    operation_id = "job_investigation",
    params(("id" = String, Path,)),
    request_body = InvestigationReport,
    responses((status = 200, body = EmailLink), (status = 400), (status = 404)))]
pub async fn investigation(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<JobId>,
    Json(req): Json<InvestigationReport>,
) -> ApiResult<Json<EmailLink>> {
    Ok(Json(
        crate::services::email_links::investigation::record(
            &state,
            auth.tenant_id,
            auth.user_id,
            id,
            &req,
        )
        .await?,
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
/// first (MAIN-455 AC-5), paged (MAIN-557 AC-5).
///
/// The workspace's own window onto work the control plane raised for it. Each
/// row is an ordinary loop job, so its transcript is read through the same
/// endpoint and the same view a spec run's is — there is no second transcript
/// mechanism to keep in step.
///
/// `sort` and `q` are refused: the only order is newest-first, and filtering is
/// the client's (MAIN-557 NG-4). Absent `after` is the first page, which is
/// what it always was.
#[utoipa::path(get, path = "/api/v1/workspaces/{id}/reviews",
    operation_id = "list_workspace_reviews",
    params(("id" = String, Path,), PageQuery),
    responses((status = 200, body = Page<WorkspaceReviewRun>)))]
pub async fn list_reviews_for_workspace(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<WorkspaceId>,
    Query(q): Query<PageQuery>,
) -> ApiResult<Json<Page<WorkspaceReviewRun>>> {
    let args = page_args(&q)?;
    Ok(Json(
        state
            .jobs
            .list_reviews_for_workspace(auth.tenant_id, id, &args)
            .await?
            .into(),
    ))
}

/// `GET /api/v1/workspaces/{id}/builds` — the Builds panel's rows (MAIN-461
/// AC-2): this repo's build runs, newest first, each naming its card by key,
/// paged on the same contract as its review twin.
#[utoipa::path(get, path = "/api/v1/workspaces/{id}/builds",
    operation_id = "list_builds_for_workspace",
    params(("id" = String, Path,), PageQuery),
    responses((status = 200, body = Page<WorkspaceBuildRun>), (status = 404)))]
pub async fn list_builds_for_workspace(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<WorkspaceId>,
    Query(q): Query<PageQuery>,
) -> ApiResult<Json<Page<WorkspaceBuildRun>>> {
    let args = page_args(&q)?;
    Ok(Json(
        state
            .jobs
            .list_builds_for_workspace(auth.tenant_id, auth.user_id, id, &args)
            .await?
            .into(),
    ))
}

/// Both run listings' page arguments, validated the same way — a stale cursor
/// or an unknown sort is the caller's 400, never a silently different page.
///
/// `q` is refused by NAME rather than left to the SQL. These lists declare no
/// searchable column (MAIN-557 NG-4 — filtering is the client's), and the
/// pagination skeleton renders an empty search set as `1 = 0`, so a `q` would
/// otherwise answer `200` with an empty page: "nothing matched" in reply to a
/// question this endpoint does not answer at all. Saying so is the difference
/// between an empty result and an empty result that means something.
fn page_args(q: &PageQuery) -> ApiResult<nook_db::paging::PageArgs> {
    if q.q.as_deref().is_some_and(|s| !s.trim().is_empty()) {
        return Err(crate::error::ApiError::BadRequest(
            "this list does not search — filter the returned rows client-side".into(),
        ));
    }
    q.args(crate::repo::jobs::RUN_PAGE_SORTS)
        .map_err(crate::services::operator_queries::bad_page)
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

/// `GET /api/v1/jobs/{id}/commands` — the commands the caller may run on this
/// run (MAIN-530 AC-1).
///
/// Gated exactly as steering it is (AC-2): a person's action, on a run they can
/// see. A node token gets the same refusal `POST /messages` gives it.
#[utoipa::path(get, path = "/api/v1/jobs/{id}/commands",
    operation_id = "list_job_commands",
    params(("id" = String, Path,)),
    responses((status = 200, body = [ChatCommand]), (status = 403), (status = 404)))]
pub async fn commands(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<JobId>,
) -> ApiResult<Json<Vec<ChatCommand>>> {
    auth.require_user()?;
    jobs::visible(&state, auth.tenant_id, auth.user_id, id).await?;
    Ok(Json(crate::services::agent_commands::catalog()))
}

/// `POST /api/v1/jobs/{id}/commands` — run one of them (AC-1).
#[utoipa::path(post, path = "/api/v1/jobs/{id}/commands",
    operation_id = "run_job_command",
    params(("id" = String, Path,)),
    request_body = RunChatCommand,
    responses((status = 200, body = ChatCommandResult), (status = 400), (status = 403), (status = 404)))]
pub async fn run_command(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<JobId>,
    Json(req): Json<RunChatCommand>,
) -> ApiResult<Json<ChatCommandResult>> {
    auth.require_user()?;
    let job = jobs::visible(&state, auth.tenant_id, auth.user_id, id).await?;
    Ok(Json(
        crate::services::agent_commands::run(&req, || status_text(&state, auth.tenant_id, &job))
            .await?,
    ))
}

/// What `/status` says about a loop run (AC-5): its state, the machine holding
/// it, the card it is about, and — only while it is still queued — why it has
/// not started.
///
/// Every line is read off the job row that already exists (NG-6). The queued
/// reason is the sentence the run view itself renders, and it is CONDITIONAL on
/// the state for the same reason that view makes it conditional: a reason left
/// behind on a row that has since been claimed describes a wait that is over,
/// and repeating it would be inventing a gate for a run that has passed it.
async fn status_text(state: &AppState, tenant: TenantId, job: &LoopJob) -> ApiResult<String> {
    let node = match job.executor_node_id {
        Some(id) => state
            .nodes
            .name_of(id)
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| id.0.to_string()),
        None => "not placed on a machine yet".into(),
    };
    let ticket = match job.target_task_id {
        Some(id) => state
            .tasks
            .key_of(tenant, id)
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| id.0.to_string()),
        // A review run is about a repository and carries no card — the column
        // is nullable for exactly that reason.
        None => "none — this run is about a repo, not a card".into(),
    };

    let mut out = format!(
        "Run: {} ({})\nNode: {node}\nTicket: {ticket}",
        job.state, job.kind
    );
    if job.state == "queued" {
        if let Some(reason) = job.queued_reason.as_deref() {
            out.push_str("\nWaiting: ");
            out.push_str(reason);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod page_arg_tests {
    use super::*;

    fn query(q: Option<&str>, sort: Option<&str>, after: Option<&str>) -> PageQuery {
        PageQuery {
            q: q.map(str::to_string),
            after: after.map(str::to_string),
            limit: None,
            sort: sort.map(str::to_string),
            dir: None,
        }
    }

    fn refusal(r: ApiResult<nook_db::paging::PageArgs>) -> String {
        match r {
            Err(crate::error::ApiError::BadRequest(m)) => m,
            Err(other) => panic!("expected a 400, got {other:?}"),
            Ok(_) => panic!("expected a refusal"),
        }
    }

    #[test]
    fn no_query_string_is_the_first_page() {
        let args = page_args(&query(None, None, None)).expect("the default is always valid");
        assert!(args.cursor.is_none());
        assert!(args.sort.is_id(), "newest first, on the keyset order");
    }

    /// The searchless list says so, rather than answering "nothing matched" —
    /// which is what an empty search set renders as in SQL.
    #[test]
    fn a_search_term_is_refused_by_name() {
        assert!(
            refusal(page_args(&query(Some("MAIN-557"), None, None))).contains("does not search")
        );
    }

    /// Whitespace is how a cleared search box arrives, and it means "no filter"
    /// everywhere else in the contract. It must not become a 400.
    #[test]
    fn a_blank_search_term_is_no_filter_not_a_refusal() {
        page_args(&query(Some("   "), None, None)).expect("a cleared box is not a search");
    }

    #[test]
    fn there_is_no_sortable_column_and_a_stale_cursor_is_refused() {
        assert!(refusal(page_args(&query(None, Some("created"), None))).contains("sort key"));
        assert!(refusal(page_args(&query(None, None, Some("not-a-cursor")))).contains("cursor"));
    }
}
