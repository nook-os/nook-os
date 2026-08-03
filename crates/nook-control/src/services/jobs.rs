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
            "unknown job kind {:?} — expected one of spec, decompose",
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
            target_task_id: target_id,
            workspace_id: target.workspace_id,
            requested_by,
            seed: seed.clone(),
            predecessor_job_id: None,
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
    let private = load_target(state, tenant, updated.target_task_id)
        .await
        .map(|t| is_private(&t))
        .unwrap_or(true);
    record_job_event(state, tenant, "job.state_changed", &updated, private).await;
    // Nudge the ticket's live Loop panel that the job changed (MAIN-128 AC-2).
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

    record_job_event(state, tenant, "job.created", &job, is_private(&target)).await;
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

    // Nudge the ticket's live Loop panel that a new transcript line landed
    // (MAIN-128 AC-2 — the run "streams" as narration arrives). Best-effort: a
    // missing job row just means no live nudge, never a failed append.
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

/// Capacity assumed for a node that reports none — an agent old enough to
/// predate `max_loop_jobs` (MAIN-142). The shipped default rather than
/// unlimited: an unreported cap should behave like the configuration everything
/// else ships with, not like permission to take every job in the queue.
pub const CAPACITY_WHEN_UNREPORTED: u32 = 2;

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
/// at. Empty string if the row has vanished (the caller fails the job).
async fn task_key(state: &AppState, tenant: TenantId, task_id: TaskId) -> ApiResult<String> {
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
            job_id: job.id.0.to_string(),
            kind: job.kind.clone(),
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
