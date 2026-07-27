//! Durable human interactions (MAIN-159): an explicit ask, persisted with
//! authorization, announced over the notification channels (transport only),
//! answerable from any surface, resumable.
//!
//! The control plane is the source of truth. Channels carry prompt/choices/link;
//! they never carry the authority to answer — that follows the subject's
//! visibility and is checked here. Subject-generic by design: an interaction may
//! name a loop job and/or a ticket, so the loop-jobs chain (MAIN-127) is the
//! first consumer, not the only one.
//!
//! This slice deliberately does NOT touch job lifecycle (NG-3): raising an
//! interaction neither moves the job to `waiting_on_human` nor writes its
//! transcript — the bridging ticket (MAIN-162) wires those together.

use nook_types::*;
use serde_json::json;
use uuid::Uuid;

use crate::auth::{AuthCtx, Principal};
use crate::error::{ApiError, ApiResult};
use crate::events::{self, EventDraft};
use crate::state::AppState;

async fn load(state: &AppState, tenant: TenantId, id: InteractionId) -> ApiResult<Interaction> {
    sqlx::query_as("SELECT * FROM interactions WHERE id = $1 AND tenant_id = $2")
        .bind(id)
        .bind(tenant)
        .fetch_optional(&state.db)
        .await?
        .ok_or(ApiError::NotFound)
}

async fn load_task(state: &AppState, tenant: TenantId, task_id: TaskId) -> ApiResult<TaskItem> {
    sqlx::query_as("SELECT * FROM tasks WHERE id = $1 AND tenant_id = $2")
        .bind(task_id)
        .bind(tenant)
        .fetch_optional(&state.db)
        .await?
        .ok_or(ApiError::NotFound)
}

async fn load_job(state: &AppState, tenant: TenantId, job_id: JobId) -> ApiResult<LoopJob> {
    sqlx::query_as("SELECT * FROM loop_jobs WHERE id = $1 AND tenant_id = $2")
        .bind(job_id)
        .bind(tenant)
        .fetch_optional(&state.db)
        .await?
        .ok_or(ApiError::NotFound)
}

/// Is the subject card private (creator + assignee only)?
fn is_private(task: &TaskItem) -> bool {
    task.visibility == "private"
}

/// May `viewer` see (and therefore answer) an interaction whose subject ticket is
/// `task_id`? A subject-less interaction is tenant-wide. A subject the viewer
/// cannot see is treated as invisible — the caller gets `NotFound`, never the
/// prompt (AC-3: refused without leaking).
async fn subject_visible(
    state: &AppState,
    tenant: TenantId,
    viewer: UserId,
    task_id: Option<TaskId>,
) -> ApiResult<bool> {
    match task_id {
        None => Ok(true),
        Some(t) => match load_task(state, tenant, t).await {
            Ok(task) => Ok(crate::services::tasks::visible_to(&task, viewer)),
            Err(ApiError::NotFound) => Ok(false),
            Err(e) => Err(e),
        },
    }
}

