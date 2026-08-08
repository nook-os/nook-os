//! Loop jobs (MAIN-127): the durable `loop_jobs` record and its lifecycle,
//! riding the generic work queue. This is the CORE slice — no executor
//! selection (MAIN-160), no node execution (MAIN-161), no interaction bridging
//! (MAIN-162). Creating a job enqueues a `loop.job` work item; job state is DB
//! state a later consumer drives off queue consumption.
//!
//! Shared by the REST handlers (and, later, MCP) so the surfaces never drift.

use nook_types::*;
use serde_json::json;
use uuid::Uuid;

use crate::error::{ApiError, ApiResult};
use crate::events::{self, EventDraft};
use crate::queue::NewWork;
use crate::state::AppState;

/// The `capabilities @> '{"shared_operator":true}'::jsonb` containment test,
/// routed through the json seam (MAIN-201) so the jsonb operator lives in the
/// Postgres impl, not inline. The flag is a code constant, so `literal` is the
/// injection-safe form here (never user input).
/// The work-queue routing string every loop job enqueues under. A future
/// consumer (MAIN-160) filters `receive` on exactly this.
pub const WORK_TYPE: &str = "loop.job";

/// The job kinds [`create`] accepts — the TICKET-targeted ones. `spec` fills in
/// a ticket; `decompose` breaks an epic into children.
///
/// `review` is deliberately absent: it targets a workspace, not a ticket, so it
/// cannot be raised through a path whose whole input is a task id. It has its
/// own entry point, [`enqueue_review`], which is also where the dedupe lives.
const KINDS: [&str; 3] = ["spec", "decompose", "build"];

/// The workspace-targeted job kind (MAIN-408). Matches the `loop_jobs_kind_check`
/// constraint added by migration 0040.
pub const REVIEW_KIND: &str = "review";

/// The ticket-targeted builder kind (MAIN-383). In the kind CHECK since
/// migration 0049; enqueue is manual here — triggers and convergence are the
/// arc's split 2, not this one.
pub const BUILD_KIND: &str = "build";

/// The runtime a loop job needs authorized on its executor (MAIN-160). Both
/// kinds drive the `nook-spec` / `nook-epic` skills under Claude Code, so the
/// executor must report the `claude` runtime `authorized` (MAIN-126). A single
/// constant because this slice has no other runtime; a future job kind that
/// needs a different one would carry it on the job.
pub const LOOP_RUNTIME: &str = "claude";

/// Terminal states have no outgoing transition — a job there is finished and
/// can only be re-run as a fresh job (AC-5).
fn is_terminal(state: &str) -> bool {
    matches!(state, "completed" | "failed" | "canceled")
}

/// The legal lifecycle graph (AC-6). `cancel` is handled separately: it is
/// allowed from ANY non-terminal state, so it is deliberately not enumerated
/// here for every source.
fn legal_transition(from: &str, to: &str) -> bool {
    // Cancelling out of any live state is always allowed.
    if to == "canceled" {
        return !is_terminal(from);
    }
    matches!(
        (from, to),
        ("queued", "claimed")
            | ("claimed", "running")
            | ("claimed", "failed")
            | ("running", "waiting_on_human")
            | ("running", "completed")
            | ("running", "failed")
            | ("waiting_on_human", "running")
            | ("waiting_on_human", "completed")
            | ("waiting_on_human", "failed")
    )
}

async fn load(state: &AppState, tenant: TenantId, id: JobId) -> ApiResult<LoopJob> {
    state.jobs.get(tenant, id).await?.ok_or(ApiError::NotFound)
}

/// The job's target card. `NotFound` if it is gone or not this tenant's.
async fn load_target(state: &AppState, tenant: TenantId, task_id: TaskId) -> ApiResult<TaskItem> {
    state
        .tasks
        .get_row(tenant, task_id)
        .await?
        .ok_or(ApiError::NotFound)
}

/// Load a job AND enforce that `viewer` may see it — mirroring the create-side
/// check so get/cancel/rerun never expose a private card's job (and its
/// transcript) to a tenant member who cannot see the card (MAIN-76). Returns
/// `NotFound` for both a missing job and an invisible target, so the two are
/// indistinguishable to the caller.
///
/// **A job with no target card is TENANT-VISIBLE** (MAIN-408, Ryan's ruling):
/// a `review` job is about a repository, not somebody's card, and the sweep
/// raises it with no human requester — so there is no owner to scope to and
/// `visible_to` has nothing to evaluate. Any member of the tenant may see it,
/// its notifications and its transcript; tenant scoping is still enforced by
/// `load`. This matches the rule `interactions::subject_visible` has always
/// applied to a job-less ask (`None => true`), so the two surfaces agree.
///
/// The knowing cost: in a multi-team tenant every member can read any review
/// transcript. The alternative considered and rejected was workspace-scoping,
/// which would need a workspace-visibility predicate that does not exist.
async fn load_visible(
    state: &AppState,
    tenant: TenantId,
    viewer: UserId,
    id: JobId,
) -> ApiResult<(LoopJob, Option<TaskItem>)> {
    let job = load(state, tenant, id).await?;
    let Some(task_id) = job.target_task_id else {
        return Ok((job, None));
    };
    let target = load_target(state, tenant, task_id).await?;
    if !crate::services::tasks::visible_to(&target, viewer) {
        return Err(ApiError::NotFound);
    }
    Ok((job, Some(target)))
}

async fn transcript(state: &AppState, id: JobId) -> ApiResult<Vec<LoopJobTranscriptEntry>> {
    state.jobs.transcript(id).await
}

async fn detail(state: &AppState, job: LoopJob) -> ApiResult<LoopJobDetail> {
    let transcript = transcript(state, job.id).await?;
    Ok(LoopJobDetail { job, transcript })
}

