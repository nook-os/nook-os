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

/// The work-queue routing string every loop job enqueues under. A future
/// consumer (MAIN-160) filters `receive` on exactly this.
pub const WORK_TYPE: &str = "loop.job";

/// The two job kinds this slice knows. `spec` fills in a ticket; `decompose`
/// breaks an epic into children.
const KINDS: [&str; 2] = ["spec", "decompose"];

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
    sqlx::query_as("SELECT * FROM loop_jobs WHERE id = $1 AND tenant_id = $2")
        .bind(id)
        .bind(tenant)
        .fetch_optional(&state.db)
        .await?
        .ok_or(ApiError::NotFound)
}

/// The job's target card. `NotFound` if it is gone or not this tenant's.
async fn load_target(state: &AppState, tenant: TenantId, task_id: TaskId) -> ApiResult<TaskItem> {
    sqlx::query_as("SELECT * FROM tasks WHERE id = $1 AND tenant_id = $2")
        .bind(task_id)
        .bind(tenant)
        .fetch_optional(&state.db)
        .await?
        .ok_or(ApiError::NotFound)
}

/// Load a job AND enforce that `viewer` may see its target card — mirroring the
/// create-side check so get/cancel/rerun never expose a private card's job (and
/// its transcript) to a tenant member who cannot see the card (MAIN-76). Returns
/// `NotFound` for both a missing job and an invisible target, so the two are
/// indistinguishable to the caller.
async fn load_visible(
    state: &AppState,
    tenant: TenantId,
    viewer: UserId,
    id: JobId,
) -> ApiResult<(LoopJob, TaskItem)> {
    let job = load(state, tenant, id).await?;
    let target = load_target(state, tenant, job.target_task_id).await?;
    if !crate::services::tasks::visible_to(&target, viewer) {
        return Err(ApiError::NotFound);
    }
    Ok((job, target))
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
    let target = load_target(state, tenant, req.target_task_id).await?;
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

    record_job_event(state, tenant, "job.created", &job, is_private(&target)).await;
    detail(state, job).await
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

    // Privacy of the target card gates the notification (not the activity
    // event) — a private card's state changes must not ring the tenant-wide
    // bell. A vanished target is treated as private (fail closed).
    let private = load_target(state, tenant, updated.target_task_id)
        .await
        .map(|t| is_private(&t))
        .unwrap_or(true);
    record_job_event(state, tenant, "job.state_changed", &updated, private).await;
    Ok(updated)
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

    record_job_event(state, tenant, "job.created", &job, is_private(&target)).await;
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

/// Is the target card private (creator + assignee only)?
fn is_private(target: &TaskItem) -> bool {
    target.visibility == "private"
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
    let person: Option<Uuid> = sqlx::query_scalar("SELECT person_id FROM users WHERE id = $1")
        .bind(job.requested_by)
        .fetch_optional(&state.db)
        .await?;
    let Some(person) = person else {
        return set_queued_reason(state, job_id, "the requester has no person identity").await;
    };

    // The best eligible node: owned-and-online-and-authorized first, else the
    // online authorized shared operator. `@>` containment tests the operator
    // flag; the EXISTS scans the reported auth profiles for our runtime.
    let node: Option<NodeId> = sqlx::query_scalar(
        "SELECT id FROM nodes
         WHERE tenant_id = $1
           AND status = 'online'
           AND (owner_person_id = $2 OR capabilities @> '{\"shared_operator\":true}')
           AND EXISTS (
                 SELECT 1
                 FROM jsonb_array_elements(COALESCE(capabilities->'runtime_auth', '[]'::jsonb)) e
                 WHERE e->>'runtime' = $3 AND e->>'state' = 'authorized'
               )
         ORDER BY (owner_person_id = $2) DESC NULLS LAST, id
         LIMIT 1",
    )
    .bind(tenant)
    .bind(person)
    .bind(LOOP_RUNTIME)
    .fetch_optional(&state.db)
    .await?;

    let Some(node) = node else {
        let reason = no_executor_reason(state, tenant, person).await?;
        return set_queued_reason(state, job_id, &reason).await;
    };

    // Atomic claim: only the caller that flips `queued` -> `claimed` wins.
    let claimed: Option<LoopJob> = sqlx::query_as(
        "UPDATE loop_jobs
            SET executor_node_id = $2, state = 'claimed', queued_reason = NULL, updated_at = now()
          WHERE id = $1 AND state = 'queued'
          RETURNING *",
    )
    .bind(job_id)
    .bind(node)
    .fetch_optional(&state.db)
    .await?;

    match claimed {
        Some(job) => {
            let private = load_target(state, tenant, job.target_task_id)
                .await
                .map(|t| is_private(&t))
                .unwrap_or(true);
            record_job_event(state, tenant, "job.state_changed", &job, private).await;
            Ok(job)
        }
        // Lost the race — another consumer claimed it. Return the current row.
        None => load(state, tenant, job_id).await,
    }
}

/// Phrase the specific gate that blocked placement (AC-3): distinguishes "no
/// node of yours is online" from "your online nodes aren't authorized" from "no
/// operator available", so the UI can tell the PM what to do.
async fn no_executor_reason(state: &AppState, tenant: TenantId, person: Uuid) -> ApiResult<String> {
    let owned_online: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM nodes
         WHERE tenant_id = $1 AND owner_person_id = $2 AND status = 'online'",
    )
    .bind(tenant)
    .bind(person)
    .fetch_one(&state.db)
    .await?;
    let operator_online: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM nodes
         WHERE tenant_id = $1 AND status = 'online'
           AND capabilities @> '{\"shared_operator\":true}'",
    )
    .bind(tenant)
    .fetch_one(&state.db)
    .await?;

    Ok(match (owned_online, operator_online) {
        (0, 0) => "no eligible executor: you have no node online and no shared operator is available".into(),
        (0, _) => format!(
            "no eligible executor: you have no node online, and the shared operator is not authorized for the {LOOP_RUNTIME} runtime"
        ),
        (_, 0) => format!(
            "no eligible executor: your online node(s) are not authorized for the {LOOP_RUNTIME} runtime, and no shared operator is available"
        ),
        _ => format!(
            "no eligible executor: no online node (yours or the shared operator) is authorized for the {LOOP_RUNTIME} runtime"
        ),
    })
}