/// Create an interaction — the one creation path (`nook interactions ask`).
///
/// Executor-scoped anti-spoof (AC-4): when a `job_id` is named, a NODE caller
/// must be that job's executor, so a machine cannot pin a pause onto work it is
/// not running. The subject ticket is then the job's target. A direct `task_id`
/// (no job) must be visible to the caller. Everything else is a standalone ask.
pub async fn create(
    state: &AppState,
    tenant: TenantId,
    caller: &AuthCtx,
    req: CreateInteractionRequest,
) -> ApiResult<Interaction> {
    let prompt = req.prompt.trim();
    if prompt.is_empty() {
        return Err(ApiError::BadRequest("an interaction needs a prompt".into()));
    }

    // Resolve the subject and enforce the anti-spoof / visibility gates.
    let (job_id, task_id) = if let Some(job_id) = req.job_id {
        let job = load_job(state, tenant, job_id).await?;
        match caller.principal {
            // The anti-spoof heart (AC-4): the requesting node must own the run.
            Principal::Node(nid) if job.executor_node_id == Some(nid) => {}
            Principal::Node(_) => {
                return Err(ApiError::ForbiddenMsg(
                    "this node is not the executor of that job — it cannot pause it".into(),
                ))
            }
            // A person raising a job-scoped ask may do so only for a job whose
            // target they can see — same reach as any other view of the card.
            Principal::User => {
                if !crate::services::tasks::visible_to(
                    &load_task(state, tenant, job.target_task_id).await?,
                    caller.user_id,
                ) {
                    return Err(ApiError::NotFound);
                }
            }
        }
        (Some(job_id), Some(job.target_task_id))
    } else if let Some(t) = req.task_id {
        // Anchoring directly to a ticket: it must be visible to the caller.
        let task = load_task(state, tenant, t).await?;
        if !crate::services::tasks::visible_to(&task, caller.user_id) {
            return Err(ApiError::NotFound);
        }
        (None, Some(t))
    } else {
        (None, None)
    };

    // The requesting node is the anti-spoof anchor and the WS push target: the
    // job's executor when job-scoped, else the caller node itself.
    let node_id: Option<NodeId> = match (job_id, caller.principal) {
        (Some(_), _) => match load_job(state, tenant, job_id.unwrap()).await {
            Ok(job) => job.executor_node_id,
            Err(_) => None,
        },
        (None, Principal::Node(nid)) => Some(nid),
        (None, Principal::User) => None,
    };

    let id = InteractionId::new();
    let interaction: Interaction = sqlx::query_as(
        "INSERT INTO interactions
            (id, tenant_id, job_id, task_id, prompt, choices,
             requested_by_node_id, requested_by_session_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         RETURNING *",
    )
    .bind(id)
    .bind(tenant)
    .bind(job_id)
    .bind(task_id)
    .bind(prompt)
    .bind(req.choices.as_deref())
    .bind(node_id)
    .bind(req.session_id)
    .fetch_one(&state.db)
    .await?;

    // Private subject → the tenant-wide bell must not surface it (mirrors jobs);
    // the activity event still records. A subject-less ask is tenant business.
    let private = match task_id {
        Some(t) => load_task(state, tenant, t)
            .await
            .map(|t| is_private(&t))
            .unwrap_or(true),
        None => false,
    };
    announce(
        state,
        tenant,
        caller,
        "interaction.created",
        &interaction,
        private,
    )
    .await;
    Ok(interaction)
}

/// Read one interaction, refusing (as `NotFound`) a viewer who cannot see its
/// subject — a private card's prompt stays private.
pub async fn get(
    state: &AppState,
    tenant: TenantId,
    viewer: UserId,
    id: InteractionId,
) -> ApiResult<Interaction> {
    let interaction = load(state, tenant, id).await?;
    if !subject_visible(state, tenant, viewer, interaction.task_id).await? {
        return Err(ApiError::NotFound);
    }
    Ok(interaction)
}

/// Every pending interaction the viewer is allowed to see (AC-3), newest last.
/// Subject-less asks are tenant-wide; the rest are filtered by card visibility.
pub async fn list_pending(
    state: &AppState,
    tenant: TenantId,
    viewer: UserId,
) -> ApiResult<Vec<Interaction>> {
    let rows: Vec<Interaction> = sqlx::query_as(
        "SELECT * FROM interactions
         WHERE tenant_id = $1 AND state = 'pending'
         ORDER BY created_at",
    )
    .bind(tenant)
    .fetch_all(&state.db)
    .await?;

    let mut out = Vec::with_capacity(rows.len());
    for i in rows {
        if subject_visible(state, tenant, viewer, i.task_id).await? {
            out.push(i);
        }
    }
    Ok(out)
}

/// Answer a pending interaction — the single endpoint every surface calls.
///
/// First authorized answer wins (AC-3): the state flip is a conditional UPDATE
/// on `state = 'pending'`, so a race resolves to exactly one winner and the
/// losers get a clean "already answered" 409. A viewer who cannot see the
/// subject is refused as `NotFound` without the prompt ever reaching them. On
/// success the answer is pushed to the requesting node over its WS channel
/// (AC-4) and is also pullable via `get` (`ask --wait`).
pub async fn answer(
    state: &AppState,
    tenant: TenantId,
    viewer: UserId,
    id: InteractionId,
    response: String,
) -> ApiResult<Interaction> {
    // Load first for the visibility gate — but the prompt never leaves this
    // function on the refusal path.
    let existing = load(state, tenant, id).await?;
    if !subject_visible(state, tenant, viewer, existing.task_id).await? {
        return Err(ApiError::NotFound);
    }

    let updated: Option<Interaction> = sqlx::query_as(
        "UPDATE interactions
            SET state = 'answered', answered_by = $2, response = $3,
                answered_at = now(), updated_at = now()
          WHERE id = $1 AND state = 'pending'
          RETURNING *",
    )
    .bind(id)
    .bind(viewer)
    .bind(&response)
    .fetch_optional(&state.db)
    .await?;

    let Some(interaction) = updated else {
        // Lost the race, or it was canceled — either way it is no longer
        // pending. A distinct 409, no prompt leaked.
        return Err(ApiError::Conflict(
            "this interaction has already been answered".into(),
        ));
    };

    // Push the answer to the node that asked, so a waiting executor is unblocked
    // without polling (AC-4). Best-effort: `ask --wait` also pulls via `get`, so
    // an offline node still gets its answer when it reconnects and re-reads.
    if let Some(node) = interaction.requested_by_node_id {
        state.registry.send_to_node(
            node,
            nook_proto::ControlToNode::InteractionAnswer {
                request_id: interaction.id.0.to_string(),
                answer: response.clone(),
            },
        );
    }

    // `interaction.answered` is activity-only (not catalogued → no tenant-wide
    // bell); the live surfaces update off the `InteractionChanged` UiEvent.
    let private = match interaction.task_id {
        Some(t) => load_task(state, tenant, t)
            .await
            .map(|t| is_private(&t))
            .unwrap_or(true),
        None => false,
    };
    record(
        state,
        tenant,
        "user",
        viewer.0,
        "interaction.answered",
        &interaction,
        private,
    )
    .await;
    publish_changed(state, tenant, &interaction);
    Ok(interaction)
}

/// Cancel a pending interaction (e.g. the ask is moot). Idempotent-ish: a 409 if
/// it already reached a terminal state. Subject visibility gates it, same as
/// answering.
pub async fn cancel(
    state: &AppState,
    tenant: TenantId,
    viewer: UserId,
    id: InteractionId,
) -> ApiResult<Interaction> {
    let existing = load(state, tenant, id).await?;
    if !subject_visible(state, tenant, viewer, existing.task_id).await? {
        return Err(ApiError::NotFound);
    }
    let updated: Option<Interaction> = sqlx::query_as(
        "UPDATE interactions
            SET state = 'canceled', updated_at = now()
          WHERE id = $1 AND state = 'pending'
          RETURNING *",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?;
    let Some(interaction) = updated else {
        return Err(ApiError::Conflict(
            "this interaction is no longer pending".into(),
        ));
    };
    record(
        state,
        tenant,
        "user",
        viewer.0,
        "interaction.canceled",
        &interaction,
        false,
    )
    .await;
    publish_changed(state, tenant, &interaction);
    Ok(interaction)
}

/// Record the lifecycle event (activity + maybe a notification) AND fan the live
/// `InteractionChanged` signal to the UI. `announce` is the create-side entry so
/// the actor is derived from the caller; `record` is the shared inner path.
async fn announce(
    state: &AppState,
    tenant: TenantId,
    caller: &AuthCtx,
    kind: &'static str,
    interaction: &Interaction,
    target_private: bool,
) {
    let (actor_type, actor_id) = match caller.principal {
        Principal::User => ("user", caller.user_id.0),
        Principal::Node(nid) => ("node", nid.0),
    };
    record(
        state,
        tenant,
        actor_type,
        actor_id,
        kind,
        interaction,
        target_private,
    )
    .await;
    publish_changed(state, tenant, interaction);
}

/// Record one interaction event. `target_private` rides the payload so
/// `events::notable` can suppress the tenant-wide bell for a private subject
/// without a DB read — exactly the job-event contract.
async fn record(
    state: &AppState,
    tenant: TenantId,
    actor_type: &'static str,
    actor_id: Uuid,
    kind: &'static str,
    interaction: &Interaction,
    target_private: bool,
) {
    events::record(
        state,
        tenant,
        EventDraft::new(kind)
            .actor(actor_type, actor_id)
            .payload(json!({
                "interaction_id": interaction.id,
                "task_id": interaction.task_id,
                "job_id": interaction.job_id,
                "prompt": interaction.prompt,
                "choices": interaction.choices,
                "state": interaction.state,
                "target_private": target_private,
            })),
    )
    .await;
}

/// Nudge every live surface (the global pending badge, the per-ticket panel) to
/// refetch. Carries only the subject ticket id, mirroring `TaskChanged`'s
/// "what you have is stale" contract.
fn publish_changed(state: &AppState, tenant: TenantId, interaction: &Interaction) {
    state.registry.publish(
        tenant,
        nook_proto::UiEvent::InteractionChanged {
            task_id: interaction.task_id,
        },
    );
}