/// Create a job from a ticket/epic, enqueue its work item, and return it with
/// its (empty) transcript. `decompose` requires the target to be an epic; both
/// require the caller to be able to see the target card.
pub async fn create(
    state: &AppState,
    tenant: TenantId,
    requested_by: UserId,
    req: CreateLoopJobRequest,
) -> ApiResult<LoopJobDetail> {
    if !KINDS.contains(&req.kind.as_str()) {
        return Err(ApiError::BadRequest(format!(
            "unknown job kind {:?} — expected spec, decompose or build. A review \
             job targets a workspace, not a ticket: raise it with POST /api/v1/reviews.",
            req.kind
        )));
    }

    // Accept a UUID or a board key (MAIN-209) — the Loop panel opens by key.
    // `resolve_id` is tenant-scoped and 404s an unknown key.
    let target_id =
        crate::services::tasks::resolve_id(state.tasks.as_ref(), tenant, &req.target_task_id)
            .await?;

    // The target must exist in this tenant and be visible to the requester —
    // a job is not a way to reach a private card you could not otherwise see.
    let target = load_target(state, tenant, target_id).await?;
    if !crate::services::tasks::visible_to(&target, requested_by) {
        return Err(ApiError::NotFound);
    }
    if req.kind == "decompose" && target.type_ != "epic" {
        return Err(ApiError::BadRequest(
            "a decompose job's target must be an epic".into(),
        ));
    }
    if req.kind == BUILD_KIND {
        // One live build run per card (AC-4). The partial unique index added by
        // 0049 is the atomic backstop; this check is what turns the second
        // enqueue into an answer — the job already on it — instead of a 500.
        if let Some(existing) = state.jobs.active_build_for(tenant, target_id).await? {
            return Err(ApiError::Conflict(format!(
                "a build for this card is already in flight: job {existing}"
            )));
        }
    }

    // The seed is the human's opening brief (MAIN-231). Blank is the same as
    // absent — a job opened with whitespace starts from the ticket alone.
    let seed = req
        .seed
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let id = JobId::new();
    let job: LoopJob = state
        .jobs
        .create(crate::repo::jobs::NewLoopJob {
            id,
            tenant,
            kind: req.kind.clone(),
            target_task_id: Some(target_id),
            workspace_id: target.workspace_id,
            requested_by,
            seed: seed.clone(),
            predecessor_job_id: None,
            review_pr_number: None,
            review_head_sha: None,
        })
        .await?;

    // The brief opens the transcript as the human line it is, so every viewing
    // surface shows what the run was asked to do before the agent says anything
    // (AC-1/AC-4 — `append_transcript` fans the live `JobChanged`).
    if let Some(seed) = seed.as_deref() {
        append_transcript(state, job.id, "human", seed).await.ok();
    }

    // Ride the generic queue (AC-2). Payload is the job id as JSON — the
    // consumer re-fetches the row rather than trusting anything else on the
    // envelope. Enqueue AFTER the row exists so a consumer that races us always
    // finds the job.
    state
        .queue
        .enqueue(NewWork::new(
            tenant.0,
            WORK_TYPE,
            serde_json::to_vec(&job.id).unwrap_or_default(),
        ))
        .await?;

    record_job_event(state, tenant, "job.created", &job, is_private(&target)).await;
    detail(state, job).await
}

/// Raise a `review` job against a workspace, unless one is already in flight
/// (MAIN-408 AC-2/AC-3).
///
/// **This is the ONLY way a review job is created** — both the manual endpoint
/// and the board-signal sweep call it, and neither has its own notion of
/// "already queued". That is AC-3 stated as code: two enqueue paths with two
/// dedupe rules is how one workspace ends up reviewed twice concurrently, and
/// the way to make that impossible is to leave only one path.
///
/// Returns `Ok(None)` when a live review already exists — deduped, not an
/// error, because both callers treat "already covered" as success. That is also
/// what makes AC-4 hold: the sweep may run forever without the queue growing,
/// since a `queued`, `claimed`, `running` or `waiting_on_human` review all count
/// as in flight.
/// Raise one managed run for one work item (MAIN-455).
///
/// The dedupe is the DATABASE's: 0046's partial unique index refuses a second
/// live run for the same (workspace, item). Two control-plane replicas
/// converging the same instant therefore cannot both raise one, and the loser
/// gets `None` rather than an error — the same shape `claim_for_executor` uses
/// for the same reason.
///
/// The item's label is the seed, so the agent is TOLD which PR it owns instead
/// of filtering a list to discover it. That is what retired the shard
/// arithmetic: a run that knows its item needs no partition.
#[allow(clippy::too_many_arguments)]
pub async fn raise_run(
    state: &AppState,
    tenant: TenantId,
    requested_by: UserId,
    workspace: WorkspaceId,
    kind: &str,
    item: &crate::services::work_source::WorkItem,
    note: Option<&str>,
) -> ApiResult<Option<LoopJob>> {
    let job = match state
        .jobs
        .create(crate::repo::jobs::NewLoopJob {
            id: JobId::new(),
            tenant,
            kind: kind.to_string(),
            target_task_id: None,
            workspace_id: Some(workspace),
            requested_by,
            seed: Some(item.label.clone()),
            predecessor_job_id: None,
            review_pr_number: Some(item.key),
            review_head_sha: Some(item.fingerprint.clone()),
        })
        .await
    {
        Ok(j) => j,
        // A unique-index violation here is the dedupe WORKING, not a fault:
        // another replica raised this run between our read and our write, which
        // is precisely what 0046's index exists to arbitrate.
        Err(crate::error::ApiError::Db(e)) if e.is_unique_violation() => return Ok(None),
        Err(e) => return Err(e),
    };

    append_transcript(state, job.id, "human", &item.label)
        .await
        .ok();
    if let Some(note) = note.map(str::trim).filter(|n| !n.is_empty()) {
        append_transcript(state, job.id, "human", note).await.ok();
    }

    // Enqueue AFTER the row exists, so a consumer racing us always finds it —
    // the same ordering `enqueue_review` relies on.
    state
        .queue
        .enqueue(NewWork::new(
            tenant.0,
            WORK_TYPE,
            serde_json::to_vec(&job.id).unwrap_or_default(),
        ))
        .await?;

    record_job_event(state, tenant, "job.created", &job, false).await;
    Ok(Some(job))
}