/// Record why a job stays queued, without changing its state. A no-op guard on
/// `state = 'queued'` so a concurrent claim is never clobbered by a stale
/// reason write.
async fn set_queued_reason(state: &AppState, job_id: JobId, reason: &str) -> ApiResult<LoopJob> {
    sqlx::query("UPDATE loop_jobs SET queued_reason = $2, updated_at = now() WHERE id = $1 AND state = 'queued'")
        .bind(job_id)
        .bind(reason)
        .execute(&state.db)
        .await?;
    // Return the current row (its state is still queued unless a claim raced in).
    sqlx::query_as("SELECT * FROM loop_jobs WHERE id = $1")
        .bind(job_id)
        .fetch_one(&state.db)
        .await
        .map_err(Into::into)
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
/// at. Empty string if the row has vanished (the caller fails the job).
async fn task_key(state: &AppState, tenant: TenantId, task_id: TaskId) -> ApiResult<String> {
    let key: Option<String> = sqlx::query_scalar(
        "SELECT b.key || '-' || t.number
         FROM tasks t JOIN boards b ON b.id = t.board_id
         WHERE t.id = $1 AND t.tenant_id = $2",
    )
    .bind(task_id)
    .bind(tenant)
    .fetch_optional(&state.db)
    .await?;
    Ok(key.unwrap_or_default())
}

/// Resolve a job's workspace to a clonable git remote + branch, preferring the
/// executor node's own `node_workspaces` row and falling back to any node's row
/// for that workspace. `None` when no row carries a usable remote — the node
/// cannot derive it from a `workspace_id` alone.
pub async fn resolve_repo(
    state: &AppState,
    workspace_id: WorkspaceId,
    node: NodeId,
) -> ApiResult<Option<(String, String)>> {
    let row: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT git_remote_url, git_branch FROM node_workspaces
         WHERE workspace_id = $1 AND node_id = $2",
    )
    .bind(workspace_id)
    .bind(node)
    .fetch_optional(&state.db)
    .await?;
    let row = match row {
        Some(r @ (Some(_), _)) => Some(r),
        // The executor has no usable row — take any node's remote for the ws.
        _ => {
            sqlx::query_as(
                "SELECT git_remote_url, git_branch FROM node_workspaces
             WHERE workspace_id = $1 AND git_remote_url IS NOT NULL
             LIMIT 1",
            )
            .bind(workspace_id)
            .fetch_optional(&state.db)
            .await?
        }
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
    let Some((repo_url, branch)) = resolve_repo(state, workspace_id, node).await? else {
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
            job_id: job.id.0.to_string(),
            kind: job.kind.clone(),
            target_task_key,
            repo_url,
            branch,
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
    Ok(())
}

/// Fail every job a node was executing when it disconnected (AC-4): the session
/// died with the node. Terminal jobs are untouched (the guard in `transition`).
pub async fn fail_stranded_for_node(
    state: &AppState,
    tenant: TenantId,
    node: NodeId,
) -> ApiResult<()> {
    let stranded: Vec<JobId> = sqlx::query_scalar(
        "SELECT id FROM loop_jobs
         WHERE executor_node_id = $1 AND state IN ('claimed', 'running')",
    )
    .bind(node)
    .fetch_all(&state.db)
    .await?;
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
    let reaped: Vec<(JobId, TenantId, TaskId, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "UPDATE loop_jobs j
            SET state = 'failed', updated_at = now()
           FROM nodes n
          WHERE j.executor_node_id = n.id
            AND j.state IN ('claimed', 'running')
            AND n.last_seen_at IS NOT NULL
            AND n.last_seen_at < now() - ($1::bigint * interval '1 second')
        RETURNING j.id, j.tenant_id, j.target_task_id, n.last_seen_at",
    )
    .bind(grace_secs as i64)
    .fetch_all(&state.db)
    .await?;

    for (id, tenant, target, last_seen) in &reaped {
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
            let private = load_target(state, *tenant, *target)
                .await
                .map(|t| is_private(&t))
                .unwrap_or(true);
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
    let exec: Option<Option<NodeId>> = sqlx::query_scalar(
        "SELECT executor_node_id FROM loop_jobs WHERE id = $1 AND tenant_id = $2",
    )
    .bind(id)
    .bind(tenant)
    .fetch_optional(&state.db)
    .await?;
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
