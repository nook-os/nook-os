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
use crate::services::main_ci;
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
const KINDS: [&str; 4] = ["spec", "decompose", "epic-run", "build"];

/// The workspace-targeted job kind (MAIN-408). Matches the `loop_jobs_kind_check`
/// constraint added by migration 0040.
pub const REVIEW_KIND: &str = "review";

/// The ticket-targeted builder kind (MAIN-383). In the kind CHECK since
/// migration 0050; enqueue is manual here — triggers and convergence are the
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
pub fn is_terminal(state: &str) -> bool {
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
            "unknown job kind {:?} — expected spec, decompose, epic-run or build. A review \
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
    if req.kind == "epic-run" {
        if target.type_ != "epic" {
            return Err(ApiError::BadRequest(
                "an epic-run job's target must be an epic — it merges the epic's \
                 children, and a leaf task has none"
                    .into(),
            ));
        }
        // One pass per epic at a time (MAIN-144 AC-3). Refused WITH the running
        // job's id, so the caller can watch that one instead of retrying.
        if let Some(existing) = state.jobs.active_epic_run_for(tenant, target_id).await? {
            return Err(ApiError::Conflict(format!(
                "an epic-run for this epic is already in flight: job {existing}"
            )));
        }
    }
    if req.kind == BUILD_KIND {
        // One live build run per card (AC-4). The partial unique index added by
        // 0050 is the atomic backstop; this check is what turns the second
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
            review_forced: false,
            review_pr_number: None,
            review_head_sha: None,
            build_fingerprint: None,
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
    // A human forced this run at an already-verdicted head (MAIN-473).
    // Review items only; a build item ignores it.
    forced: bool,
) -> ApiResult<Option<LoopJob>> {
    let job = match state
        .jobs
        .create(crate::repo::jobs::NewLoopJob {
            id: JobId::new(),
            tenant,
            kind: kind.to_string(),
            // A build item targets its card (0050's dedupe index arbitrates);
            // a review item is about the workspace alone.
            target_task_id: item.target_task_id,
            workspace_id: Some(workspace),
            requested_by,
            seed: Some(item.label.clone()),
            predecessor_job_id: None,
            review_pr_number: (item.target_task_id.is_none()).then_some(item.key),
            review_head_sha: (item.target_task_id.is_none()).then(|| item.fingerprint.clone()),
            build_fingerprint: item
                .target_task_id
                .is_some()
                .then(|| item.fingerprint.clone()),
            review_forced: forced && item.target_task_id.is_none(),
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
            .post_verdict(
                &repo,
                pr.max(0) as u64,
                head,
                label,
                body,
                job.review_forced,
            )
            .await
            .map_err(|e| ApiError::BadRequest(format!("posting the verdict failed: {e}")))?;

        // The card mirror, CP-posted like the verdict itself (MAIN-477 AC-2):
        // the skill used to append this line by hand and a card that cycled
        // builder↔reviewer read as a wall of near-duplicates. Best-effort —
        // the posted verdict is the record of truth — but never silent.
        if let Err(e) = mirror_verdict_to_card(
            state,
            tenant,
            &forge,
            &repo,
            pr.max(0) as u64,
            head,
            &req.verdict,
        )
        .await
        {
            tracing::warn!(pr, error = %e, "verdict card mirror failed — the PR verdict stands");
        }
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

/// Converge BUILD runs for one workspace (MAIN-458): one live run per owed
/// card, raised only when the card's fingerprint is not what the last
/// OUTCOMED run recorded — `owed()`'s rule, unchanged, over a second source.
///
/// `only_task` is the manual trigger's focus: the same convergence, filtered
/// to one card, so "build this NOW" cannot bypass the dedupe or the claim.
///
/// AC-3's board mechanics happen HERE, not in the skill: a fresh pick is
/// CLAIMED (atomically — a lost claim skips the item, the same 409 a second
/// builder would have eaten) before its run is raised, which also moves the
/// card to In Progress.
pub async fn converge_builds(
    state: &AppState,
    tenant: TenantId,
    requested_by: UserId,
    workspace: WorkspaceId,
    only_task: Option<TaskId>,
) -> ApiResult<crate::services::run_reconcile::Converged> {
    let ws = state
        .workspaces
        .get(tenant, workspace)
        .await?
        .ok_or(ApiError::NotFound)?;
    let token = crate::services::workspace_gh_token(state, tenant, workspace).await;
    // The workspace's declared ceiling (MAIN-461, landed): unset means the
    // default of one, 0 is the workspace-level kill-switch.
    let ceiling = ws.build_max_replicas.unwrap_or(1).max(0) as usize;
    let rejected_heads: std::collections::HashMap<i64, String> = state
        .jobs
        .rejected_review_heads(tenant, workspace)
        .await?
        .into_iter()
        .collect();
    // MAIN-489 AC-5: the manual trigger overrules the loop's OWN escalation —
    // a card it labelled `blocked` after three runs concluded nothing — and
    // nothing else. A card a human blocked for their own reason stays blocked
    // however it is named, so the strike count is the whole permission.
    let unblock_task = match only_task {
        Some(t)
            if state.tasks.build_failures(t, tenant).await?
                >= crate::services::build_handback::MAX_STRIKES =>
        {
            Some(t)
        }
        _ => None,
    };
    let source = crate::services::work_source::BuildWork {
        tasks: state.tasks.as_ref(),
        tenant,
        viewer: requested_by,
        demand: &state.review_demand,
        token,
        rejected_heads,
        unblock_task,
    };
    use crate::services::work_source::WorkSource;
    let Some(mut items) = source.items(workspace, ws.git_remote_url.as_deref()).await else {
        // UNKNOWN, never "no work" — hold rather than conclude on a guess.
        return Ok(Default::default());
    };
    if let Some(t) = only_task {
        items.retain(|i| i.target_task_id == Some(t));
    }
    let heads = state.jobs.build_run_heads(tenant, workspace).await?;
    let (owed, withheld, live) =
        crate::services::run_reconcile::owed(&items, &heads, ceiling, chrono::Utc::now());

    let mut jobs = Vec::new();
    for item in owed {
        let mut claimed: Option<TaskId> = None;
        if item.claim_first {
            let Some(task) = item.target_task_id else {
                continue;
            };
            // The claim is the atomic lock the skill used to take itself; a
            // loss is normal — another replica (or a human) got there first.
            if crate::routes::task_query::claim_inner(
                state,
                tenant,
                requested_by,
                &task.0.to_string(),
                Some("started".into()),
            )
            .await
            .is_err()
            {
                continue;
            }
            // AC-5 (MAIN-489): a card can only reach a claim carrying a FULL
            // set of strikes through a human's hand — the `blocked` label they
            // lifted, or the card they named to the manual trigger below. Both
            // nudges therefore spend the strikes, and this is the one place
            // that has to know it.
            if let Err(e) = state
                .tasks
                .clear_build_failures(task, tenant, crate::services::build_handback::MAX_STRIKES)
                .await
            {
                // Never fatal after a successful claim: returning here would
                // leave the card held for a run this pass then never raises,
                // which is the wedge this whole mechanism exists to prevent.
                tracing::warn!(%workspace, error = %e, "could not spend a nudged card's strikes");
            }
            claimed = Some(task);
        }
        let raised = raise_run(
            state,
            tenant,
            requested_by,
            workspace,
            BUILD_KIND,
            item,
            None,
            false,
        )
        .await;
        match raised {
            Ok(Some(job)) => jobs.push(job),
            other => {
                if let Err(e) = &other {
                    tracing::warn!(%workspace, item = %item.label, error = %e, "could not raise build run");
                }
                // COMPENSATE the claim: a card claimed for a run that never
                // materialized (a lost index race, a failed insert) would
                // otherwise sit assigned forever — out of every future pick
                // with nothing working it — and, released but left in the
                // started column, would read as unheld work in progress.
                if let Some(task) = claimed {
                    if let Err(e) = give_card_back(state, tenant, task, None).await {
                        tracing::warn!(%workspace, error = ?e, "could not give a compensated card back");
                    }
                }
            }
        }
    }
    Ok(crate::services::run_reconcile::Converged {
        raised: jobs.len(),
        jobs,
        withheld,
        live,
    })
}

/// Give a card the loop holds back to the board: release the claim, and return
/// it to the unstarted column.
///
/// One definition, because the callers must not drift. The claim and the
/// column are a pair: released but left in the started column reads as unheld
/// work in progress, and moved back while still assigned is out of every
/// future pick with nothing working it. Used to compensate a run that never
/// materialized, and (MAIN-482 AC-6, generalised by MAIN-489) to undo the claim
/// of any run that ended without an outcome — those never reach the outcome
/// handler, which is otherwise the only thing that releases a claim.
///
/// `held_by` is the fence, and it is why there is one function rather than two.
/// `None` releases whatever holds the card — right for compensating a claim we
/// took moments ago in the same call. `Some(user)` releases only while that
/// user still holds it UNDER A LEASE, which is what stops a run's death
/// undoing a human's own claim (MAIN-489 AC-7): a card somebody took over has
/// another assignee, and one dragged into progress by hand has no lease. It
/// answers `false` and moves nothing when the card was not ours to give back.
///
/// A failed release is logged and the move still attempted, which is the
/// compensation path's own behaviour: the two writes fail independently, and
/// abandoning the column move because the release failed would leave the card
/// in the started column as well as assigned — strictly worse than half of the
/// pair landing. A `held_by` that does not match is the other case entirely:
/// nothing was released, so there is no half to complete.
pub(crate) async fn give_card_back(
    state: &AppState,
    tenant: TenantId,
    task: TaskId,
    held_by: Option<UserId>,
) -> ApiResult<bool> {
    match held_by {
        Some(holder) => {
            if !state.tasks.release_claim_of(task, tenant, holder).await? {
                return Ok(false);
            }
        }
        None => {
            if let Err(e) = state.tasks.release_assignment(task, tenant).await {
                tracing::warn!(task = %task.0, error = %e, "could not release a claim");
            }
        }
    }
    let row = state
        .tasks
        .get_row(tenant, task)
        .await?
        .ok_or(ApiError::NotFound)?;
    let todo =
        crate::services::tasks::column_of_type(state.tasks.as_ref(), row.board_id, "unstarted")
            .await?;
    state
        .tasks
        .update_fields(
            tenant,
            task,
            crate::repo::tasks::TaskEdit {
                title: None,
                description: None,
                column_id: Some(todo.0),
                position: None,
                assignee_user_id: None,
                priority: None,
                set_workspace: false,
                workspace_id: None,
                expected_updated_at: None,
                type_: None,
                visibility: None,
                set_parent: false,
                parent_task_id: None,
            },
        )
        .await?;
    Ok(true)
}

/// The `Closes KEY` line — the reviewer's only join from a PR to its contract,
/// parsed by the same literal rule it teaches.
///
/// `pub(crate)` because the merge sweep (MAIN-491) joins by exactly this line:
/// a second parser that disagreed about what counts as a key would move the
/// wrong card, or none.
pub(crate) fn closes_key(body: &str) -> Option<String> {
    body.lines().find_map(|l| {
        let token = l
            .trim()
            .strip_prefix("Closes ")?
            .split_whitespace()
            .next()?;
        let (prefix, num) = token.rsplit_once('-')?;
        (!prefix.is_empty() && !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()))
            .then(|| token.to_string())
    })
}

/// AC-3's join check (MAIN-459): a `pr_opened` outcome must name a PR of THIS
/// workspace's repository whose body closes THIS run's card.
///
/// Validated as far as the deployment can see. The URL shape and repository
/// are always checkable for a GitHub workspace and refuse loudly on mismatch.
/// The body's `Closes` line needs a readable forge: no credential, or a forge
/// that fails to answer, SKIPS that half with a warning rather than refusing —
/// availability must not gate recording, or an outage strands every finished
/// run un-recordable. A workspace whose remote is not GitHub validates nothing.
async fn validate_pr_join(
    state: &AppState,
    tenant: TenantId,
    task: TaskId,
    workspace: nook_types::WorkspaceId,
    url: &str,
) -> ApiResult<()> {
    let Some(ws) = state.workspaces.get(tenant, workspace).await? else {
        return Ok(());
    };
    let Some(repo) = ws
        .git_remote_url
        .as_deref()
        .and_then(crate::services::forge::github_repo)
    else {
        return Ok(());
    };
    // `github_repo` lowercases (it rides `normalize_remote`), while `gh pr
    // create` prints GitHub's canonical casing — so the host/owner/name half
    // is compared case-insensitively, or a repo named `Acme/API` would refuse
    // every correct outcome. The path literal and the number are exact.
    let expected = format!("https://github.com/{}/{}/pull/", repo.owner, repo.name);
    let head = url.get(..expected.len()).unwrap_or_default();
    let number: u64 = (head.to_lowercase() == expected)
        .then(|| url[expected.len()..].trim_end_matches('/').parse().ok())
        .flatten()
        .ok_or_else(|| {
            ApiError::BadRequest(format!(
                "the url is not a pull request of this workspace's repository \
                 (expected {expected}<number>)"
            ))
        })?;

    // The workspace's own identity first (MAIN-456); otherwise the
    // deployment's forge — the SAME instance the review demand holds, which is
    // also what lets a test point this check at a fake. Neither present:
    // nothing can read the body, so the Closes half is skipped.
    let own;
    let forge: &dyn crate::services::forge::Forge =
        match crate::services::workspace_gh_token(state, tenant, workspace).await {
            Some(t) => {
                own = crate::services::forge::GithubForge::from_token(&t);
                &own
            }
            None => match state.review_demand.forge() {
                Some(f) => f,
                None => return Ok(()),
            },
        };
    let body = match forge.pr_details(&repo, number).await {
        Ok(d) => d.body,
        Err(e) => {
            tracing::warn!(
                %workspace, pr = number, error = %e,
                "could not read the PR body — recording the outcome without the Closes check"
            );
            return Ok(());
        }
    };
    let Some(key) = closes_key(&body) else {
        return Err(ApiError::BadRequest(
            "the PR body has no `Closes <KEY>` line — the reviewer cannot join it to its \
             contract; add one and report the outcome again"
                .into(),
        ));
    };
    match crate::services::tasks::resolve_id(state.tasks.as_ref(), tenant, &key).await {
        Ok(resolved) if resolved == task => Ok(()),
        Ok(_) => Err(ApiError::BadRequest(format!(
            "the PR closes {key}, which is not this run's card"
        ))),
        Err(_) => Err(ApiError::BadRequest(format!(
            "the PR closes {key}, which resolves to no card on this board"
        ))),
    }
}

/// Record what a build run concluded, and mirror it to the board (MAIN-458
/// AC-2/AC-3). The card's move is code's job now; the agent only concludes.
pub async fn record_build_outcome(
    state: &AppState,
    tenant: TenantId,
    job_id: JobId,
    req: &nook_types::BuildOutcomeRequest,
) -> ApiResult<LoopJob> {
    let job = state
        .jobs
        .get(tenant, job_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let (Some(task), Some(ws)) = (job.target_task_id, job.workspace_id) else {
        return Err(ApiError::BadRequest(
            "only a directed build run records an outcome".into(),
        ));
    };
    if job.kind != BUILD_KIND {
        return Err(ApiError::BadRequest(
            "only a build run records a build outcome".into(),
        ));
    }
    // Validate the SHAPE before recording anything, so a malformed call
    // changes nothing anywhere — and carry the validated values as a type,
    // so the arms below cannot drift apart from this check.
    enum Concluded<'a> {
        PrOpened(&'a str),
        Blocked(&'a str),
        Nothing,
    }
    let url = req.url.as_deref().map(str::trim).filter(|u| !u.is_empty());
    let question = req
        .question
        .as_deref()
        .map(str::trim)
        .filter(|q| !q.is_empty());
    let concluded = match req.outcome.as_str() {
        "pr_opened" => {
            let url = url.ok_or_else(|| ApiError::BadRequest("pr_opened needs a url".into()))?;
            // AC-3 (MAIN-459): validate the join BEFORE recording, so a PR
            // that names another repository or another card changes nothing
            // anywhere — the run sees the refusal and can fix its PR.
            validate_pr_join(state, tenant, task, ws, url).await?;
            Concluded::PrOpened(url)
        }
        "blocked" => Concluded::Blocked(
            question.ok_or_else(|| ApiError::BadRequest("blocked needs a question".into()))?,
        ),
        "nothing_to_do" => Concluded::Nothing,
        other => {
            return Err(ApiError::BadRequest(format!(
                "outcome must be one of pr_opened|blocked|nothing_to_do, got {other:?}"
            )));
        }
    };

    // Record FIRST, exactly once: the guarded UPDATE (live run, no outcome
    // yet) is the idempotence gate, so a retried delivery — an agent timing
    // out on a call that landed — cannot post a second comment, re-label, or
    // re-release. The board writes below happen only on the one recording.
    if state.jobs.set_build_outcome(job_id, &req.outcome).await? == 0 {
        return Err(ApiError::Conflict(
            "this run is not live, or its outcome is already recorded".into(),
        ));
    }

    // A REPAIR run's card was never claimed by the loop and may be held by a
    // human — the release below is only ever undoing the loop's own claim.
    let is_repair = job
        .build_fingerprint
        .as_deref()
        .is_some_and(|f| f.starts_with("repair:"));

    // The outcome above is recorded whatever happens next; a board write
    // that fails after it must therefore be LOUD — the run has consumed the
    // card, and a silent half-mirror would self-perpetuate (the repair
    // source reads `pr_url` off the card). The transcript carries the manual
    // fix an operator needs.
    let board = async {
        // AC-6 (MAIN-489): this run concluded something, so the card's run of
        // runs that concluded nothing is over. Three failures spread across
        // successful builds must never add up to an escalation.
        if !is_repair {
            state.tasks.clear_build_failures(task, tenant, 0).await?;
        }
        match concluded {
            Concluded::PrOpened(url) => {
                // The reviewer's ONLY join from a PR to its contract is the
                // card — record the PR on it and park it where a human
                // reviews (AC-3).
                let row = state
                    .tasks
                    .get_row(tenant, task)
                    .await?
                    .ok_or(ApiError::NotFound)?;
                let review_col = crate::services::tasks::column_of_type(
                    state.tasks.as_ref(),
                    row.board_id,
                    "review",
                )
                .await?;
                state.tasks.set_pr_url(task, url, review_col).await?;
            }
            Concluded::Blocked(question) => {
                // Question first, then the label, then the release — the
                // order a human reads: the card explains itself before it
                // reappears.
                state
                    .tasks
                    .create_comment(crate::repo::tasks::NewComment {
                        tenant,
                        task,
                        author_type: "system".into(),
                        author_id: None,
                        author_name: "nook-build loop".into(),
                        body_md: question.to_string(),
                    })
                    .await?;
                state.tasks.attach_label(tenant, task, "blocked").await?;
                if !is_repair {
                    state.tasks.release_assignment(task, tenant).await?;
                }
            }
            Concluded::Nothing => {
                if !is_repair {
                    state.tasks.release_assignment(task, tenant).await?;
                }
            }
        }
        Ok::<(), crate::error::ApiError>(())
    }
    .await;
    if let Err(e) = board {
        tracing::error!(
            job = %job_id, task = %task.0, error = ?e,
            "build outcome recorded but the board write failed — fix the card by hand"
        );
        append_transcript(
            state,
            job_id,
            "system",
            &format!(
                "outcome {} recorded, but mirroring it to the board FAILED ({e:?}) — \
                 the card needs a hand: check its PR link, column and labels",
                req.outcome
            ),
        )
        .await
        .ok();
    }

    append_transcript(
        state,
        job_id,
        "system",
        &format!("outcome: {}", req.outcome),
    )
    .await
    .ok();
    state
        .registry
        .publish(tenant, nook_proto::UiEvent::TaskChanged { task_id: task });
    state.jobs.reload(job_id).await
}

/// Is this exact conclusion already on the card? Same head AND same verdict
/// wording — a redelivery would change nothing but a timestamp, which is not
/// information. A new head or a changed verdict is a real event and appends.
fn mirror_is_duplicate(existing_bodies: &[&str], head: &str, verdict_words: &str) -> bool {
    let marker = format!("Loop review of {head}");
    existing_bodies
        .iter()
        .any(|b| b.starts_with(&marker) && b.contains(verdict_words))
}

/// Mirror one verdict onto its board card, collapsed (MAIN-477 AC-2): the
/// card is found through the PR body's `Closes <KEY>` join, and a comment for
/// the SAME head and verdict is never appended twice — a redelivery changes
/// nothing but a timestamp, which is not information. A new head or a changed
/// verdict appends normally, preserving the card's history of real events.
async fn mirror_verdict_to_card(
    state: &AppState,
    tenant: TenantId,
    forge: &crate::services::forge::GithubForge,
    repo: &crate::services::forge::Repo,
    pr: u64,
    head: &str,
    verdict: &str,
) -> anyhow::Result<()> {
    use crate::services::forge::Forge as _;
    let body = forge.pr_details(repo, pr).await?.body;
    let Some(key) = closes_key(&body) else {
        // No join line: nothing to mirror to, and the reviewer escalates that
        // on the PR itself — not this mirror's job to repeat.
        return Ok(());
    };
    // A key that does not resolve — a typo in `Closes`, another board's key —
    // is exactly what an operator wants named in the log, so it propagates to
    // the caller's warn instead of being swallowed (the mirror stays
    // best-effort; the PR verdict already stands).
    let task = crate::services::tasks::resolve_id(state.tasks.as_ref(), tenant, &key)
        .await
        .map_err(|e| anyhow::anyhow!("Closes {key} does not resolve to a card: {e:?}"))?;
    let line = format!(
        "Loop review of {head} — {}: https://github.com/{}/{}/pull/{pr}",
        verdict.replace('_', " "),
        repo.owner,
        repo.name
    );
    let existing = state.tasks.comments_of(task).await?;
    let bodies: Vec<&str> = existing.iter().map(|c| c.body_md.as_str()).collect();
    if mirror_is_duplicate(&bodies, head, &verdict.replace('_', " ")) {
        return Ok(());
    }
    state
        .tasks
        .create_comment(crate::repo::tasks::NewComment {
            tenant,
            task,
            author_type: "system".into(),
            author_id: None,
            author_name: "nook-review loop".into(),
            body_md: line.clone(),
        })
        .await?;
    // The path this replaces went through the comment ROUTE, which feeds the
    // activity feed (and the notification bridge) and repaints the card live.
    // A mirror that skipped both would be a silent downgrade: same payload
    // rules as the route — a private card's event carries no excerpt and no
    // key, which is what keeps it out of feeds and channels.
    let meta = state.tasks.task_visibility_naming(task, tenant).await?;
    let mut payload = serde_json::json!({ "task_id": task, "author": "nook-review loop" });
    if let Some((visibility, number, board_key)) = meta {
        if visibility != "private" {
            payload["excerpt"] = serde_json::json!(line.chars().take(140).collect::<String>());
            if let (Some(k), Some(n)) = (board_key, number) {
                payload["key"] = serde_json::json!(format!("{k}-{n}"));
            }
        }
    }
    crate::events::record(
        state,
        tenant,
        crate::events::EventDraft::new("task.comment.created").payload(payload),
    )
    .await;
    state
        .registry
        .publish(tenant, nook_proto::UiEvent::TaskChanged { task_id: task });
    Ok(())
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
///
/// `force` (MAIN-473) overrules exactly one of those rules — the
/// verdicted-head rest, for one named PR — and nothing else: the live-run
/// dedupe and the workspace ceiling (including `0 = off`) refuse a forced
/// enqueue the same as any other.
pub async fn enqueue_review(
    state: &AppState,
    tenant: TenantId,
    requested_by: UserId,
    workspace: WorkspaceId,
    seed: Option<String>,
    pr: Option<i64>,
    force: bool,
) -> ApiResult<crate::services::run_reconcile::Converged> {
    use crate::services::work_source::WorkSource;

    if force && pr.is_none() {
        return Err(ApiError::BadRequest(
            "force needs a PR number — a blanket re-review of every verdicted head is \
             never what anyone means"
                .into(),
        ));
    }
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

    let Some(pr) = pr else {
        return crate::services::run_reconcile::converge(
            state,
            &source,
            tenant,
            requested_by,
            workspace,
            ws.git_remote_url.as_deref(),
            ceiling,
            seed.as_deref(),
        )
        .await;
    };

    // Directed at ONE pull request. The item comes from the same source the
    // reconciler reads — its current head is the fingerprint a run needs. An
    // unreadable forge is an OUTAGE the server is already retrying, never a
    // client error: 503, matching how `converge` holds on the same `None`.
    let Some(items) = source.items(workspace, ws.git_remote_url.as_deref()).await else {
        return Err(ApiError::ServiceUnavailable(
            "the forge cannot be read right now, so the PR's current head is unknown — \
             try again shortly"
                .into(),
        ));
    };
    let Some(item) = items.iter().find(|i| i.key == pr) else {
        return Err(ApiError::BadRequest(format!(
            "PR #{pr} is not an open pull request needing review in this workspace"
        )));
    };

    let heads = state.jobs.review_run_heads(tenant, workspace).await?;
    if !force {
        // The reconciler's own rule decides, unchanged — a verdicted head
        // rests, a hold holds, a live run blocks, the ceiling caps — and it
        // declines the same quiet way the blanket path does, so the presence
        // of `--pr` alone never changes how a condition is reported.
        let (owed, withheld, live) = crate::services::run_reconcile::owed(
            std::slice::from_ref(item),
            &heads,
            ceiling,
            chrono::Utc::now(),
        );
        if owed.is_empty() {
            return Ok(crate::services::run_reconcile::Converged {
                jobs: Vec::new(),
                raised: 0,
                withheld,
                live,
            });
        }
    } else {
        // Force overrules exactly ONE rule: the verdicted-head rest. Every
        // other control stands, refused most-specific first.
        //
        // One live run per PR (AC-3) — named by id, so the refusal points at
        // the thing to wait on; checked before the ceiling because "your PR's
        // own run is live" is the actionable answer when both are true.
        if heads
            .iter()
            .any(|h| h.item_key == pr && h.live_head.is_some())
        {
            let live_id = live_review_run_id(state, tenant, workspace, pr).await?;
            return Err(ApiError::Conflict(format!(
                "a review run is already live for PR #{pr}{} — wait for it or cancel it",
                live_id.map(|id| format!(" (job {id})")).unwrap_or_default()
            )));
        }
        // Then the workspace ceiling (`0 = off` is the workspace-level kill
        // switch, and a forced run past it would execute review work the owner
        // turned off): count live runs the way `owed()` does and refuse past
        // the cap rather than silently exceeding it.
        let live = heads.iter().filter(|h| h.live_head.is_some()).count();
        if ceiling == 0 {
            return Err(ApiError::Conflict(
                "reviews are off for this workspace (ceiling 0) — raise the ceiling to force one"
                    .into(),
            ));
        }
        if live >= ceiling {
            return Err(ApiError::Conflict(format!(
                "the review ceiling ({ceiling}) is full — {live} run(s) live; wait for one to \
                 finish or raise the ceiling"
            )));
        }
    }
    // Raised through the same path as every other run; the partial unique
    // index still arbitrates a race, so two forces cannot double-raise.
    let job = raise_run(
        state,
        tenant,
        requested_by,
        workspace,
        source.kind(),
        item,
        seed.as_deref(),
        force,
    )
    .await?;
    match job {
        Some(job) => Ok(crate::services::run_reconcile::Converged {
            raised: 1,
            jobs: vec![job],
            withheld: 0,
            live: 0,
        }),
        None => {
            let live_id = live_review_run_id(state, tenant, workspace, pr).await?;
            Err(ApiError::Conflict(format!(
                "a review run is already live for PR #{pr}{} — wait for it or cancel it",
                live_id.map(|id| format!(" (job {id})")).unwrap_or_default()
            )))
        }
    }
}

/// The id of the live review run for one PR, for the refusal that names it —
/// a targeted lookup, so the id is present however old the run is.
async fn live_review_run_id(
    state: &AppState,
    tenant: TenantId,
    workspace: WorkspaceId,
    pr: i64,
) -> ApiResult<Option<JobId>> {
    state.jobs.live_review_run_for(tenant, workspace, pr).await
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
    // MAIN-489: a build run that ends having recorded NO outcome still owes its
    // card an answer. Here rather than in `finish`, because every way a run can
    // reach terminal — a node's report, the executor reaper, a cancel — comes
    // through this one write path.
    if is_terminal(to) {
        crate::services::build_handback::on_run_concluded(state, tenant, &updated).await;
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
            build_fingerprint: None,
            review_forced: false,
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
/// Eligibility (AC-1): an ONLINE node that reports the loop runtime
/// `authorized` (MAIN-126), preferring one **owned by the requester** over the
/// **shared operator** (`shared_operator` in the node's capabilities). No one
/// else's machine is ever eligible. The owned leg reaches the requester's
/// machines in every tenant they belong to (MAIN-515); the shared operator is
/// its own tenant's alone.
///
/// **Every node lookup from here on is therefore unscoped by tenant.** A
/// candidate may be homed elsewhere, and a tenant-scoped `get` would silently
/// drop it at the next gate — which is exactly how the label filter below used
/// to turn a cross-tenant candidate back into "no eligible executor".
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
    select_executor_within(state, tenant, job_id, &mut main_ci::Pass::default()).await
}

/// [`select_executor`], sharing one dispatch pass's derived view of each
/// workspace's default-branch CI (MAIN-543) so a pass with ten queued builds
/// for one repo asks the forge once, not ten times.
async fn select_executor_within(
    state: &AppState,
    tenant: TenantId,
    job_id: JobId,
    pass: &mut main_ci::Pass,
) -> ApiResult<LoopJob> {
    let job = load(state, tenant, job_id).await?;
    if job.state != "queued" {
        return Ok(job); // already claimed/terminal — nothing to place.
    }

    // The FIRST gate, before any node is even considered (MAIN-543): a build
    // run raised against a repo whose own trunk is broken cannot pass, and
    // every one we place fails at the same error while the review loop
    // escalates cards for a failure their PRs did not cause. Nothing is stored
    // — the next pass re-derives, so a green trunk resumes dispatch by itself.
    if let Some(red) = red_default_branch_holding(state, tenant, &job, pass).await {
        return set_queued_reason(state, job_id, &main_ci::reason(&red)).await;
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

    // The worktree PIN (MAIN-480 AC-5). A build card's worktree outlives its
    // run and holds the state later passes depend on — a warm agent session
    // that remembers that directory, and, after a crash, the only copy of the
    // interrupted work. Running the next pass anywhere else abandons both, so
    // the recorded node is not a preference: it is the only candidate, and a
    // job waits rather than starting somewhere it would have to begin again.
    //
    // Waiting is bounded by a human, not by a timer: `prune-worktree` clears
    // the record (even against an unreachable node) and the job is placeable
    // again the moment it does.
    let pinned = pinned_node(state, tenant, &job).await?;
    let candidates: Vec<NodeId> = match pinned {
        Some(pin) => candidates.into_iter().filter(|n| *n == pin).collect(),
        None => candidates,
    };
    if let Some(pin) = pinned {
        if candidates.is_empty() {
            let name = state
                .nodes
                .by_id_any_tenant_or_none(pin)
                .await?
                .map(|n| n.name)
                .unwrap_or_else(|| pin.0.to_string());
            return set_queued_reason(
                state,
                job_id,
                &format!(
                    "waiting for node {name}, which holds this card's worktree — it is offline \
                     or not eligible right now. Prune the worktree from the card to release it."
                ),
            )
            .await;
        }
    }

    // A kind with a placement selector runs on labeled nodes and nowhere else.
    // Review keeps the declaration's own rule (MAIN-455 AC-4) and build filters
    // to `role=build` the same way (MAIN-383 AC-3) — each selector has ONE
    // definition, read here, rather than a second copy of the string.
    let had_candidates = !candidates.is_empty();
    let candidates = if let Some(selector) = placement_selector(&job.kind) {
        let mut kept = Vec::new();
        for node in candidates {
            // Unscoped: a candidate owned by the requester may be homed in
            // another of their tenants (MAIN-515), and `get(tenant, …)` returned
            // None for it — dropping the very node this job was placed on.
            let Some(row) = state.nodes.by_id_any_tenant_or_none(node).await? else {
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
    // `loop_jobs` count rather than a node fact — so it is applied here. A
    // cordon (MAIN-505) is read off the same row for the same reason: both are
    // "not right now" rather than "not ever", and both need naming in the
    // queued reason.
    let mut chosen: Option<NodeId> = None;
    let mut blocked_by_capacity = false;
    let mut cordoned: Vec<String> = Vec::new();
    for node in candidates {
        // The capacity IN FORCE, not merely what the node advertises (MAIN-508):
        // read from the stored row on every attempt, so an operator's new number
        // lands at the next poll without the node agent restarting — the restart
        // being the thing that strands every in-flight build.
        let row = state.nodes.by_id_any_tenant_or_none(node).await?;
        // A node draining before an agent restart takes nothing new (MAIN-505
        // AC-2). Checked before capacity because it is the stronger statement:
        // the machine has slots and is still refusing them.
        if let Some(c) = row.as_ref().and_then(node_cordon) {
            let name = row
                .as_ref()
                .map(|r| r.name.clone())
                .unwrap_or_else(|| node.0.to_string());
            cordoned.push(format!("{name} ({})", c.reason));
            continue;
        }
        let cap = match row {
            Some(row) => crate::services::loop_capacity::of(&row).effective,
            None => CAPACITY_WHEN_UNREPORTED,
        };
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
        let reason = if !cordoned.is_empty() {
            // Named rather than lumped in with "no eligible executor" (AC-3):
            // the machine IS eligible and IS online, it is draining, and this
            // says so in the node's own words — which is the whole question
            // "why did nothing get placed on azul" is asking. Capacity is
            // added rather than replaced, so a mixed fleet is described and
            // not summarised into a half-truth.
            let mut r = format!("no eligible executor: cordoned — {}", cordoned.join("; "));
            if blocked_by_capacity {
                r.push_str("; the rest are at their loop-job capacity");
            }
            r
        } else if blocked_by_capacity {
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

/// The failing default-branch run holding this job back, or `None` to carry on
/// (MAIN-543 AC-1).
///
/// BUILD work only, and that is NG-1 rather than an oversight: a red trunk is
/// not a reason to stop reviewing or merging — those are separate judgements —
/// and a spec or decompose run writes no code the trunk could break. A job with
/// no workspace has no trunk to read, so it dispatches.
async fn red_default_branch_holding(
    state: &AppState,
    tenant: TenantId,
    job: &LoopJob,
    pass: &mut main_ci::Pass,
) -> Option<crate::services::forge::CiRun> {
    if job.kind != BUILD_KIND {
        return None;
    }
    main_ci::red_default_branch(state, tenant, job.workspace_id?, pass).await
}

/// How many of a tenant's queued jobs one pass will try to place.
///
/// Not a fairness rule — the order already is one. It bounds the work a single
/// pass does on a board with a very long queue, and it can only ever skip jobs
/// that already have [`DISPATCH_PASS_LIMIT`] worthier ones ahead of them.
///
/// **The window advances only when the head of the order is SLOW, not when it
/// is UNPLACEABLE.** A job rises into the window as the ones above it are
/// placed — but if a tenant queues more than this many and the top of the order
/// is refused for a stable reason (a workspace at its build ceiling, a kind no
/// online node accepts), then the job below the window is never tried, even
/// with an executor standing free, and nothing lifts it. MAIN-496's starvation
/// cancel is what eventually clears such a head; this cap does not. Do not
/// raise it, or drop it, on the belief that the window always drains.
pub const DISPATCH_PASS_LIMIT: usize = 32;

/// Offer this tenant's free executor capacity to its queued jobs, worthiest
/// first, and return the jobs that were placed (MAIN-509).
///
/// This — not the arrival of a work item — is what decides WHICH queued job
/// gets a freed executor. The order comes from
/// [`crate::repo::jobs::LoopJobRepository::queued_in_dispatch_order`]: card
/// priority, then how long the job has waited. The pass runs down that order
/// and does not stop at the first refusal, because a job can be unplaceable for
/// a reason of its own — a worktree pin on a dark node, a label nothing wears —
/// and the jobs behind it must not inherit that wait. Each attempt re-reads the
/// node's in-flight count, so capacity still stops the pass exactly where it
/// should.
pub async fn place_queued_in_order(state: &AppState, tenant: TenantId) -> ApiResult<Vec<LoopJob>> {
    let queued = state.jobs.queued_in_dispatch_order(tenant).await?;
    let mut placed = Vec::new();
    // One memo for the whole pass, dropped with it: the default-branch signal
    // is derived per PASS, never cached across them (MAIN-543 AC-2).
    let mut pass = main_ci::Pass::default();
    for job_id in queued.into_iter().take(DISPATCH_PASS_LIMIT) {
        match select_executor_within(state, tenant, job_id, &mut pass).await {
            Ok(job) if job.state == "claimed" => placed.push(job),
            Ok(_) => {}
            // One job's transient failure is not the pass's: the jobs behind it
            // are still owed their turn at the free executor.
            Err(e) => {
                tracing::warn!(job = %job_id.0, error = %e, "executor selection failed")
            }
        }
    }
    Ok(placed)
}

/// The node a job is pinned to, if its card already has a worktree somewhere
/// (MAIN-480 AC-5).
///
/// Only BUILD work is pinned. A review or spec run carries no state across
/// passes, and pinning one would strand it on a machine for no gain; a card's
/// `worktree_node_id` may also have been set by the human `start-work` path,
/// which those kinds have no business honouring.
async fn pinned_node(
    state: &AppState,
    tenant: TenantId,
    job: &LoopJob,
) -> ApiResult<Option<NodeId>> {
    if job.kind != BUILD_KIND {
        return Ok(None);
    }
    let Some(task) = job.target_task_id else {
        return Ok(None);
    };
    let Some(card) = state.tasks.get_row(tenant, task).await? else {
        return Ok(None);
    };
    Ok(
        match (card.worktree_path.as_deref(), card.worktree_node_id) {
            (Some(_), Some(node)) => Some(node),
            _ => None,
        },
    )
}

/// The cordon a node is reporting, if any (MAIN-505).
///
/// Stored as JSON like every other node blob, so this is the one place that
/// knows how to read it back. A row written by a newer node than this control
/// plane understands reads as "not cordoned" rather than failing placement —
/// withholding every job over an unparseable field would be a worse outcome
/// than placing one on a draining machine.
fn node_cordon(node: &nook_types::Node) -> Option<nook_types::NodeCordon> {
    serde_json::from_value(node.cordon.clone()?).ok()
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
///
/// "Yours" means yours ANYWHERE (MAIN-515), because that is what eligibility
/// now means: an owned node in another of your tenants is a candidate, so
/// counting only this tenant's would report "you have no node online" about a
/// machine that is online and was rejected for some other gate entirely. The
/// one thing tenancy still refuses is a shared operator elsewhere, and that
/// gets its own sentence rather than the generic fall-through — the original
/// report was of an owner hunting capacity that was never the problem.
async fn no_executor_reason(
    state: &AppState,
    tenant: TenantId,
    person: Uuid,
    kind: &str,
) -> ApiResult<String> {
    let owned_here: i64 = state.nodes.owned_online_count(tenant, person).await?;
    let (owned_elsewhere, operators_elsewhere) =
        state.nodes.owned_online_elsewhere(tenant, person).await?;
    let operator_online: i64 = state.nodes.shared_operator_online_count(tenant).await?;
    let owned_online = owned_here + owned_elsewhere;

    // Gated on the person's own machines alone, NOT on whether this tenant also
    // has an operator: with an in-tenant operator that is merely ineligible,
    // the `(0, _)` arm below would say "you have no node online" — the one
    // sentence AC-6 exists to prevent, and it would be said in exactly the
    // state the bug was reported from. The operator's own state is added to
    // the sentence instead of replacing it, because both facts are true and
    // only one of them is surprising.
    if owned_online == 0 && operators_elsewhere > 0 {
        let here = if operator_online == 0 {
            "and this tenant has none of its own".to_string()
        } else {
            format!(
                "and this tenant's own is not authorized for the {LOOP_RUNTIME} runtime or does \
                 not accept {kind} jobs"
            )
        };
        return Ok(format!(
            "no eligible executor: your only online node(s) are shared operators in another of \
             your tenants, and a shared operator serves only the tenant it was joined to — \
             {here}. Join a node to this tenant, or raise the {kind} job in the tenant that has \
             the operator"
        ));
    }

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
            // The deployment's advertised HTTP API (MAIN-465) — NOOK_PUBLIC_API_URL,
            // a dedicated setting. Deliberately NOT agent_public_url (the mTLS
            // agent listener, which resolves somewhere different on purpose)
            // and NOT public_base_url (the browser URL, wrong from inside a
            // container on a dev stack).
            server_url: state.cfg.public_api_url.clone(),
            // The agent's own identity, in the JOB's tenant, as the person who
            // asked for the run. Revoked in `finish`.
            nook_token: mint_job_token(state, tenant, job.requested_by, job.id).await,
            job_id: job.id.0.to_string(),
            kind: job.kind.clone(),
            // Which PR this run owns, so the agent is told rather than having to
            // find its share, and so it can resume that PR's session.
            review_pr_number: job.review_pr_number.map(|n| n.max(0) as u64),
            review_forced: job.review_forced,
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

/// Record a run the node REFUSED to launch, and give its card back (MAIN-482
/// AC-6).
///
/// A refusal terminates a run before its agent ever started, so nothing will
/// report an outcome for it — and [`record_build_outcome`] is the only thing
/// that releases the loop's claim. Left to `finish` alone, a refused build
/// would sit claimed-in-progress in the started column with nothing running,
/// which is exactly the dishonest board the guards exist to prevent. So the
/// refusal itself explains the card, releases the claim and returns it to the
/// unstarted column, leaving it pickable by the next converge pass.
///
/// A REPAIR run's card was never claimed by the loop and may be held by a
/// human — its comment lands but the claim is left alone, mirroring the
/// outcome handler's rule.
///
/// **The giving back is the TRANSITION's** since MAIN-489: `failed` reaches
/// [`crate::services::build_handback::on_run_concluded`], which releases the
/// claim, returns the card and counts the attempt. Doing it here as well would
/// clear the assignee before that guarded release could recognise the card as
/// ours — and a refusal that never counted would be the one failure mode
/// exempt from the three-strike stop, which is the class most likely to repeat
/// forever (a node misconfigured the same way on every pass).
pub async fn refuse(state: &AppState, tenant: TenantId, id: JobId, reason: &str) -> ApiResult<()> {
    let reason = reason.trim();
    let job = load(state, tenant, id).await?;
    append_transcript(state, id, "system", reason).await.ok();

    if let Some(task) = job.target_task_id {
        // The comment first, then the transition — the order a human reads: the
        // card explains why it came back before it reappears in the queue.
        if let Err(e) = state
            .tasks
            .create_comment(crate::repo::tasks::NewComment {
                tenant,
                task,
                author_type: "system".into(),
                author_id: None,
                author_name: "nook-build loop".into(),
                body_md: reason.to_string(),
            })
            .await
        {
            tracing::warn!(job = %id, task = %task.0, error = ?e, "could not comment a refusal");
        }
        // The handback nudges the UI for a card it gives back; a REPAIR run's
        // card it deliberately leaves alone, and the comment above still has
        // to reach the surfaces watching it.
        state
            .registry
            .publish(tenant, nook_proto::UiEvent::TaskChanged { task_id: task });
    }

    let _ = transition(state, tenant, id, "failed").await;
    revoke_job_token(state, tenant, id).await;
    Ok(())
}

/// Apply a node's `JobRefused` — ONLY for a job that node is actually
/// executing, so a node token cannot terminate another executor's run or hand
/// back a card it has nothing to do with (MAIN-161 security).
pub async fn refuse_from_node(
    state: &AppState,
    node: NodeId,
    id: JobId,
    reason: &str,
) -> ApiResult<()> {
    let Some(tenant) = executing_tenant(state, id, node).await? else {
        tracing::warn!(job = %id.0, node = %node.0, "node refused a job it does not execute — dropped");
        return Ok(());
    };
    refuse(state, tenant, id, reason).await
}

/// Fail every job a node was executing when it disconnected (AC-4): the session
/// died with the node. Terminal jobs are untouched (the guard in `transition`).
pub async fn fail_stranded_for_node(state: &AppState, node: NodeId) -> ApiResult<()> {
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
        // Each job's OWN tenant (MAIN-515): the scan is by node, and a node may
        // hold work from any tenant its owner belongs to, so the disconnecting
        // connection's tenant is not the one to write the failure in.
        let Some((tenant, _)) = state.jobs.tenant_and_target_of(id).await? else {
            continue;
        };
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
            // The UPDATE above went round `transition`, so the handback it
            // carries has to be repeated here — a build run reaped with its
            // node is exactly the shape that wedged a card (MAIN-489).
            crate::services::build_handback::on_run_concluded(state, *tenant, &job).await;
        }
    }
    Ok(reaped.len() as u64)
}

/// Fail every in-flight job that has gone silent past `stall_secs` (MAIN-506) —
/// the orphan [`reap_stale_executors`] structurally cannot see.
///
/// An executor agent that restarts mid-run leaves its streaming child alive with
/// nobody reading its stdout: output reaches nobody, no outcome is ever
/// recorded, and a restarted agent resumes a FRESH run rather than re-adopting
/// this one. The NODE, meanwhile, is perfectly healthy — it reconnected and is
/// heartbeating — so the liveness cutoff above never trips and the job sits
/// `running` forever. Hence the different signal: the job's own progress, which
/// is what actually stopped.
///
/// Progress is "a new transcript entry or a state change", because that is what
/// a live streaming run produces continuously — an entry per assistant message
/// and per tool call. The window has to outlast the longest single tool call a
/// build might make (a full test suite), which is why `job_stall_secs` defaults
/// to an hour rather than to the starvation window's half.
///
/// Everything downstream is deliberately identical to a liveness reap: the same
/// transcript line, the same `job.state_changed` event, and the same handback,
/// so a build card whose run was orphaned comes back to the board and becomes
/// eligible for a fresh — `--resume`-warm — run under the ordinary failure
/// backoff.
pub async fn reap_stalled_jobs(state: &AppState, stall_secs: u64) -> ApiResult<u64> {
    let stalled = state.jobs.reap_stalled_jobs(stall_secs as i64).await?;

    for crate::repo::jobs::StalledJob {
        id,
        tenant,
        target_task_id: target,
        last_progress_at,
    } in &stalled
    {
        append_transcript(
            state,
            *id,
            "system",
            &format!(
                "no progress since {} — the executor agent is no longer reading this run, \
                 reaped after {stall_secs}s",
                last_progress_at.to_rfc3339()
            ),
        )
        .await
        .ok();
        if let Ok(job) = load(state, *tenant, *id).await {
            let private = target_is_private(state, *tenant, *target).await;
            record_job_event(state, *tenant, "job.state_changed", &job, private).await;
            crate::services::build_handback::on_run_concluded(state, *tenant, &job).await;
        }
    }
    Ok(stalled.len() as u64)
}

// The two ways a `queued` job ends without ever running (MAIN-496). Until this,
// `queued` had exactly one forward exit — `queued -> claimed` — so a job nothing
// could place churned forever: `reap_stale_executors` sees only jobs with an
// executor, and the claim reaper needs a lease a never-claimed card does not
// have.
//
// Both endings are `canceled`, never a new `queued -> failed` (AC-4). The run
// never happened; `failed` means it ran and lost, which is what the build
// failure ladder and outcome reporting read. `cancel` is already legal from
// every non-terminal state, so the transition table is untouched.
//
// Both are one guarded `UPDATE … WHERE state = 'queued' … RETURNING`, the same
// atomic pattern as the executor claim — so every replica may scan and a job is
// ended by exactly one of them, and a job claimed between scan and update falls
// out of the guard.

/// A queued job whose target card reached a terminal column is pointless: cancel
/// it (AC-1). No threshold and no heuristic — the closed card is the evidence.
///
/// No label change is needed here, unlike [`escalate_starved_queued`], because
/// both build work sources already exclude a finished card: `fresh_items`
/// passes `done: false` to the pick query (MAIN-464), and `tasks_with_pr`
/// excludes a terminal column too. That second half was NOT true when this
/// landed — the repair query had no column filter, so an AC-1 cancel handed the
/// same card straight back and the pair cycled forever. It is a property of the
/// card, so it belongs in the query rather than as a strip here.
pub async fn cancel_queued_on_finished_cards(state: &AppState, tenant: TenantId) -> ApiResult<u64> {
    let ended = state.jobs.cancel_queued_on_finished_cards(tenant).await?;
    for job in &ended {
        append_transcript(
            state,
            job.id,
            "system",
            &format!(
                "target card reached a terminal column after {} queued — canceled, \
                 the run has nothing left to do{}",
                waited(job.queued_since),
                job.queued_reason
                    .as_deref()
                    .map(|r| format!(" (last reason: {r})"))
                    .unwrap_or_default()
            ),
        )
        .await
        .ok();
        announce_queued_ending(state, job).await;
    }
    Ok(ended.len() as u64)
}

/// The queued build jobs a red default branch is currently holding (MAIN-543
/// AC-9) — the ones the starvation window must not escalate.
///
/// A dispatch pause produces exactly the shape [`escalate_starved_queued`]
/// looks for: a reason that does not move, on a job nothing is placing. Left
/// alone, a trunk red for longer than `starve_secs` would mark every waiting
/// card `blocked` and hand it back to a human — the outcome the pause exists to
/// prevent. So the pause is EXEMPT, and the exemption is asked of the same
/// derived signal on every scan rather than of a stored marker: the moment the
/// trunk goes green the jobs are ordinary again, including for this rule.
///
/// A forge outage yields an empty set, which restores the pre-MAIN-496
/// behaviour exactly — the same fail-open direction as dispatch itself.
///
/// The candidate read carries the starvation window, so a scan with nothing
/// near it asks no forge at all: this costs one indexed query on the ordinary
/// pass, and reaches GitHub only for a job that is actually about to be
/// escalated.
async fn paused_by_red_trunk(
    state: &AppState,
    tenant: TenantId,
    starve_secs: u64,
) -> ApiResult<Vec<JobId>> {
    let queued = state
        .jobs
        .queued_builds_starving(tenant, starve_secs as i64)
        .await?;
    let mut pass = main_ci::Pass::default();
    let mut exempt = Vec::new();
    for build in queued {
        if main_ci::red_default_branch(state, tenant, build.workspace_id, &mut pass)
            .await
            .is_some()
        {
            exempt.push(build.id);
        }
    }
    Ok(exempt)
}

/// A queued job whose reason has stood unchanged past `starve_secs` is starved:
/// cancel it, take the card out of the loop's reach, and say so on it
/// (AC-2/AC-3).
///
/// Stopping re-selection is the load-bearing half — a bare cancel on a
/// still-open card produces cancel -> enqueue -> cancel forever, worse than the
/// silence it replaces — and it takes BOTH labels, because the two build work
/// sources gate on different things. `fresh_items` reads `agent-ready` cards,
/// so the strip removes the card from that set. `repair_items` never consults
/// `agent-ready` at all: it selects from cards with a recorded PR, and what
/// removes a card from THAT set is `blocked` (`tasks_with_pr`). Stripping alone
/// would have left a repair run cycling indefinitely, one comment and one
/// notification per threshold.
///
/// The escalation shape is the claim reaper's cap path, not a second vocabulary:
/// an activity event, a comment on the card, and a warning notification that
/// links it without naming a private card's title. `blocked` is likewise the
/// label `record_build_outcome`'s own handback already uses for "the loop
/// cannot proceed; a human decides" — not a third name for the same state.
pub async fn escalate_starved_queued(
    state: &AppState,
    tenant: TenantId,
    starve_secs: u64,
) -> ApiResult<u64> {
    let ended = state
        .jobs
        .cancel_starved_queued(
            tenant,
            starve_secs as i64,
            &paused_by_red_trunk(state, tenant, starve_secs).await?,
        )
        .await?;
    for job in &ended {
        let reason = job.queued_reason.as_deref().unwrap_or("no reason recorded");
        // The wait quoted is the UNCHANGED-REASON wait, which is what the
        // threshold actually measured — `created_at`'s span would read as a
        // claim about the whole time queued that this rule never checked.
        let body = format!(
            "loop run starved — this reason stood unchanged for {}, so the run was canceled \
             and the card marked `blocked` (and `agent-ready` removed) to keep the loop from \
             re-raising it until a human looks: {reason}",
            waited(job.reason_since)
        );
        append_transcript(state, job.id, "system", &body).await.ok();
        announce_queued_ending(state, job).await;

        // A `review` run has no card: the job transcript and the event above are
        // the whole record, and there is no label to set.
        let Some(task) = job.target_task_id else {
            continue;
        };
        if let Err(e) = state.tasks.set_agent_ready(job.tenant, task, false).await {
            tracing::warn!(job = %job.id, task = %task.0, error = %e, "could not strip agent-ready from a starved card");
        }
        if let Err(e) = state.tasks.attach_label(job.tenant, task, "blocked").await {
            tracing::warn!(job = %job.id, task = %task.0, error = %e, "could not mark a starved card blocked");
        }
        let _ = state
            .tasks
            .create_comment(crate::repo::tasks::NewComment {
                tenant: job.tenant,
                task,
                author_type: "system".into(),
                author_id: None,
                author_name: "Loop-job reaper".into(),
                body_md: body.clone(),
            })
            .await;
        raise_starved(state, job.tenant, task, &body).await;
        state.registry.publish(
            job.tenant,
            nook_proto::UiEvent::TaskChanged { task_id: task },
        );
    }
    Ok(ended.len() as u64)
}

/// The `job.state_changed` event any transition would have emitted — the atomic
/// UPDATE already made the change, this is only its announcement (and the live
/// nudge the job surfaces listen for).
async fn announce_queued_ending(state: &AppState, job: &crate::repo::jobs::EndedQueuedJob) {
    if let Ok(row) = load(state, job.tenant, job.id).await {
        let private = target_is_private(state, job.tenant, row.target_task_id).await;
        record_job_event(state, job.tenant, "job.state_changed", &row, private).await;
    }
    state.registry.publish(
        job.tenant,
        nook_proto::UiEvent::JobChanged {
            task_id: job.target_task_id,
        },
    );
}

/// A warning notification linking the starved card. A private card's title never
/// reaches the tenant-wide bell (MAIN-76), so only the key and the link go out —
/// the same rule the claim reaper's escalation follows.
async fn raise_starved(state: &AppState, tenant: TenantId, task: TaskId, body: &str) {
    let key = state
        .tasks
        .key_of(tenant, task)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| task.0.to_string());
    let base = state.cfg.public_base_url.trim_end_matches('/');
    crate::services::notify::raise(
        state,
        tenant,
        crate::services::notify::Draft::new(format!("Loop run starved: {key}"))
            .level("warning")
            .kind("job.starved")
            .body(body.to_string())
            .link(format!("{base}/board?task={task}"))
            .payload(json!({ "task_id": task, "key": key })),
    )
    .await;
}

/// How long a job has been waiting, for a human to read. Coarse on purpose: the
/// escalation is about hours and minutes, and a seconds-exact figure in a card
/// comment reads as precision nobody asked for.
fn waited(since: chrono::DateTime<chrono::Utc>) -> String {
    let mins = (chrono::Utc::now() - since).num_minutes().max(0);
    match (mins / 60, mins % 60) {
        (0, 0) => "under a minute".to_string(),
        (0, m) => format!("{m}m"),
        (h, 0) => format!("{h}h"),
        (h, m) => format!("{h}h{m}m"),
    }
}

/// The tenant of the job `node` is executing, or `None` when it is not its
/// executor. The gate for accepting a node's streamed transcript / finish
/// (MAIN-161 security): a node token is scoped to its OWN runs, so it must not
/// be able to inject into or terminate another executor's job. `None` for a
/// missing job, an unplaced one, or a mismatch.
///
/// **The tenant is the JOB's, never the connection's (MAIN-515).** A node
/// authenticates in the tenant the MACHINE was joined into, and since ownership
/// crosses tenants a job placed there by its owner need not live in that one.
/// Asking with the connection's tenant found no row, so every report from such
/// a run — transcript, worktree, finish, refusal — was dropped as a spoof and
/// the run completed with nothing recorded, to be reaped later as stalled. The
/// node match below is what authorizes; the tenant is looked up, not asserted.
async fn executing_tenant(
    state: &AppState,
    id: JobId,
    node: NodeId,
) -> ApiResult<Option<TenantId>> {
    let Some((tenant, _)) = state.jobs.tenant_and_target_of(id).await? else {
        return Ok(None);
    };
    let exec: Option<Option<NodeId>> = state.jobs.executor_of(tenant, id).await?;
    Ok(matches!(exec, Some(Some(n)) if n == node).then_some(tenant))
}

/// Append a transcript line reported by a node — ONLY for a job that node is
/// actually executing. A line for someone else's job (or a spoof) is dropped
/// with a warning, never applied (MAIN-161 security).
pub async fn transcript_from_node(
    state: &AppState,
    node: NodeId,
    id: JobId,
    source: &str,
    content: &str,
) -> ApiResult<()> {
    if executing_tenant(state, id, node).await?.is_none() {
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
pub async fn turn_from_node(state: &AppState, node: NodeId, id: JobId, active: bool) {
    let tenant = match executing_tenant(state, id, node).await {
        Ok(Some(t)) => t,
        _ => {
            tracing::warn!(job = %id.0, node = %node.0, "node reported a turn for a job it does not execute — dropped");
            return;
        }
    };
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
    node: NodeId,
    id: JobId,
    ok: bool,
    message: &str,
) -> ApiResult<()> {
    let Some(tenant) = executing_tenant(state, id, node).await? else {
        tracing::warn!(job = %id.0, node = %node.0, "node reported finish for a job it does not execute — dropped");
        return Ok(());
    };
    finish(state, tenant, id, ok, message).await
}

/// Record where a build run's worktree lives (MAIN-480 AC-4).
///
/// Node-scoped like every other node report: a node may only speak for a job it
/// actually executes, so it cannot point another executor's card at a directory
/// on this machine.
///
/// The record is the load-bearing part of the ticket, not bookkeeping. It is
/// what pins later passes to this node (`select_executor`), what
/// `prune-worktree` addresses, and what tells the node's reconnect sweep the
/// directory is still wanted.
pub async fn record_worktree_from_node(
    state: &AppState,
    node: NodeId,
    id: JobId,
    path: &str,
) -> ApiResult<()> {
    let Some(tenant) = executing_tenant(state, id, node).await? else {
        tracing::warn!(job = %id.0, node = %node.0, "node reported a worktree for a job it does not execute — dropped");
        return Ok(());
    };
    let job = load(state, tenant, id).await?;
    let Some(task) = job.target_task_id else {
        return Ok(());
    };
    state.tasks.record_loop_worktree(task, node, path).await?;
    state
        .registry
        .publish(tenant, nook_proto::UiEvent::TaskChanged { task_id: task });
    Ok(())
}

/// Answer a reconnecting node's inventory of build worktrees (MAIN-480 AC-1):
/// anything it holds that no card records HERE is an orphan, and the node is
/// told to remove it.
///
/// Deliberately one-directional. A path this side records but the node does not
/// hold is NOT repaired from here — the tree may be on a machine that is simply
/// slow to report, and re-creating it is the next run's job anyway. Only the
/// direction that leaks disk is acted on.
pub async fn sweep_worktrees_on_node(
    state: &AppState,
    tenant: TenantId,
    node: NodeId,
    held: &[String],
) -> ApiResult<usize> {
    let recorded = state.tasks.worktree_paths_on_node(node).await?;
    let orphans: Vec<&String> = held.iter().filter(|p| !recorded.contains(p)).collect();
    for path in &orphans {
        tracing::info!(node = %node.0, path = %path, "removing a build worktree no card records");
        let _ = state.registry.request_op(node, |request_id| {
            nook_proto::ControlToNode::RemoveWorktree {
                request_id,
                worktree_path: (*path).clone(),
            }
        });
    }
    let _ = tenant;
    Ok(orphans.len())
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

#[cfg(test)]
mod tests {
    use super::closes_key;

    /// The literal contract, and only it: a line starting `Closes `, a token
    /// shaped like a key. Lookalikes must not join a PR to a card it does not
    /// close.
    #[test]
    fn the_closes_line_finds_its_key_and_ignores_lookalikes() {
        assert_eq!(
            closes_key("What changed\n\nCloses MAIN-459\n\nRisk: Low"),
            Some("MAIN-459".into())
        );
        assert_eq!(closes_key("Closes WEB-UI-7 tail"), Some("WEB-UI-7".into()));
        assert_eq!(closes_key("It closes MAIN-459"), None, "mid-sentence");
        assert_eq!(closes_key("Closes the gap"), None, "no key shape");
        assert_eq!(closes_key("Closes MAIN-"), None, "no number");
        assert_eq!(closes_key(""), None);
    }
}

#[cfg(test)]
mod verdict_mirror_tests {
    use super::mirror_is_duplicate;

    /// MAIN-477 AC-2/AC-4: the card mirror never appends an identical line,
    /// and never swallows a real event.
    #[test]
    fn the_mirror_collapses_duplicates_and_keeps_real_events() {
        let existing = vec![
            "Loop review of abc123 — changes requested: https://github.com/a/b/pull/7",
            "PR opened: https://github.com/a/b/pull/7",
        ];
        assert!(mirror_is_duplicate(
            &existing,
            "abc123",
            "changes requested"
        ));
        // A push is a new head — a real event.
        assert!(!mirror_is_duplicate(
            &existing,
            "def456",
            "changes requested"
        ));
        // The verdict changing at the same head is a real event too.
        assert!(!mirror_is_duplicate(&existing, "abc123", "approved"));
        assert!(!mirror_is_duplicate(&[], "abc123", "approved"));
    }
}