/// Record what a review run concluded, and deliver it (MAIN-455; NG-4 of
/// MAIN-448 overturned by owner ruling 2026-08-08 — code posts, the agent only
/// concludes).
///
/// Ordering is deliberate: GitHub FIRST, the database second. A verdict stored
/// but unposted is invisible to every human working in GitHub, while a verdict
/// posted but unstored merely re-raises one run at this head — which will then
/// skip against the comment it finds. The failure that costs less is the one
/// left possible.
pub async fn record_verdict(
    state: &AppState,
    tenant: TenantId,
    job_id: JobId,
    req: &nook_types::ReviewVerdictRequest,
) -> ApiResult<LoopJob> {
    const VERDICTS: [(&str, Option<&str>); 4] = [
        ("approved", Some("loop-approved")),
        ("changes_requested", Some("loop-changes-requested")),
        ("needs_human", Some("needs-human-review")),
        // A skip posts nothing: it defers to a review already on the PR.
        ("skipped", None),
    ];
    let Some((_, label)) = VERDICTS.iter().find(|(v, _)| *v == req.verdict) else {
        return Err(ApiError::BadRequest(format!(
            "verdict must be one of approved|changes_requested|needs_human|skipped, got {:?}",
            req.verdict
        )));
    };

    let job = state
        .jobs
        .get(tenant, job_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let (Some(pr), Some(head), Some(workspace)) = (
        job.review_pr_number,
        job.review_head_sha.as_deref(),
        job.workspace_id,
    ) else {
        return Err(ApiError::BadRequest(
            "only a directed review run records a verdict".into(),
        ));
    };

    if let Some(label) = label {
        let body = req
            .body
            .as_deref()
            .map(str::trim)
            .filter(|b| !b.is_empty())
            .ok_or_else(|| ApiError::BadRequest("a posted verdict needs a body".into()))?;
        let ws = state
            .workspaces
            .get(tenant, workspace)
            .await?
            .ok_or(ApiError::NotFound)?;
        let repo = ws
            .git_remote_url
            .as_deref()
            .and_then(crate::services::forge::github_repo)
            .ok_or_else(|| {
                ApiError::BadRequest("this workspace's remote is not a GitHub repository".into())
            })?;
        // The workspace's own identity first (MAIN-456): a tenant that
        // configured a token posts as itself, and the fleet variable is only
        // the single-tenant fallback.
        let forge = match crate::services::workspace_gh_token(state, tenant, workspace).await {
            Some(t) => crate::services::forge::GithubForge::from_token(&t),
            None => crate::services::forge::GithubForge::from_env().ok_or_else(|| {
                ApiError::BadRequest(
                    "no GitHub token — set one on the workspace (or NOOK_GH_TOKEN for the                      fleet); the verdict cannot be posted, so it is not recorded"
                        .into(),
                )
            })?,
        };
        forge
            .post_verdict(&repo, pr.max(0) as u64, head, label, body)
            .await
            .map_err(|e| ApiError::BadRequest(format!("posting the verdict failed: {e}")))?;
    }

    if state.jobs.set_review_verdict(job_id, &req.verdict).await? == 0 {
        return Err(ApiError::Conflict(
            "this run is not live — a verdict lands before the run finishes".into(),
        ));
    }
    append_transcript(
        state,
        job_id,
        "system",
        &format!("verdict: {}", req.verdict),
    )
    .await
    .ok();
    state.jobs.reload(job_id).await
}

/// "Review this workspace NOW" — the manual path, and it is the SAME
/// convergence the reconciler runs, not a second kind of review (MAIN-455).
///
/// It used to raise one undirected job and leave the agent to scan the queue
/// and pick — the last place selection reasoning lived. Directed runs ended
/// that: this raises one run per pull request that is owed one, through the
/// same `owed()` rule, the same dedupe index, and the same ceiling. A repo
/// with no forge raises nothing, and the counts say so rather than a job that
/// would have found nothing to scan.
pub async fn enqueue_review(
    state: &AppState,
    tenant: TenantId,
    requested_by: UserId,
    workspace: WorkspaceId,
    seed: Option<String>,
) -> ApiResult<crate::services::run_reconcile::Converged> {
    let ws = state
        .workspaces
        .get(tenant, workspace)
        .await?
        .ok_or(ApiError::NotFound)?;
    let ceiling = ws.review_loop_max_replicas.unwrap_or(1).max(0) as usize;
    // A person asking NOW deserves an answer about now, not about the cache's
    // last look up to a TTL ago — they may have opened the PR ten seconds back.
    state.review_demand.forget(workspace);
    let source = crate::services::work_source::ReviewWork {
        demand: &state.review_demand,
        token: crate::services::workspace_gh_token(state, tenant, workspace).await,
    };
    crate::services::run_reconcile::converge(
        state,
        &source,
        tenant,
        requested_by,
        workspace,
        ws.git_remote_url.as_deref(),
        ceiling,
        seed.as_deref(),
    )
    .await
}

/// Read a job with its transcript (AC-3). 404 if it is not this tenant's, or if
/// `viewer` cannot see the job's target card — a private card's transcript stays
/// private (mirrors the create-side gate).
pub async fn get(
    state: &AppState,
    tenant: TenantId,
    viewer: UserId,
    id: JobId,
) -> ApiResult<LoopJobDetail> {
    let (job, _) = load_visible(state, tenant, viewer, id).await?;
    detail(state, job).await
}

/// Every loop job on a ticket, newest first (MAIN-128) — what the ticket's Loop
/// panel lists to find the active/latest run and offer re-run on a failed one.
/// Visibility-gated on the target card (a private card's jobs stay private,
/// mirroring `get`). Transcripts are omitted — this is the cheap list; the panel
/// fetches the chosen job's `get` for its transcript. `NotFound` (not empty) when
/// the caller cannot see the card, so its existence never leaks.
pub async fn list_for_task(
    state: &AppState,
    tenant: TenantId,
    viewer: UserId,
    task_id: TaskId,
) -> ApiResult<Vec<LoopJob>> {
    let target = load_target(state, tenant, task_id).await?;
    if !crate::services::tasks::visible_to(&target, viewer) {
        return Err(ApiError::NotFound);
    }
    state.jobs.list_for_task(tenant, task_id).await
}

/// Move a job to `to`, refusing illegal transitions (AC-6). Records a
/// `job.state_changed` event on success. The single write path for lifecycle
/// changes — cancel and (later) the executor's claim/run/finish all go through
/// here so the legality check lives in one place.
pub async fn transition(
    state: &AppState,
    tenant: TenantId,
    id: JobId,
    to: &str,
) -> ApiResult<LoopJob> {
    let job = load(state, tenant, id).await?;
    if job.state == to {
        return Ok(job);
    }
    if !legal_transition(&job.state, to) {
        return Err(ApiError::Conflict(format!(
            "illegal job transition {} -> {to}",
            job.state
        )));
    }
    let updated: LoopJob = state.jobs.transition(id, to).await?;

    // Privacy of the target card gates the notification (not the activity
    // event) — a private card's state changes must not ring the tenant-wide
    // bell. A vanished target is treated as private (fail closed).
    let private = target_is_private(state, tenant, updated.target_task_id).await;
    record_job_event(state, tenant, "job.state_changed", &updated, private).await;
    // Nudge every live job surface that the job changed (MAIN-128 AC-2). A
    // ticketless review run nudges too — its surface is the Reviews panel.
    state.registry.publish(
        tenant,
        nook_proto::UiEvent::JobChanged {
            task_id: updated.target_task_id,
        },
    );

    // MAIN-162: a job that fails or is canceled cancels any pending interaction
    // it raised — a paused human ask on dead work is moot. (A human who then
    // answers the now-canceled ask is told so clearly; see `interactions::answer`.)
    if matches!(to, "failed" | "canceled") {
        crate::services::interactions::cancel_for_job(state, tenant, id).await;
    }
    Ok(updated)
}

/// Pause a RUNNING job on a human interaction (MAIN-162): `running →
/// waiting_on_human`, persisted so the pause survives CP/node restarts. A no-op
/// for a job not currently running (already paused, or a state where no ask can
/// fire), so raising an interaction never fails on job state.
pub async fn pause_for_human(state: &AppState, tenant: TenantId, id: JobId) -> ApiResult<()> {
    let job = load(state, tenant, id).await?;
    if job.state == "running" {
        transition(state, tenant, id, "waiting_on_human").await?;
    }
    Ok(())
}

/// Resume a PAUSED job once its interaction is answered (MAIN-162):
/// `waiting_on_human → running`. A no-op if the job is not paused (already
/// resumed, canceled, or never paused), so answering never fails on job state.
/// If the executor node is gone the resumed run cannot continue there — that
/// dead-executor case is surfaced by the caller and reaped by MAIN-164.
pub async fn resume_from_human(state: &AppState, tenant: TenantId, id: JobId) -> ApiResult<()> {
    let job = load(state, tenant, id).await?;
    if job.state == "waiting_on_human" {
        transition(state, tenant, id, "running").await?;
    }
    Ok(())
}

/// Cancel a job from any non-terminal state (AC-5). A no-op-style 200 if it is
/// already canceled; a 409 if it already finished. Refuses (as NotFound) a
/// caller who cannot see the target card.
pub async fn cancel(
    state: &AppState,
    tenant: TenantId,
    viewer: UserId,
    id: JobId,
) -> ApiResult<LoopJob> {
    load_visible(state, tenant, viewer, id).await?;
    transition(state, tenant, id, "canceled").await
}

/// Re-run a failed or canceled job as a FRESH job (AC-5): a new row in `queued`,
/// linking back to its predecessor, re-enqueued. The original is left as-is —
/// its transcript is the record of what happened.
pub async fn rerun(
    state: &AppState,
    tenant: TenantId,
    requested_by: UserId,
    id: JobId,
) -> ApiResult<LoopJobDetail> {
    let (prev, target) = load_visible(state, tenant, requested_by, id).await?;
    if !matches!(prev.state.as_str(), "failed" | "canceled") {
        return Err(ApiError::Conflict(
            "only a failed or canceled job can be re-run".into(),
        ));
    }

    let new_id = JobId::new();
    let job: LoopJob = state
        .jobs
        .create(crate::repo::jobs::NewLoopJob {
            id: new_id,
            tenant,
            kind: prev.kind.clone(),
            target_task_id: prev.target_task_id,
            workspace_id: prev.workspace_id,
            requested_by,
            seed: prev.seed.clone(),
            predecessor_job_id: Some(prev.id),
            review_pr_number: None,
            review_head_sha: None,
        })
        .await?;

    // The brief is part of what the job IS, so the successor starts from the
    // same one — a re-run that quietly dropped it would run different work.
    if let Some(seed) = prev.seed.as_deref() {
        append_transcript(state, job.id, "human", seed).await.ok();
    }

    state
        .queue
        .enqueue(NewWork::new(
            tenant.0,
            WORK_TYPE,
            serde_json::to_vec(&job.id).unwrap_or_default(),
        ))
        .await?;

    record_job_event(
        state,
        tenant,
        "job.created",
        &job,
        private_target(target.as_ref()),
    )
    .await;
    detail(state, job).await
}

/// Send an unsolicited steering message to a job (MAIN-231) — the input half of
/// the loop, parallel to (and independent of) the interaction ask/answer model.
///
/// Authorization is the job's subject visibility, exactly as answering an ask:
/// a caller who cannot see the target card gets `NotFound`, never a hint that
/// the job exists. A terminal job is refused with the reason (AC-3) — there is
/// no session left to steer and appending would pretend otherwise.
///
/// On success the message lands in the transcript as `human` (durable and
/// ordered, AC-3; the append fans the live `JobChanged`, AC-4), is pushed to the
/// executor node for delivery into the live session, and — if the run was paused
/// on a human — resumes it exactly like an answer does. A push that does not
/// land is recorded honestly on the transcript rather than silently dropped.
pub async fn post_message(
    state: &AppState,
    tenant: TenantId,
    viewer: UserId,
    id: JobId,
    body: &str,
) -> ApiResult<LoopJobTranscriptEntry> {
    let body = body.trim();
    if body.is_empty() {
        return Err(ApiError::BadRequest("a message needs a body".into()));
    }
    let (job, _) = load_visible(state, tenant, viewer, id).await?;
    if is_terminal(&job.state) {
        return Err(ApiError::Conflict(format!(
            "this job is {} and can no longer be sent messages",
            job.state
        )));
    }

    let entry = append_transcript(state, id, "human", body).await?;

    // Deliver into the run. A job still `queued` has no executor yet — the
    // message waits in the transcript, which the run reads as its context.
    let pushed = match job.executor_node_id {
        Some(node) => state.registry.send_to_node(
            node,
            nook_proto::ControlToNode::JobMessage {
                job_id: job.id.0.to_string(),
                body: body.to_string(),
            },
        ),
        None => false,
    };

    // A steering message that reached no live session must say so, for the same
    // reason an undelivered interaction answer does: the human should not read
    // "sent" as "the agent saw it".
    if !pushed && job.executor_node_id.is_some() {
        append_transcript(
            state,
            id,
            "system",
            "message recorded, but the executor node is offline — it did not reach the run",
        )
        .await
        .ok();
    }

    // A paused run resumes on unsolicited input exactly as it does on an answer
    // (AC-3): the human has spoken, so the wait is over.
    if let Err(e) = resume_from_human(state, tenant, id).await {
        tracing::warn!(job = %id.0, error = %e, "could not resume job on steering message");
    }
    Ok(entry)
}

/// Append one line to a job's transcript (AC-3). The writer API MAIN-161's node
/// execution uses; exposed here so the storage lives with the job, not the
/// executor.
pub async fn append_transcript(
    state: &AppState,
    id: JobId,
    source: &str,
    content: &str,
) -> ApiResult<LoopJobTranscriptEntry> {
    let entry: LoopJobTranscriptEntry = state.jobs.append_transcript(id, source, content).await?;

    // Nudge the live surfaces that a new transcript line landed (MAIN-128 AC-2
    // — the run "streams" as narration arrives). Best-effort: a missing job row
    // just means no live nudge, never a failed append. A review run has no
    // ticket, but it streams all the same — its surface is the workspace's
    // Reviews panel, and the ticketless skip here is what left reviews static
    // while specs streamed (MAIN-455).
    if let Ok(Some((tenant, task_id))) = state.jobs.tenant_and_target_of(id).await {
        state
            .registry
            .publish(tenant, nook_proto::UiEvent::JobChanged { task_id });
    }
    Ok(entry)
}

/// Is the target card private (creator + assignee only)?
fn is_private(target: &TaskItem) -> bool {
    target.visibility == "private"
}

/// The same question for a job that may have no card. No card means nothing to
/// keep private — a review job is tenant-visible, so its bell rings.
fn private_target(target: Option<&TaskItem>) -> bool {
    target.is_some_and(is_private)
}

/// Is the job's target card private? The single answer for every call site that
/// has only a job (not a loaded card): `None` target → not private (a review job
/// is tenant-visible by ruling), a target that will not load → private, failing
/// closed exactly as before.
async fn target_is_private(state: &AppState, tenant: TenantId, target: Option<TaskId>) -> bool {
    match target {
        None => false,
        Some(t) => load_target(state, tenant, t)
            .await
            .map(|t| is_private(&t))
            .unwrap_or(true),
    }
}

/// Record a job lifecycle event on the UI bus (AC-4). `target_private` is carried
/// in the payload so `events::notable()` can suppress the tenant-wide bell for a
/// private target — the activity event still records, but the notification (which
/// could surface transcript/card content) does not fan out. Every job event goes
/// through here so the privacy flag can never be forgotten at a call site.
async fn record_job_event(
    state: &AppState,
    tenant: TenantId,
    kind: &'static str,
    job: &LoopJob,
    target_private: bool,
) {
    events::record(
        state,
        tenant,
        EventDraft::new(kind)
            .actor("user", job.requested_by.0)
            .payload(json!({
                "job_id": job.id,
                "task_id": job.target_task_id,
                // A review job has no task_id; the workspace is what it is about.
                "workspace_id": job.workspace_id,
                "kind": job.kind,
                "state": job.state,
                "target_private": target_private,
            })),
    )
    .await;
}

// ── Executor selection (MAIN-160) ────────────────────────────────────────────

/// Place a queued job on an eligible executor, or leave it queued with the
/// specific reason it could not be placed.
///
/// Eligibility (AC-1): an ONLINE node in the tenant that reports the loop
/// runtime `authorized` (MAIN-126), preferring one **owned by the requester**
/// over the **shared operator** (`shared_operator` in the node's capabilities).
/// No one else's machine is ever eligible.
///
/// The claim is atomic (AC-2): the `UPDATE ... WHERE state = 'queued'` moves
/// exactly one caller from `queued` to `claimed` and stamps `executor_node_id`,
/// so two consumers racing the same job cannot both win — the loser sees zero
/// rows and reads back the winner's result. When nothing is eligible (AC-3) the
/// job stays `queued` and `queued_reason` records which gate failed, to be
/// re-evaluated the next time the job is looked at (a node may have come
/// online). Idempotent: a job already past `queued` is returned unchanged.
pub async fn select_executor(
    state: &AppState,
    tenant: TenantId,
    job_id: JobId,
) -> ApiResult<LoopJob> {
    let job = load(state, tenant, job_id).await?;
    if job.state != "queued" {
        return Ok(job); // already claimed/terminal — nothing to place.
    }

    // The person the requester is — a node's ownership keys on the person, not
    // the per-tenant user (MAIN-130).
    let person: Option<Uuid> = state.identity.person_id_of(job.requested_by).await?;
    let Some(person) = person else {
        return set_queued_reason(state, job_id, "the requester has no person identity").await;
    };

    // Candidates in preference order: owned-and-online-and-authorized first,
    // then the online authorized shared operator. The selection is a `nodes`
    // query and lives on NodeRepository, so there is one definition of who may
    // run work — including the kind filter and the build wall (MAIN-142).
    let candidates: Vec<NodeId> = state
        .nodes
        .eligible_loop_executors(tenant, person, LOOP_RUNTIME, &job.kind)
        .await?;

    // A kind with a placement selector runs on labeled nodes and nowhere else.
    // Review keeps the declaration's own rule (MAIN-455 AC-4) and build filters
    // to `role=build` the same way (MAIN-383 AC-3) — each selector has ONE
    // definition, read here, rather than a second copy of the string.
    let had_candidates = !candidates.is_empty();
    let candidates = if let Some(selector) = placement_selector(&job.kind) {
        let mut kept = Vec::new();
        for node in candidates {
            let Some(row) = state.nodes.get(tenant, node).await? else {
                continue;
            };
            let labels = crate::routes::nodes::placement_of(&row).labels;
            if selector
                .iter()
                .all(|(k, v)| labels.get(k).is_some_and(|got| got == v))
            {
                kept.push(node);
            }
        }
        kept
    } else {
        candidates
    };
    let label_filtered_all = had_candidates && candidates.is_empty();

    // The last gate is how much each candidate is already holding, which is a
    // `loop_jobs` count rather than a node fact — so it is applied here.
    let mut chosen: Option<NodeId> = None;
    let mut blocked_by_capacity = false;
    for node in candidates {
        let cap = state
            .nodes
            .loop_profile(node)
            .await?
            .and_then(|(_, cap)| cap)
            .unwrap_or(CAPACITY_WHEN_UNREPORTED);
        if cap == 0 {
            // A deliberate "stop claiming" rather than a busy node.
            blocked_by_capacity = true;
            continue;
        }
        let held = state.jobs.in_flight_on_node(node).await?.len() as u32;
        if held >= cap {
            blocked_by_capacity = true;
            continue;
        }
        chosen = Some(node);
        break;
    }

    let Some(node) = chosen else {
        let reason = if blocked_by_capacity {
            "no eligible executor: every eligible node is at its loop-job capacity".to_string()
        } else if job.kind == BUILD_KIND && label_filtered_all {
            // Honest, and never a fallback (AC-3): eligible nodes exist but
            // none wears the label, and the reason says which label to set
            // rather than blaming auth or declarations that are in fact fine.
            "no eligible executor: no online eligible node carries the role=build label              — set it on a node that may build (Nodes page edits labels)"
                .to_string()
        } else {
            no_executor_reason(state, tenant, person, &job.kind).await?
        };
        return set_queued_reason(state, job_id, &reason).await;
    };

    // Re-asked at CLAIM, of the stored row, independent of the pick above
    // (MAIN-142 AC-2/AC-3). The two checks are deliberately not shared code
    // paths: this one is what holds if a node's report changes between the
    // query and the claim, or if a future caller reaches the claim by another
    // route.
    if let Some(refusal) = kind_wall_refusal(state, node, &job.kind).await? {
        return set_queued_reason(state, job_id, &refusal).await;
    }

    // Atomic claim: only the caller that flips `queued` -> `claimed` wins.
    let claimed: Option<LoopJob> = state.jobs.claim_for_executor(job_id, node).await?;

    match claimed {
        Some(job) => {
            let private = target_is_private(state, tenant, job.target_task_id).await;
            record_job_event(state, tenant, "job.state_changed", &job, private).await;
            Ok(job)
        }
        // Lost the race — another consumer claimed it. Return the current row.
        None => load(state, tenant, job_id).await,
    }
}

/// Capacity assumed for a node that reports none — an agent old enough to
/// predate `max_loop_jobs` (MAIN-142). The shipped default rather than
/// unlimited: an unreported cap should behave like the configuration everything
/// else ships with, not like permission to take every job in the queue.
pub const CAPACITY_WHEN_UNREPORTED: u32 = 2;

/// WHERE a build may run: nodes the owner labeled `role=build`, and nowhere
/// else (MAIN-383 AC-3). A builder pushes code with credentials; placement is
/// an owner's explicit act — a label set on the Nodes page — never an accident
/// of being online. Old-style `role=build` labels widen to this per-role key
/// exactly as `role=loop` does (MAIN-463).
fn build_selector() -> std::collections::BTreeMap<String, String> {
    [("role/build".to_string(), "true".to_string())]
        .into_iter()
        .collect()
}

/// The placement selector a kind requires, if any — the single point dispatch
/// reads, so a new labeled kind is one arm here and one selector definition.
fn placement_selector(kind: &str) -> Option<std::collections::BTreeMap<String, String>> {
    match kind {
        REVIEW_KIND => Some(crate::services::session_reconcile::review_loop_selector()),
        BUILD_KIND => Some(build_selector()),
        _ => None,
    }
}

/// The wall, asked of the STORED node row and answered as the refusal message
/// (MAIN-142 AC-2/AC-3), or `None` when this node may run this kind.
///
/// Two rules, and their order is the whole point. The build rule is checked
/// FIRST and reads only `shared_operator`, so a node declaring
/// `loop_kinds=build` changes nothing about it — the wall is the control
/// plane's, and a node cannot configure its way through. Only then is the
/// node's own declaration consulted, which is a filter we apply on its behalf
/// rather than a permission we take its word for.
pub async fn kind_wall_refusal(
    state: &AppState,
    node: NodeId,
    kind: &str,
) -> ApiResult<Option<String>> {
    if kind == "build" && state.nodes.is_shared_operator(node).await? {
        return Ok(Some(format!(
            "refused: node {node} is a shared operator, and shared operators never run build work"
        )));
    }
    let declared = state
        .nodes
        .loop_profile(node)
        .await?
        .map(|(kinds, _)| kinds)
        .unwrap_or_default();
    if !declared.iter().any(|k| k == kind) {
        return Ok(Some(format!(
            "refused: node {node} does not accept {kind} jobs (it accepts: {})",
            if declared.is_empty() {
                "none".to_string()
            } else {
                declared.join(", ")
            }
        )));
    }
    Ok(None)
}

/// Phrase the specific gate that blocked placement (AC-3): distinguishes "no
/// node of yours is online" from "your online nodes aren't authorized" from "no
/// operator available", so the UI can tell the PM what to do.
async fn no_executor_reason(
    state: &AppState,
    tenant: TenantId,
    person: Uuid,
    kind: &str,
) -> ApiResult<String> {
    let owned_online: i64 = state.nodes.owned_online_count(tenant, person).await?;
    let operator_online: i64 = state.nodes.shared_operator_online_count(tenant).await?;

    Ok(match (owned_online, operator_online) {
        (0, 0) => "no eligible executor: you have no node online and no shared operator is available".into(),
        // Wherever an online node exists, "not authorized" is no longer the only
        // way to be ineligible — it may simply not accept this kind (MAIN-142).
        // The reason names both rather than asserting the one it cannot tell.
        (0, _) => format!(
            "no eligible executor: you have no node online, and the shared operator is not authorized for the {LOOP_RUNTIME} runtime or does not accept {kind} jobs"
        ),
        (_, 0) => format!(
            "no eligible executor: your online node(s) are not authorized for the {LOOP_RUNTIME} runtime or do not accept {kind} jobs, and no shared operator is available"
        ),
        _ => format!(
            "no eligible executor: no online node (yours or the shared operator) is authorized for the {LOOP_RUNTIME} runtime, or none of them accepts {kind} jobs"
        ),
    })
}

/// Record why a job stays queued, without changing its state. A no-op guard on
/// `state = 'queued'` so a concurrent claim is never clobbered by a stale
/// reason write.
async fn set_queued_reason(state: &AppState, job_id: JobId, reason: &str) -> ApiResult<LoopJob> {
    state.jobs.set_queued_reason(job_id, reason).await?;
    // Return the current row (its state is still queued unless a claim raced in).
    state.jobs.reload(job_id).await
}

// ── Node execution dispatch (MAIN-161) ───────────────────────────────────────

/// The Claude Code skill each job kind runs. `decompose` breaks an epic down
/// (`nook-epic`); everything else fills a ticket in (`nook-spec`).
pub fn skill_for_kind(kind: &str) -> &'static str {
    match kind {
        "decompose" => "nook-epic",
        _ => "nook-spec",
    }
}

