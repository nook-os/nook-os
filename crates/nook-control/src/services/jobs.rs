//! Loop jobs (MAIN-127): the durable `loop_jobs` record and its lifecycle,
//! riding the generic work queue. This is the CORE slice — no executor
//! selection (MAIN-160), no node execution (MAIN-161), no interaction bridging
//! (MAIN-162). Creating a job enqueues a `loop.job` work item; job state is DB
//! state a later consumer drives off queue consumption.
//!
//! Shared by the REST handlers (and, later, MCP) so the surfaces never drift.

use nook_types::*;
use serde_json::json;

use crate::error::{ApiError, ApiResult};
use crate::events::{self, EventDraft};
use crate::queue::NewWork;
use crate::state::AppState;

/// The work-queue routing string every loop job enqueues under. A future
/// consumer (MAIN-160) filters `receive` on exactly this.
pub const WORK_TYPE: &str = "loop.job";

/// The two job kinds this slice knows. `spec` fills in a ticket; `decompose`
/// breaks an epic into children.
const KINDS: [&str; 2] = ["spec", "decompose"];

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
    sqlx::query_as("SELECT * FROM loop_jobs WHERE id = $1 AND tenant_id = $2")
        .bind(id)
        .bind(tenant)
        .fetch_optional(&state.db)
        .await?
        .ok_or(ApiError::NotFound)
}

async fn transcript(state: &AppState, id: JobId) -> ApiResult<Vec<LoopJobTranscriptEntry>> {
    Ok(
        sqlx::query_as("SELECT * FROM loop_job_transcript WHERE job_id = $1 ORDER BY id")
            .bind(id)
            .fetch_all(&state.db)
            .await?,
    )
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
            "unknown job kind {:?} — expected one of spec, decompose",
            req.kind
        )));
    }

    // The target must exist in this tenant and be visible to the requester —
    // a job is not a way to reach a private card you could not otherwise see.
    let target: TaskItem = sqlx::query_as("SELECT * FROM tasks WHERE id = $1 AND tenant_id = $2")
        .bind(req.target_task_id)
        .bind(tenant)
        .fetch_optional(&state.db)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !crate::services::tasks::visible_to(&target, requested_by) {
        return Err(ApiError::NotFound);
    }
    if req.kind == "decompose" && target.type_ != "epic" {
        return Err(ApiError::BadRequest(
            "a decompose job's target must be an epic".into(),
        ));
    }

    let id = JobId::new();
    let job: LoopJob = sqlx::query_as(
        "INSERT INTO loop_jobs
            (id, tenant_id, kind, target_task_id, workspace_id, requested_by, state)
         VALUES ($1, $2, $3, $4, $5, $6, 'queued')
         RETURNING *",
    )
    .bind(id)
    .bind(tenant)
    .bind(&req.kind)
    .bind(req.target_task_id)
    .bind(target.workspace_id)
    .bind(requested_by)
    .fetch_one(&state.db)
    .await?;

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

    record_created(state, tenant, &job).await;
    detail(state, job).await
}

/// Read a job with its transcript (AC-3). 404 if it is not this tenant's.
pub async fn get(state: &AppState, tenant: TenantId, id: JobId) -> ApiResult<LoopJobDetail> {
    let job = load(state, tenant, id).await?;
    detail(state, job).await
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
    let updated: LoopJob = sqlx::query_as(
        "UPDATE loop_jobs SET state = $2, updated_at = now()
         WHERE id = $1 RETURNING *",
    )
    .bind(id)
    .bind(to)
    .fetch_one(&state.db)
    .await?;

    events::record(
        state,
        tenant,
        EventDraft::new("job.state_changed")
            .actor("user", updated.requested_by.0)
            .payload(json!({
                "job_id": updated.id,
                "task_id": updated.target_task_id,
                "kind": updated.kind,
                "state": updated.state,
            })),
    )
    .await;
    Ok(updated)
}

/// Cancel a job from any non-terminal state (AC-5). A no-op-style 200 if it is
/// already canceled; a 409 if it already finished.
pub async fn cancel(state: &AppState, tenant: TenantId, id: JobId) -> ApiResult<LoopJob> {
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
    let prev = load(state, tenant, id).await?;
    if !matches!(prev.state.as_str(), "failed" | "canceled") {
        return Err(ApiError::Conflict(
            "only a failed or canceled job can be re-run".into(),
        ));
    }

    let new_id = JobId::new();
    let job: LoopJob = sqlx::query_as(
        "INSERT INTO loop_jobs
            (id, tenant_id, kind, target_task_id, workspace_id, requested_by,
             state, predecessor_job_id)
         VALUES ($1, $2, $3, $4, $5, $6, 'queued', $7)
         RETURNING *",
    )
    .bind(new_id)
    .bind(tenant)
    .bind(&prev.kind)
    .bind(prev.target_task_id)
    .bind(prev.workspace_id)
    .bind(requested_by)
    .bind(prev.id)
    .fetch_one(&state.db)
    .await?;

    state
        .queue
        .enqueue(NewWork::new(
            tenant.0,
            WORK_TYPE,
            serde_json::to_vec(&job.id).unwrap_or_default(),
        ))
        .await?;

    record_created(state, tenant, &job).await;
    detail(state, job).await
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
    Ok(sqlx::query_as(
        "INSERT INTO loop_job_transcript (id, job_id, source, content)
         VALUES ($1, $2, $3, $4) RETURNING *",
    )
    .bind(JobTranscriptId::new())
    .bind(id)
    .bind(source)
    .bind(content)
    .fetch_one(&state.db)
    .await?)
}

async fn record_created(state: &AppState, tenant: TenantId, job: &LoopJob) {
    events::record(
        state,
        tenant,
        EventDraft::new("job.created")
            .actor("user", job.requested_by.0)
            .payload(json!({
                "job_id": job.id,
                "task_id": job.target_task_id,
                "kind": job.kind,
                "state": job.state,
            })),
    )
    .await;
}