/// The target ticket's board key (e.g. `MAIN-42`) — what the skill is pointed
/// at. Empty string when there is no key to send: the row has vanished (the
/// caller fails the job), or the job is a `review`, which is pointed at a
/// repository rather than a ticket. The wire field stays a `String` because
/// changing the node protocol is MAIN-408's NG-1.
async fn task_key(
    state: &AppState,
    tenant: TenantId,
    task_id: Option<TaskId>,
) -> ApiResult<String> {
    let Some(task_id) = task_id else {
        return Ok(String::new());
    };
    let key: Option<String> = state.tasks.key_of(tenant, task_id).await?;
    Ok(key.unwrap_or_default())
}

/// Resolve a job's workspace to a clonable git remote + branch, preferring the
/// executor node's own `node_workspaces` row and falling back to any node's row
/// for that workspace. `None` when no row carries a usable remote — the node
/// cannot derive it from a `workspace_id` alone.
pub async fn resolve_repo(
    state: &AppState,
    tenant: TenantId,
    workspace_id: WorkspaceId,
    node: NodeId,
) -> ApiResult<Option<(String, String)>> {
    let row: Option<(Option<String>, Option<String>)> = state
        .workspaces
        .checkout_repo_and_branch(workspace_id, node)
        .await?;
    let row = match row {
        Some(r @ (Some(_), _)) => Some(r),
        // The executor has no usable row — take any node's remote for the ws.
        _ => match state
            .workspaces
            .any_checkout_repo_and_branch(workspace_id)
            .await?
        {
            Some(r @ (Some(_), _)) => Some(r),
            // No node holds a checkout of this workspace yet: a freshly-seeded,
            // never-cloned workspace the loop clones from scratch (MAIN-341's
            // dogfood is exactly this). Its OWN declared remote is the clone
            // URL; the branch defaults to `main` below. Without this, a job
            // dies with "no known git remote" for a workspace that plainly has
            // one — just no `node_workspaces` row yet.
            _ => state
                .workspaces
                .git_remote_url(workspace_id, tenant)
                .await?
                .flatten()
                .map(|url| (Some(url), None)),
        },
    };
    Ok(
        row.and_then(|(url, branch)| {
            url.map(|u| (u, branch.unwrap_or_else(|| "main".to_string())))
        }),
    )
}

/// Hand a freshly-claimed job to its executor node to run (AC-1/AC-2), moving it
/// `claimed`→`running`. Called by the dispatch consumer right after a successful
/// claim. Fails the job honestly (AC-4) when there is nowhere to run it: no
/// workspace, no known remote, or the node dropped between claim and dispatch.
pub async fn dispatch_to_node(state: &AppState, tenant: TenantId, job: &LoopJob) -> ApiResult<()> {
    let Some(node) = job.executor_node_id else {
        return Ok(());
    };
    if job.state != "claimed" {
        return Ok(()); // already dispatched (running) or terminal — idempotent.
    }
    let Some(workspace_id) = job.workspace_id else {
        return fail_with(state, tenant, job.id, "the job has no workspace to run in").await;
    };
    let Some((repo_url, branch)) = resolve_repo(state, tenant, workspace_id, node).await? else {
        return fail_with(
            state,
            tenant,
            job.id,
            "the workspace has no known git remote to clone",
        )
        .await;
    };
    let target_task_key = task_key(state, tenant, job.target_task_id).await?;

    let sent = state.registry.send_to_node(
        node,
        nook_proto::ControlToNode::RunLoopJob {
            // The SAME row `repo_url` came from, so the job's session can export it
            // and git inside authenticates with the workspace's key (MAIN-367).
            workspace_id: Some(workspace_id),
            // …and the key itself, because the session is not the first thing
            // that clones. The job builds a bare mirror in the node's clone
            // cache BEFORE any session exists, so `workspace_id` alone left that
            // step on the node's own generated key and a private repo refused it
            // at "preparing workspace". Same delivery `CloneRepo` uses.
            ssh_key: crate::services::workspace_git_key(state, tenant, workspace_id).await,
            // The workspace's own forge token rides with the run (MAIN-456), so
            // the agent's `gh` speaks as the tenant, not as the fleet. `None`
            // falls back to the node's env on the other end.
            gh_token: crate::services::workspace_gh_token(state, tenant, workspace_id).await,
            // The agent's own identity, in the JOB's tenant, as the person who
            // asked for the run. Revoked in `finish`.
            nook_token: mint_job_token(state, tenant, job.requested_by, job.id).await,
            job_id: job.id.0.to_string(),
            kind: job.kind.clone(),
            // Which PR this run owns, so the agent is told rather than having to
            // find its share, and so it can resume that PR's session.
            review_pr_number: job.review_pr_number.map(|n| n.max(0) as u64),
            target_task_key,
            repo_url,
            branch,
            seed: job.seed.clone(),
        },
    );
    if !sent {
        return fail_with(
            state,
            tenant,
            job.id,
            "the executor node went offline before the job could start",
        )
        .await;
    }
    append_transcript(state, job.id, "system", "dispatched to executor node")
        .await
        .ok();
    transition(state, tenant, job.id, "running").await?;
    Ok(())
}

/// Apply a node's `JobFinished` (AC-2/AC-4): `completed` on success, else
/// `failed` with the reason/tail preserved. Idempotent through `transition`.
pub async fn finish(
    state: &AppState,
    tenant: TenantId,
    id: JobId,
    ok: bool,
    message: &str,
) -> ApiResult<()> {
    if !ok && !message.trim().is_empty() {
        append_transcript(state, id, "system", message).await.ok();
    }
    let _ = transition(state, tenant, id, if ok { "completed" } else { "failed" }).await;
    // The agent's credential dies with the run. Expiry alone would leave a
    // working token on a shared operator node for the rest of its window after
    // the work is done. Unconditional: a FAILED job's token is exactly as live
    // as a successful one's.
    revoke_job_token(state, tenant, id).await;
    Ok(())
}

/// Fail every job a node was executing when it disconnected (AC-4): the session
/// died with the node. Terminal jobs are untouched (the guard in `transition`).
pub async fn fail_stranded_for_node(
    state: &AppState,
    tenant: TenantId,
    node: NodeId,
) -> ApiResult<()> {
    let stranded: Vec<JobId> = state.jobs.in_flight_on_node(node).await?;
    for id in stranded {
        append_transcript(
            state,
            id,
            "system",
            "executor node disconnected — job failed",
        )
        .await
        .ok();
        let _ = transition(state, tenant, id, "failed").await;
    }
    Ok(())
}

/// Record a transcript line and fail the job — the common "nowhere to run it"
/// path. Best-effort: a job already terminal is left as-is by `transition`.
async fn fail_with(state: &AppState, tenant: TenantId, id: JobId, reason: &str) -> ApiResult<()> {
    append_transcript(state, id, "system", reason).await.ok();
    let _ = transition(state, tenant, id, "failed").await;
    Ok(())
}

/// Fail every job whose executor node has gone dark (MAIN-164) — the reaper's
/// one query. Scans jobs in `claimed`/`running` whose `executor_node_id` points
/// at a node last seen more than `grace_secs` ago, moves each to `failed` with a
/// transcript line naming the cause, and emits the standard `job.state_changed`
/// event. Runs across every tenant (a CP replica serves them all); each reaped
/// row carries its own tenant.
///
/// `waiting_on_human` is deliberately NOT in the scan set (AC-2): a paused job
/// waits indefinitely regardless of executor liveness. `claimed → failed` and
/// `running → failed` are the transitions this uses, both already legal in the
/// single transition table (AC-3).
///
/// Multi-instance safe (AC-5): the reap is one conditional
/// `UPDATE ... WHERE state IN ('claimed','running')` guarded on the staleness
/// window — the same atomic pattern as the executor claim. Only the replica whose
/// UPDATE actually flips a row gets it back via `RETURNING`, so two reapers cannot
/// double-fail a job, and a job that resumed or completed between scan and update
/// falls out of the guard untouched. Returns how many jobs were reaped.
pub async fn reap_stale_executors(state: &AppState, grace_secs: u64) -> ApiResult<u64> {
    let reaped = state.jobs.reap_stale_executors(grace_secs as i64).await?;

    for crate::repo::jobs::ReapedJob {
        id,
        tenant,
        target_task_id: target,
        node_last_seen_at: last_seen,
    } in &reaped
    {
        append_transcript(
            state,
            *id,
            "system",
            &format!(
                "executor node offline since {}, reaped after {grace_secs}s",
                last_seen.to_rfc3339()
            ),
        )
        .await
        .ok();
        // Emit the same `job.state_changed` event any transition would — loaded
        // back so the payload shape matches, with the target's privacy gating the
        // tenant-wide bell. (The atomic UPDATE above already made the state
        // change; this is only its announcement.)
        if let Ok(job) = load(state, *tenant, *id).await {
            let private = target_is_private(state, *tenant, *target).await;
            record_job_event(state, *tenant, "job.state_changed", &job, private).await;
        }
    }
    Ok(reaped.len() as u64)
}

/// Is `node` the executor the job was placed on? The gate for accepting a node's
/// streamed transcript / finish (MAIN-161 security): a node token is scoped to
/// its OWN runs, so it must not be able to inject into or terminate another
/// executor's job. `false` for a missing job, an unplaced one, or a mismatch.
async fn is_executor(
    state: &AppState,
    tenant: TenantId,
    id: JobId,
    node: NodeId,
) -> ApiResult<bool> {
    let exec: Option<Option<NodeId>> = state.jobs.executor_of(tenant, id).await?;
    Ok(matches!(exec, Some(Some(n)) if n == node))
}

/// Append a transcript line reported by a node — ONLY for a job that node is
/// actually executing. A line for someone else's job (or a spoof) is dropped
/// with a warning, never applied (MAIN-161 security).
pub async fn transcript_from_node(
    state: &AppState,
    tenant: TenantId,
    node: NodeId,
    id: JobId,
    source: &str,
    content: &str,
) -> ApiResult<()> {
    if !is_executor(state, tenant, id, node).await? {
        tracing::warn!(job = %id.0, node = %node.0, "node streamed transcript for a job it does not execute — dropped");
        return Ok(());
    }
    append_transcript(state, id, source, content).await?;
    Ok(())
}

/// Record a turn boundary reported by the executor (MAIN-240).
///
/// The agent-working indicator used to be inferred from whether output was
/// arriving; this is the runtime saying so itself. Nothing is persisted — a
/// turn is a live fact, not history, and the transcript already carries what
/// was said — so this only fans the UI signal out.
///
/// Same anti-spoof gate as the transcript: a node may only speak for a job it
/// is actually executing.
pub async fn turn_from_node(
    state: &AppState,
    tenant: TenantId,
    node: NodeId,
    id: JobId,
    active: bool,
) {
    match is_executor(state, tenant, id, node).await {
        Ok(true) => {}
        _ => {
            tracing::warn!(job = %id.0, node = %node.0, "node reported a turn for a job it does not execute — dropped");
            return;
        }
    }
    if let Ok(Some(task_id)) = state.jobs.target_task_of_unscoped(id).await {
        state.registry.publish(
            tenant,
            nook_proto::UiEvent::JobTurn {
                task_id,
                job_id: id,
                active,
            },
        );
    }
}

/// Apply a node's `JobFinished` — ONLY for a job that node is actually
/// executing, so a node token cannot complete or fail another executor's job
/// (MAIN-161 security).
pub async fn finish_from_node(
    state: &AppState,
    tenant: TenantId,
    node: NodeId,
    id: JobId,
    ok: bool,
    message: &str,
) -> ApiResult<()> {
    if !is_executor(state, tenant, id, node).await? {
        tracing::warn!(job = %id.0, node = %node.0, "node reported finish for a job it does not execute — dropped");
        return Ok(());
    }
    finish(state, tenant, id, ok, message).await
}

/// The name a job's token carries, so it is identifiable in Settings → Access
/// tokens and findable for revocation without a second table.
fn job_token_name(id: JobId) -> String {
    format!("loop-job {}", id.0)
}

/// Mint the credential the agent inside a loop job acts with.
///
/// Without this the agent shells out to `nook` and `AuthConfig::load()` reads a
/// FILE — whatever `nook login` last wrote on the executor. On a shared operator
/// node that is one human's token for one tenant, so a job for another tenant's
/// workspace listed that human's boards and drafted against the wrong one. The
/// job never chose a board; nothing had ever given it an identity to choose
/// with.
///
/// Scoped to the JOB's tenant and issued as `requested_by` — the person who
/// started it. Bot identities are deliberately parked until RBAC lands, so
/// attributing to the initiator is the honest option: they asked for this run,
/// and every board action it takes is theirs.
///
/// **This buys the right TENANT, not least privilege.** `user_tokens` can
/// express only `tenant_id` and `expires_at`; inside that tenant the token can
/// do whatever the initiator can. It is strictly better than a cross-tenant
/// human credential on a shared box, and it is not a sandbox — the job-anchored
/// design that would be is tracked as a follow-up.
///
/// The expiry is a backstop, not the mechanism: [`revoke_job_token`] runs when
/// the job finishes. The window matches the node's own job timeout so a node
/// that dies without reporting cannot leave a usable token behind for long.
pub async fn mint_job_token(
    state: &AppState,
    tenant: TenantId,
    requested_by: UserId,
    id: JobId,
) -> Option<String> {
    let token = crate::routes::join::random_token(crate::auth::USER_TOKEN_PREFIX, 40);
    let new = crate::repo::identity::NewUserToken {
        id: Uuid::now_v7(),
        tenant,
        user_id: requested_by,
        token_hash: crate::seed::hash_token(&token),
        name: job_token_name(id),
        // The node stops a job at 60 minutes; two hours leaves room for the
        // finish report without leaving a long-lived credential lying around.
        expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(2)),
    };
    match state.identity.create_user_token(new).await {
        Ok(()) => Some(token),
        Err(e) => {
            // Not fatal: the job still runs, the agent just falls back to the
            // node's own login exactly as it did before. Loud, because that
            // fallback is the bug this exists to fix.
            tracing::error!(job = %id.0, error = %e, "could not mint a job token — the agent will fall back to the node's login and may see the wrong tenant");
            None
        }
    }
}

/// Revoke a job's token the moment the job ends, whatever the outcome.
///
/// Expiry alone would leave a working credential on a shared node for the rest
/// of its window after the work is done. Best-effort by design: a failure here
/// must not turn a finished job into a failed one, and the expiry still bounds
/// it.
pub async fn revoke_job_token(state: &AppState, tenant: TenantId, id: JobId) {
    if let Err(e) = state
        .identity
        .revoke_user_tokens_named(tenant, &job_token_name(id))
        .await
    {
        tracing::warn!(job = %id.0, error = %e, "could not revoke the job token; it expires on its own");
    }
}
