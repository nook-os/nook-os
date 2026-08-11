//! Loop-job and interaction data access (MAIN-255).
//!
//! Two traits:
//!
//! - [`LoopJobRepository`] — `loop_jobs` and `loop_job_transcript`: a run's
//!   lifecycle and its narration.
//! - [`InteractionRepository`] — `interactions`: the durable ask/answer a
//!   running job uses to reach a human.
//!
//! **What is deliberately NOT here.** Placing a job needs to know things about
//! other aggregates — who the requester is as a person, which node can execute,
//! what a ticket's key is, where a checkout lives. Every one of those already
//! has a repository, so the reads went onto their owners rather than being
//! copied here:
//!
//! - `users.person_id` → [`crate::repo::identity::IdentityRepository::person_id_of`]
//! - `tasks` → [`crate::repo::tasks::TaskRepository`] (`get_row`, `key_of`)
//! - `nodes` executor selection → [`crate::repo::nodes::NodeRepository`]
//! - `node_workspaces` → [`crate::repo::workspaces::WorkspaceRepository`]
//!
//! That matters more here than elsewhere: executor selection is a `nodes` query
//! with a jsonb runtime-auth scan, and a second copy of it living under "jobs"
//! is exactly how the two drift into disagreeing about who may run work.
//!
//! The queue-riding enqueue stays on the queue provider (AC-1); only DB reads
//! and writes moved.
//!
//! Methods are intent-named and coarse; no `sqlx` type appears in any
//! signature, and row mapping lives inside the impls (AC-2).

use async_trait::async_trait;
use nook_db::dialect::{time_math, type_mapping};
use nook_db::{params, Db, DbPool};
use nook_types::*;

use crate::error::ApiResult;

/// A job to enqueue.
#[derive(Debug, Clone)]
pub struct NewLoopJob {
    pub id: JobId,
    pub tenant: TenantId,
    pub kind: String,
    /// `None` for a `review` job, which targets `workspace_id` instead. The
    /// database CHECK from 0040 enforces exactly one of the two.
    pub target_task_id: Option<TaskId>,
    pub workspace_id: Option<WorkspaceId>,
    pub requested_by: UserId,
    pub seed: Option<String>,
    /// Set only by a re-run, which records what it descends from.
    pub predecessor_job_id: Option<JobId>,
    /// The work item, for a `review` run: which PR, at which head.
    pub review_pr_number: Option<i64>,
    pub review_head_sha: Option<String>,
    /// A human forced this review at an already-verdicted head (MAIN-473).
    pub review_forced: bool,
    /// The work item, for a `build` run: what the card looked like when the
    /// run was raised (MAIN-458) — `review_head_sha`'s twin.
    pub build_fingerprint: Option<String>,
}

/// What the wakeup rule knows about one pull request's runs.
#[derive(Debug, Clone, nook_db::FromDbRow)]
pub struct RunHeads {
    pub item_key: i64,
    /// The head of the newest run that CONCLUDED NOTHING, and when. Two states
    /// land here: `failed`, and `completed` with no recorded verdict — an agent
    /// that ends a pass early (checks pending, environment broken) exits zero
    /// like any other, and the two mean the same thing to the wakeup rule.
    /// Neither counts as reviewed (one bad run must not silence a PR until
    /// somebody pushes), and neither is retried instantly (a run every poll
    /// interval for the length of a CI cycle is the hot loop the hold
    /// prevents).
    pub attempted_head: Option<String>,
    pub attempted_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The head a run is in flight for, if one is. Its presence is what stops a
    /// second run being raised for the same PR.
    pub live_head: Option<String>,
    /// The head of the newest run that actually finished. A PR whose forge head
    /// still equals this has been reviewed as it stands.
    pub done_head: Option<String>,
}

/// The newest recorded verdict for one PR: which head it judged, and what it
/// said. What label restoration (MAIN-476 AC-3) reads — only verdicts this
/// deployment itself recorded, which is the "one writer wins" rule.
#[derive(Debug, Clone, nook_db::FromDbRow)]
pub struct RecordedVerdict {
    pub review_pr_number: i64,
    pub review_head_sha: String,
    pub review_verdict: String,
}

/// The value `review_verdict_source` carries for a verdict the CONTROL PLANE
/// concluded from a merge conflict (MAIN-516). `NULL` is an agent's own
/// judgement; this is the one thing that is not.
pub const CONFLICT_VERDICT_SOURCE: &str = "conflict";

/// A `changes_requested` nobody reviewed: the pull request conflicts with its
/// base, so the control plane puts it back in the repair queue — which reads
/// this ledger and not the PR's labels (MAIN-516).
#[derive(Debug, Clone)]
pub struct ConflictRejection {
    pub id: JobId,
    pub tenant: TenantId,
    pub workspace: WorkspaceId,
    /// Who the row is attributed to — the identity the hygiene pass already
    /// comments and labels as.
    pub requested_by: UserId,
    pub pr: i64,
    /// The head the conflict is AT, which is what the repair fingerprints on:
    /// the rebase that answers it moves the head, and the fingerprint clears
    /// itself.
    pub head: String,
    /// What a reader of the ledger sees this row was about.
    pub seed: String,
}

/// A job the reaper found stranded on a node that stopped reporting, with the
/// moment that node was last seen — the transcript line quotes it.
#[derive(Debug, Clone)]
pub struct ReapedJob {
    pub id: JobId,
    pub tenant: TenantId,
    /// `None` for a reaped `review` job — it has no ticket.
    pub target_task_id: Option<TaskId>,
    pub node_last_seen_at: chrono::DateTime<chrono::Utc>,
}

/// A job the reaper found orphaned: still `claimed`/`running` on a HEALTHY node,
/// but silent past the stall window (MAIN-506). `last_progress_at` is the moment
/// it last showed a sign of life, which the transcript line quotes.
#[derive(Debug, Clone)]
pub struct StalledJob {
    pub id: JobId,
    pub tenant: TenantId,
    /// `None` for a `review` job — it has no ticket.
    pub target_task_id: Option<TaskId>,
    pub last_progress_at: chrono::DateTime<chrono::Utc>,
}

/// A job the reaper ended while it was still `queued` (MAIN-496), with what it
/// was waiting on. `queued_reason` is preserved rather than overwritten — it is
/// the record of why the run never placed (AC-5).
///
/// The two timestamps are two different questions and the wrong one reads as a
/// claim the rule never checked: `queued_since` is the whole wait, and
/// `reason_since` is how long the reason has stood unchanged, which is what the
/// starvation threshold actually measures.
#[derive(Debug, Clone)]
pub struct EndedQueuedJob {
    pub id: JobId,
    pub tenant: TenantId,
    /// `None` for a `review` job — it has no ticket to escalate onto.
    pub target_task_id: Option<TaskId>,
    pub queued_reason: Option<String>,
    pub queued_since: chrono::DateTime<chrono::Utc>,
    pub reason_since: chrono::DateTime<chrono::Utc>,
}

/// One row a scan is about to end, read BEFORE the cancel — which is the only
/// way to see the `updated_at` that is the reason's clock rather than the moment
/// of cancellation.
///
/// Read separately rather than as a `RETURNING` over `UPDATE … FROM`, which is
/// where this started: SQLite's `RETURNING` cannot name the target by alias nor
/// reach a FROM-joined table, so that shape parsed on Postgres and died on the
/// other engine (`no such column: j.id`). The separation costs nothing that
/// matters, because the READ was never what made an ending exactly-once — the
/// guard on the UPDATE is, and it is unchanged.
#[derive(nook_db::FromDbRow)]
struct QueuedCandidate {
    id: JobId,
    tenant_id: TenantId,
    target_task_id: Option<TaskId>,
    queued_reason: Option<String>,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

impl QueuedCandidate {
    fn into_ended(self) -> EndedQueuedJob {
        EndedQueuedJob {
            id: self.id,
            tenant: self.tenant_id,
            target_task_id: self.target_task_id,
            queued_reason: self.queued_reason,
            queued_since: self.created_at,
            reason_since: self.updated_at,
        }
    }
}

/// One silent in-flight job, read before the fail — for `QueuedCandidate`'s
/// reason (SQLite's `RETURNING` reaches neither an alias nor a joined table).
///
/// The two progress facts are read SEPARATELY and folded in Rust rather than
/// with `GREATEST`, which SQLite does not have under that name. Both are needed:
/// `updated_at` alone would call a job that has just been claimed stale as soon
/// as it inherited an old row's clock, and the transcript alone is `NULL` for a
/// run that has not written a line yet.
#[derive(nook_db::FromDbRow)]
struct StalledCandidate {
    id: JobId,
    tenant_id: TenantId,
    target_task_id: Option<TaskId>,
    updated_at: chrono::DateTime<chrono::Utc>,
    last_entry_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl StalledCandidate {
    fn into_stalled(self) -> StalledJob {
        StalledJob {
            id: self.id,
            tenant: self.tenant_id,
            target_task_id: self.target_task_id,
            last_progress_at: self
                .last_entry_at
                .unwrap_or(self.updated_at)
                .max(self.updated_at),
        }
    }
}

/// A new interaction: a running job asking a human something.
#[derive(Debug, Clone)]
pub struct NewInteraction {
    pub id: InteractionId,
    pub tenant: TenantId,
    pub job_id: Option<JobId>,
    pub task_id: Option<TaskId>,
    pub prompt: String,
    pub choices: Option<Vec<String>>,
    pub requested_by_node_id: Option<NodeId>,
    pub requested_by_session_id: Option<SessionId>,
}

#[async_trait]
pub trait LoopJobRepository: Send + Sync {
    async fn get(&self, tenant: TenantId, id: JobId) -> ApiResult<Option<LoopJob>>;

    /// Unscoped — the node socket holds a job id before it knows the tenant,
    /// and authorizes on what comes back. Named so that is visible.
    ///
    /// `None` means "no ticket to name": either the job does not exist, or it
    /// is a `review` job, which targets a workspace. Both callers publish a
    /// ticket-keyed `UiEvent`, so both correctly do nothing in either case —
    /// which is why the two are deliberately not distinguished here.
    async fn target_task_of_unscoped(&self, id: JobId) -> ApiResult<Option<TaskId>>;

    async fn create(&self, new: NewLoopJob) -> ApiResult<LoopJob>;

    /// Per PR: the head of its newest LIVE run, and the head of its newest
    /// COMPLETED run. Both `None` when that PR has no such run.
    ///
    /// One query rather than two calls per pull request, because a repo with
    /// forty open PRs would otherwise make forty round trips on every pass.
    /// This is the whole state the wakeup rule reads: a run is owed when the
    /// forge's head differs from the completed head and nothing is live.
    async fn review_run_heads(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
    ) -> ApiResult<Vec<RunHeads>>;

    /// The newest REJECTING head per PR: what the latest
    /// `changes_requested` review run reviewed. This — never the PR's current
    /// head, which the repair's own push moves — is what a repair item is
    /// fingerprinted on (MAIN-458): "a new REJECTED head re-raises".
    async fn rejected_review_heads(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
    ) -> ApiResult<Vec<(i64, String)>>;

    /// `review_run_heads`'s twin for BUILD runs (MAIN-458), keyed by the
    /// card's board number — the same key `BuildWork` items carry — with
    /// `build_fingerprint`/`build_outcome` in the roles head-sha/verdict play
    /// for reviews.
    ///
    /// All THREE terminal states are an attempt HERE (MAIN-489), because this
    /// is the wakeup REST and not the failure ladder: a canceled run's card is
    /// handed straight back now, so leaving it out would re-raise it on the
    /// very next pass. The ladder itself still reads `failed` alone, which is
    /// the distinction MAIN-496 drew when it chose to cancel a queued job it
    /// could not place rather than fail it.
    async fn build_run_heads(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
    ) -> ApiResult<Vec<RunHeads>>;

    /// Record what a build run concluded. Guarded on a live build run — an
    /// outcome on a finished or foreign job is a caller bug, answered with 0.
    async fn set_build_outcome(&self, id: JobId, outcome: &str) -> ApiResult<u64>;

    /// Per PR: the newest completed run that recorded a REAL verdict —
    /// `skipped` deferred to someone else's review, so it restores nothing.
    async fn recorded_review_verdicts(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
    ) -> ApiResult<Vec<RecordedVerdict>>;

    /// The LIVE review run for one PR, if any — a targeted lookup on the same
    /// predicate 0046's partial unique index arbitrates, so a refusal can name
    /// the run to wait on unconditionally (MAIN-473), not only when it falls
    /// inside some listing window.
    async fn live_review_run_for(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
        pr: i64,
    ) -> ApiResult<Option<JobId>>;

    /// A workspace's review runs, newest first — what the workspace's review
    /// surface reads. Bounded, because a busy repo accumulates one per push per
    /// PR and a page does not want all of them.
    async fn list_reviews_for_workspace(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
        limit: i64,
    ) -> ApiResult<Vec<LoopJob>>;

    /// The Builds panel's rows (MAIN-461 AC-2): each build run with its card
    /// named by KEY — joined here because `LoopJob` never carries the key and
    /// the panel must not pay one lookup per row. The key is VIEWER-GATED
    /// (MAIN-265/MAIN-86): a private card a viewer cannot see lists its run
    /// keyless, exactly as a private epic's key is withheld from its children.
    async fn list_builds_for_workspace(
        &self,
        tenant: TenantId,
        viewer: nook_types::UserId,
        workspace: WorkspaceId,
        limit: i64,
    ) -> ApiResult<Vec<nook_types::WorkspaceBuildRun>>;

    /// The states of a workspace's LIVE build runs (MAIN-495) — every run that
    /// has not reached a terminal state, `queued` ones included.
    ///
    /// States rather than a count, because the build-loop status has to tell
    /// the two apart: a queued run is not running, and it is very often exactly
    /// what the shortfall beside it is about. Unbounded on purpose — a limit
    /// would silently undercount, which is the one thing a capacity report may
    /// not do, and the live set is bounded by the ceiling and the fleet anyway.
    async fn live_build_states(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
    ) -> ApiResult<Vec<String>>;

    /// The MCP status surface's rows (MAIN-525 AC-1): a workspace's runs,
    /// newest first, each already carrying its card's KEY and its executor's
    /// NAME. `kind` `None` means every kind; `live_only` keeps the runs that
    /// have not reached a terminal state. The key is VIEWER-GATED exactly as
    /// [`LoopJobRepository::list_builds_for_workspace`]'s is — a run's
    /// transcript can quote a private card, so its identity is withheld the
    /// same way.
    async fn list_runs_for_workspace(
        &self,
        tenant: TenantId,
        viewer: nook_types::UserId,
        workspace: WorkspaceId,
        kind: Option<&str>,
        live_only: bool,
        limit: i64,
    ) -> ApiResult<Vec<nook_types::LoopRunSummary>>;

    /// Record what a review run concluded. Guarded on a live review run — a
    /// verdict on a finished or foreign job is a caller bug, answered with 0.
    async fn set_review_verdict(&self, id: JobId, verdict: &str) -> ApiResult<u64>;

    /// Record the `changes_requested` a CONFLICTING pull request earns, at the
    /// head it conflicts at (MAIN-516). Answers whether it recorded one.
    ///
    /// `false` is the idempotent case, and it covers both shapes of "already
    /// said": this head already carries a `changes_requested` — an agent's, or
    /// an earlier pass's — or another replica inserted its row between our read
    /// and our write, which 0060's partial unique index arbitrates.
    ///
    /// The row is the NEWEST verdict for that head once written, which matters
    /// for the one overlap: a review run already live at this head records its
    /// own verdict afterwards, and `recorded_review_verdicts` — newest wins —
    /// keeps reporting the conflict's `changes_requested`. That is the honest
    /// answer (the pull request does conflict, whatever the reviewer thought of
    /// the code), and it does not stick: the rebase moves the head, and the
    /// verdict recorded for the new one is nobody's but the reviewer's.
    async fn record_conflict_rejection(&self, rejection: ConflictRejection) -> ApiResult<bool>;

    /// The live epic-run for this epic, if one is in flight (MAIN-144 AC-3) —
    /// the dedupe that keeps "one deliberate enqueue per pass" true.
    async fn active_epic_run_for(&self, tenant: TenantId, task: TaskId)
        -> ApiResult<Option<JobId>>;

    async fn list_for_task(&self, tenant: TenantId, task: TaskId) -> ApiResult<Vec<LoopJob>>;

    async fn transition(&self, id: JobId, to: &str) -> ApiResult<LoopJob>;

    /// Place a job on a node — but only from `queued`, so of two racing
    /// dispatchers exactly one wins and the loser sees `None`.
    async fn claim_for_executor(&self, id: JobId, node: NodeId) -> ApiResult<Option<LoopJob>>;

    /// Explain why a job is still queued, as the sentence a human reads AND the
    /// gate a client branches on (MAIN-494) — one write, so the two can never
    /// describe different waits. `kind` is `None` for the residual
    /// no-eligible-executor reason, which is not a gate.
    ///
    /// Guarded on `queued` so a job that got placed in the meantime is not
    /// annotated with a stale excuse, and on the reason actually CHANGING —
    /// re-writing the same sentence every dispatch cycle is not news, and the
    /// starvation rule (MAIN-496) reads `updated_at` as the moment the reason
    /// last moved.
    async fn set_queued_reason(
        &self,
        id: JobId,
        reason: &str,
        kind: Option<QueuedReason>,
    ) -> ApiResult<u64>;

    /// Reload unscoped, for the paths that have just written by id.
    async fn reload(&self, id: JobId) -> ApiResult<LoopJob>;

    /// Which tenant and ticket a job belongs to, unscoped — what the transcript
    /// append needs to aim its live nudge. Best-effort at the call site: a
    /// missing job means no nudge, never a failed append. The inner `Option` is
    /// `None` for a `review` job (no ticket); the outer is `None` for no such
    /// job.
    async fn tenant_and_target_of(
        &self,
        id: JobId,
    ) -> ApiResult<Option<(TenantId, Option<TaskId>)>>;

    /// Which node a job was placed on. The outer `Option` is "no such job";
    /// the inner is "not placed yet".
    async fn executor_of(&self, tenant: TenantId, id: JobId) -> ApiResult<Option<Option<NodeId>>>;

    /// Jobs still believed to be running on a node — what a disconnect strands.
    async fn in_flight_on_node(&self, node: NodeId) -> ApiResult<Vec<JobId>>;

    /// `tenant`'s `queued` jobs, worthiest first — the order a freed executor
    /// is offered to them (MAIN-509).
    ///
    /// **The rule is card priority, then how long the job has waited**, and it
    /// is expressed here as a query precisely so every replica applies the same
    /// one. Placement used to be whichever work item happened to be in the
    /// durable queue when an executor freed, which systematically favoured the
    /// NEWEST job: an unplaceable one is re-armed after a delay while a freshly
    /// raised one is delivered at once. Accidental LIFO, and `!!` jobs waited
    /// hours behind a `↑` one.
    ///
    /// Priority sorts the board's way (`1` urgent … `4` low, `0` unset LAST),
    /// so this cannot invert what a human set. A job with no card — a review
    /// run — is unset by the same reading, and sorts with the other unset ones
    /// rather than being invented a rank. `created_at` then `id` breaks every
    /// remaining tie, so the order is total and identical everywhere.
    async fn queued_in_dispatch_order(&self, tenant: TenantId) -> ApiResult<Vec<JobId>>;

    /// The live build run already on a card, if any (MAIN-383 AC-4) — what the
    /// create path names in its refusal. The 0050 partial unique index is the
    /// atomic version of the same rule.
    async fn active_build_for(&self, tenant: TenantId, task: TaskId) -> ApiResult<Option<JobId>>;

    /// Fail every job whose executor stopped reporting more than `grace_secs`
    /// ago, returning what was reaped. One guarded `UPDATE … RETURNING`, so two
    /// reapers cannot double-fail a job and a job that resumed between scan and
    /// update falls out of the guard untouched.
    async fn reap_stale_executors(&self, grace_secs: i64) -> ApiResult<Vec<ReapedJob>>;

    /// Fail every `claimed`/`running` job that has shown no progress — no new
    /// transcript entry, no state change — for more than `stall_secs`, whatever
    /// its node's liveness says (MAIN-506).
    ///
    /// This is the orphan case [`Self::reap_stale_executors`] structurally
    /// cannot see: an executor agent that restarted leaves its streaming child
    /// running with nobody reading it, and the NODE is fine — heartbeating,
    /// `last_seen_at` at now — so the liveness cutoff never trips and the job
    /// sits `running` forever. Job-level progress is the signal because
    /// job-level progress is what actually stopped.
    ///
    /// `waiting_on_human` is excluded for the same reason it is excluded there:
    /// a paused run is silent by design, indefinitely.
    async fn reap_stalled_jobs(&self, stall_secs: i64) -> ApiResult<Vec<StalledJob>>;

    /// Cancel `tenant`'s `queued` jobs whose target card has reached a terminal
    /// column (MAIN-496 AC-1). A closed card is unambiguous evidence the run is
    /// pointless, so there is no threshold here — the rule is the column.
    ///
    /// Tenant-scoped, unlike [`Self::reap_stale_executors`], because the caller
    /// must ask `loops::enabled` of the tenant whose board it is about to
    /// write: cancelling a loops-OFF tenant's job because a DIFFERENT tenant
    /// has loops on would break MAIN-239's promise that such a job simply waits.
    async fn cancel_queued_on_finished_cards(
        &self,
        tenant: TenantId,
    ) -> ApiResult<Vec<EndedQueuedJob>>;

    /// Cancel `tenant`'s `queued` jobs whose `queued_reason` has stood unchanged
    /// for more than `starve_secs` (MAIN-496 AC-2). A job with NO reason yet has
    /// never been through dispatch — loops may simply be off — and is left
    /// alone; a job whose reason keeps moving is progressing toward placement
    /// and its `updated_at` keeps moving with it.
    async fn cancel_starved_queued(
        &self,
        tenant: TenantId,
        starve_secs: i64,
    ) -> ApiResult<Vec<EndedQueuedJob>>;

    // ── transcript ──────────────────────────────────────────────────────────

    async fn transcript(&self, id: JobId) -> ApiResult<Vec<LoopJobTranscriptEntry>>;

    async fn append_transcript(
        &self,
        id: JobId,
        source: &str,
        content: &str,
    ) -> ApiResult<LoopJobTranscriptEntry>;
}

#[async_trait]
pub trait InteractionRepository: Send + Sync {
    async fn get(&self, tenant: TenantId, id: InteractionId) -> ApiResult<Option<Interaction>>;

    async fn create(&self, new: NewInteraction) -> ApiResult<Interaction>;

    async fn list_pending(&self, tenant: TenantId) -> ApiResult<Vec<Interaction>>;

    /// Answer, but only while pending — so two people racing to answer produce
    /// one winner and one `None`, rather than the second overwriting the first.
    async fn answer(
        &self,
        id: InteractionId,
        viewer: UserId,
        response: &str,
    ) -> ApiResult<Option<Interaction>>;

    async fn cancel(&self, id: InteractionId) -> ApiResult<Option<Interaction>>;

    /// Cancel every question a job still had outstanding — what finishing or
    /// failing a run does to the asks nobody answered. Tenant-scoped, like
    /// every other write here.
    async fn cancel_for_job(&self, tenant: TenantId, job: JobId) -> ApiResult<Vec<Interaction>>;
}

// ── the DbPool implementations ──────────────────────────────────────────────

pub struct DbLoopJobRepository {
    db: DbPool,
}

/// The candidate read both queued-job endings start from, up to the `AND …`
/// each one appends. `$1` is the tenant (MAIN-496: these two write a board a
/// human reads, so they run only for a tenant whose loops are on).
const QUEUED_CANDIDATE_COLS: &str = "SELECT id, tenant_id, target_task_id, queued_reason,
            created_at, updated_at
       FROM loop_jobs
      WHERE tenant_id = $1 AND state = 'queued'";

impl DbLoopJobRepository {
    pub fn new(db: DbPool) -> Self {
        Self { db }
    }

    /// Cancel ONE candidate, re-asserting the predicate that made it one.
    /// `true` when this caller is the one that ended it — the same
    /// exactly-once property the executor claim has, so every replica may scan.
    /// `binds[0]` is the job id (`$1`); a guard needing more starts at `$2`.
    async fn claim_ending(&self, guard: &str, binds: Vec<nook_db::DbValue>) -> ApiResult<bool> {
        Ok(self
            .db
            .exec(
                &format!(
                    "UPDATE loop_jobs SET state = 'canceled', updated_at = {now}
                      WHERE id = $1 AND state = 'queued' AND {guard}",
                    now = type_mapping(self.db.engine()).now()
                ),
                binds,
            )
            .await?
            > 0)
    }
}

#[async_trait]
impl LoopJobRepository for DbLoopJobRepository {
    async fn get(&self, tenant: TenantId, id: JobId) -> ApiResult<Option<LoopJob>> {
        Ok(self
            .db
            .query_opt(
                "SELECT * FROM loop_jobs WHERE id = $1 AND tenant_id = $2",
                params![id, tenant],
            )
            .await?)
    }

    async fn target_task_of_unscoped(&self, id: JobId) -> ApiResult<Option<TaskId>> {
        // `IS NOT NULL` rather than decoding a nullable scalar: a review job's
        // row simply does not match, which yields the `None` the contract asks
        // for without a NULL ever reaching `get_at`.
        Ok(self
            .db
            .query_scalar_opt::<TaskId>(
                "SELECT target_task_id FROM loop_jobs
                 WHERE id = $1 AND target_task_id IS NOT NULL",
                params![id],
            )
            .await?)
    }

    async fn create(&self, new: NewLoopJob) -> ApiResult<LoopJob> {
        Ok(self
            .db
            .query_one(
                "INSERT INTO loop_jobs
                    (id, tenant_id, kind, target_task_id, workspace_id, requested_by,
                     state, predecessor_job_id, seed, review_pr_number, review_head_sha,
                     build_fingerprint, review_forced)
                 VALUES ($1, $2, $3, $4, $5, $6, 'queued', $7, $8, $9, $10, $11, $12)
                 RETURNING *",
                params![
                    new.id,
                    new.tenant,
                    new.kind,
                    new.target_task_id.map(|t| t.0),
                    new.workspace_id.map(|w| w.0),
                    new.requested_by,
                    new.predecessor_job_id.map(|p| p.0),
                    new.seed,
                    new.review_pr_number,
                    new.review_head_sha,
                    new.build_fingerprint,
                    new.review_forced
                ],
            )
            .await?)
    }

    async fn review_run_heads(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
    ) -> ApiResult<Vec<RunHeads>> {
        // Two plain queries merged here rather than one clever one. A single
        // statement wanting "the newest row per group" reaches for `DISTINCT ON`
        // or `array_agg(... ORDER BY ...)`, both Postgres-only, and this file is
        // held to SQL both engines run.
        #[derive(nook_db::FromDbRow)]
        struct Head {
            review_pr_number: i64,
            review_head_sha: Option<String>,
        }

        #[derive(nook_db::FromDbRow)]
        struct FailedHead {
            review_pr_number: i64,
            review_head_sha: Option<String>,
            updated_at: chrono::DateTime<chrono::Utc>,
        }

        // At most one row per PR: the partial unique index from 0046 is what
        // makes that true, not an assumption here.
        let live: Vec<Head> = self
            .db
            .query_all(
                "SELECT review_pr_number, review_head_sha FROM loop_jobs
                  WHERE tenant_id = $1 AND workspace_id = $2 AND kind = 'review'
                    AND review_pr_number IS NOT NULL
                    AND state IN ('queued', 'claimed', 'running', 'waiting_on_human')",
                params![tenant, workspace.0],
            )
            .await?;

        // The newest run per PR that CONCLUDED NOTHING — `failed`, or
        // `completed` without a verdict (an early return exits zero like any
        // other pass) — so both are held rather than mistaken for a review or
        // retried hot.
        let attempted: Vec<FailedHead> = self
            .db
            .query_all(
                "SELECT review_pr_number, review_head_sha, updated_at FROM loop_jobs j
                  WHERE tenant_id = $1 AND workspace_id = $2 AND kind = 'review'
                    AND review_pr_number IS NOT NULL
                    AND (state = 'failed' OR (state = 'completed' AND review_verdict IS NULL))
                    AND updated_at = (
                        SELECT MAX(updated_at) FROM loop_jobs k
                         WHERE k.workspace_id = j.workspace_id
                           AND k.review_pr_number = j.review_pr_number
                           AND k.kind = 'review'
                           AND (k.state = 'failed'
                                OR (k.state = 'completed' AND k.review_verdict IS NULL)))",
                params![tenant, workspace.0],
            )
            .await?;

        // The newest FINISHED run per PR. `completed` only — a failed run has
        // reviewed nothing, so treating it as a head would let one failure
        // silence a PR until somebody pushed again.
        let done: Vec<Head> = self
            .db
            .query_all(
                "SELECT review_pr_number, review_head_sha FROM loop_jobs j
                  WHERE tenant_id = $1 AND workspace_id = $2 AND kind = 'review'
                    AND review_pr_number IS NOT NULL AND state = 'completed'
                    AND review_verdict IS NOT NULL
                    AND created_at = (
                        SELECT MAX(created_at) FROM loop_jobs k
                         WHERE k.workspace_id = j.workspace_id
                           AND k.review_pr_number = j.review_pr_number
                           AND k.kind = 'review' AND k.state = 'completed'
                           AND k.review_verdict IS NOT NULL)",
                params![tenant, workspace.0],
            )
            .await?;

        let mut by_pr: std::collections::HashMap<i64, RunHeads> = std::collections::HashMap::new();
        for h in live {
            by_pr
                .entry(h.review_pr_number)
                .or_insert_with(|| RunHeads {
                    item_key: h.review_pr_number,
                    live_head: None,
                    done_head: None,
                    attempted_head: None,
                    attempted_at: None,
                })
                .live_head = h.review_head_sha;
        }
        for h in done {
            by_pr
                .entry(h.review_pr_number)
                .or_insert_with(|| RunHeads {
                    item_key: h.review_pr_number,
                    live_head: None,
                    done_head: None,
                    attempted_head: None,
                    attempted_at: None,
                })
                .done_head = h.review_head_sha;
        }
        for h in attempted {
            let e = by_pr.entry(h.review_pr_number).or_insert_with(|| RunHeads {
                item_key: h.review_pr_number,
                live_head: None,
                done_head: None,
                attempted_head: None,
                attempted_at: None,
            });
            e.attempted_head = h.review_head_sha;
            e.attempted_at = Some(h.updated_at);
        }
        Ok(by_pr.into_values().collect())
    }

    async fn recorded_review_verdicts(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
    ) -> ApiResult<Vec<RecordedVerdict>> {
        // Newest per PR by `ORDER BY … LIMIT 1` rather than `MAX(created_at)`:
        // created_at has second resolution, so two runs in one second would
        // both match a MAX and the same PR would come back twice. The id is a
        // UUIDv7, time-ordered, which is what makes it an honest tiebreak.
        Ok(self
            .db
            .query_all(
                "SELECT review_pr_number, review_head_sha, review_verdict FROM loop_jobs j
                  WHERE tenant_id = $1 AND workspace_id = $2 AND kind = 'review'
                    AND review_pr_number IS NOT NULL AND review_head_sha IS NOT NULL
                    AND state = 'completed'
                    AND review_verdict IS NOT NULL AND review_verdict <> 'skipped'
                    AND j.id = (
                        SELECT k.id FROM loop_jobs k
                         WHERE k.tenant_id = j.tenant_id
                           AND k.workspace_id = j.workspace_id
                           AND k.review_pr_number = j.review_pr_number
                           AND k.kind = 'review' AND k.state = 'completed'
                           AND k.review_verdict IS NOT NULL AND k.review_verdict <> 'skipped'
                         ORDER BY k.created_at DESC, k.id DESC LIMIT 1)",
                params![tenant, workspace.0],
            )
            .await?)
    }

    async fn live_review_run_for(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
        pr: i64,
    ) -> ApiResult<Option<JobId>> {
        Ok(self
            .db
            .query_scalar_opt::<JobId>(
                "SELECT id FROM loop_jobs
                  WHERE tenant_id = $1 AND workspace_id = $2 AND kind = 'review'
                    AND review_pr_number = $3
                    AND state IN ('queued', 'claimed', 'running', 'waiting_on_human')",
                params![tenant, workspace.0, pr],
            )
            .await?)
    }

    async fn set_review_verdict(&self, id: JobId, verdict: &str) -> ApiResult<u64> {
        // Dialect-dispatched `now()` (MAIN-477): the hardcoded literal predated
        // the SQLite leg covering this path — `verdict_silence` is what covers
        // it now, and is the test that found the slip.
        Ok(self
            .db
            .exec(
                &format!(
                    "UPDATE loop_jobs SET review_verdict = $2, updated_at = {}
                  WHERE id = $1 AND kind = 'review'
                    AND state IN ('claimed', 'running', 'waiting_on_human')",
                    type_mapping(self.db.engine()).now()
                ),
                params![id.0, verdict],
            )
            .await?)
    }

    async fn record_conflict_rejection(&self, r: ConflictRejection) -> ApiResult<bool> {
        // The idempotence covers an AGENT's rejection too, not only an earlier
        // pass's: a head a reviewer already rejected is in the repair queue on
        // its own account, and a second row saying the same thing would only
        // compete to be the newest one read.
        let already: Option<JobId> = self
            .db
            .query_scalar_opt(
                "SELECT id FROM loop_jobs
                  WHERE tenant_id = $1 AND workspace_id = $2 AND kind = 'review'
                    AND review_pr_number = $3 AND review_head_sha = $4
                    AND review_verdict = 'changes_requested'",
                params![r.tenant, r.workspace.0, r.pr, r.head.clone()],
            )
            .await?;
        if already.is_some() {
            return Ok(false);
        }
        // `completed` with a verdict and no executor: a conclusion that nobody
        // ran, stated as one. That state is also what keeps the review side off
        // this head (it is a `done_head` to `review_run_heads`), so the REBASE
        // is what gets reviewed and the conflict never is.
        match self
            .db
            .exec(
                &format!(
                    "INSERT INTO loop_jobs
                        (id, tenant_id, kind, workspace_id, requested_by, state, seed,
                         review_pr_number, review_head_sha, review_verdict,
                         review_verdict_source, created_at, updated_at)
                     VALUES ($1, $2, 'review', $3, $4, 'completed', $5, $6, $7,
                             'changes_requested', $8, {now}, {now})",
                    now = type_mapping(self.db.engine()).now()
                ),
                params![
                    r.id,
                    r.tenant,
                    r.workspace.0,
                    r.requested_by,
                    r.seed,
                    r.pr,
                    r.head,
                    CONFLICT_VERDICT_SOURCE
                ],
            )
            .await
        {
            Ok(n) => Ok(n > 0),
            Err(e) if e.is_unique_violation() => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    async fn rejected_review_heads(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
    ) -> ApiResult<Vec<(i64, String)>> {
        #[derive(nook_db::FromDbRow)]
        struct Row {
            review_pr_number: i64,
            review_head_sha: String,
        }
        let rows: Vec<Row> = self
            .db
            .query_all(
                "SELECT j.review_pr_number, j.review_head_sha FROM loop_jobs j
                  WHERE j.tenant_id = $1 AND j.workspace_id = $2 AND j.kind = 'review'
                    AND j.review_verdict = 'changes_requested'
                    AND j.review_pr_number IS NOT NULL AND j.review_head_sha IS NOT NULL
                    AND j.id = (
                        SELECT k.id FROM loop_jobs k
                         WHERE k.workspace_id = j.workspace_id
                           AND k.review_pr_number = j.review_pr_number
                           AND k.kind = 'review'
                           AND k.review_verdict = 'changes_requested'
                         ORDER BY k.created_at DESC, k.id DESC LIMIT 1)",
                params![tenant, workspace.0],
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.review_pr_number, r.review_head_sha))
            .collect())
    }

    async fn set_build_outcome(&self, id: JobId, outcome: &str) -> ApiResult<u64> {
        // `type_mapping(...).now()`, not a literal `now()`: this runs on the
        // SQLite leg from day one, unlike `set_review_verdict` above, whose
        // hardcoded `now()` predates the leg covering this path.
        Ok(self
            .db
            .exec(
                &format!(
                    "UPDATE loop_jobs SET build_outcome = $2, updated_at = {}
                  WHERE id = $1 AND kind = 'build' AND build_outcome IS NULL
                    AND state IN ('claimed', 'running', 'waiting_on_human')",
                    type_mapping(self.db.engine()).now()
                ),
                params![id.0, outcome],
            )
            .await?)
    }

    async fn build_run_heads(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
    ) -> ApiResult<Vec<RunHeads>> {
        // `tasks.number` is INT4 on Postgres; decoding it straight into i64
        // dies with a ColumnDecode on the production engine (SQLite is
        // untyped enough not to notice). Decode the column's own width and
        // widen in Rust.
        #[derive(nook_db::FromDbRow)]
        struct Head {
            item_key: i32,
            fingerprint: Option<String>,
        }
        #[derive(nook_db::FromDbRow)]
        struct AttemptedHead {
            item_key: i32,
            fingerprint: Option<String>,
            updated_at: chrono::DateTime<chrono::Utc>,
        }
        // Fresh and repair are separate fingerprint SPACES with separate
        // bookkeeping: keyed apart (repair = the NEGATED card number, matching
        // `BuildWork`'s items), so a repair outcome can never overwrite the
        // record that the card's CONTENT was already built — the overwrite
        // that dragged an In-Review card back into a fresh build.
        let keyed = |raw: i32, fp: &Option<String>| -> i64 {
            let n = i64::from(raw);
            if fp.as_deref().is_some_and(|f| f.starts_with("repair:")) {
                -n
            } else {
                n
            }
        };

        // Keyed by the card's board number via the join — the number is what
        // `BuildWork` items carry, and 0050's per-card unique index is what
        // makes "at most one live row per key" true.
        let live: Vec<Head> = self
            .db
            .query_all(
                "SELECT t.number AS item_key, j.build_fingerprint AS fingerprint
                   FROM loop_jobs j JOIN tasks t ON t.id = j.target_task_id
                  WHERE j.tenant_id = $1 AND j.workspace_id = $2 AND j.kind = 'build'
                    AND j.state IN ('queued', 'claimed', 'running', 'waiting_on_human')",
                params![tenant, workspace.0],
            )
            .await?;

        let attempted: Vec<AttemptedHead> = self
            .db
            .query_all(
                "SELECT t.number AS item_key, j.build_fingerprint AS fingerprint, j.updated_at
                   FROM loop_jobs j JOIN tasks t ON t.id = j.target_task_id
                  WHERE j.tenant_id = $1 AND j.workspace_id = $2 AND j.kind = 'build'
                    AND (j.state IN ('failed', 'canceled')
                         OR (j.state = 'completed' AND j.build_outcome IS NULL))
                    AND j.updated_at = (
                        SELECT MAX(k.updated_at) FROM loop_jobs k
                         WHERE k.workspace_id = j.workspace_id
                           AND k.target_task_id = j.target_task_id
                           AND k.kind = 'build'
                           AND ((k.build_fingerprint LIKE 'repair:%')
                                = (j.build_fingerprint LIKE 'repair:%'))
                           AND (k.state IN ('failed', 'canceled')
                                OR (k.state = 'completed' AND k.build_outcome IS NULL)))",
                params![tenant, workspace.0],
            )
            .await?;

        let done: Vec<Head> = self
            .db
            .query_all(
                "SELECT t.number AS item_key, j.build_fingerprint AS fingerprint
                   FROM loop_jobs j JOIN tasks t ON t.id = j.target_task_id
                  WHERE j.tenant_id = $1 AND j.workspace_id = $2 AND j.kind = 'build'
                    AND j.state = 'completed' AND j.build_outcome IS NOT NULL
                    AND j.created_at = (
                        SELECT MAX(k.created_at) FROM loop_jobs k
                         WHERE k.workspace_id = j.workspace_id
                           AND k.target_task_id = j.target_task_id
                           AND k.kind = 'build' AND k.state = 'completed'
                           AND ((k.build_fingerprint LIKE 'repair:%')
                                = (j.build_fingerprint LIKE 'repair:%'))
                           AND k.build_outcome IS NOT NULL)",
                params![tenant, workspace.0],
            )
            .await?;

        let mut by_key: std::collections::HashMap<i64, RunHeads> = std::collections::HashMap::new();
        let entry = |m: &mut std::collections::HashMap<i64, RunHeads>, k: i64| {
            m.entry(k).or_insert_with(|| RunHeads {
                item_key: k,
                live_head: None,
                done_head: None,
                attempted_head: None,
                attempted_at: None,
            });
        };
        for h in live {
            let k = keyed(h.item_key, &h.fingerprint);
            entry(&mut by_key, k);
            by_key.get_mut(&k).unwrap().live_head = h.fingerprint;
        }
        for h in done {
            let k = keyed(h.item_key, &h.fingerprint);
            entry(&mut by_key, k);
            by_key.get_mut(&k).unwrap().done_head = h.fingerprint;
        }
        for h in attempted {
            let k = keyed(h.item_key, &h.fingerprint);
            entry(&mut by_key, k);
            let e = by_key.get_mut(&k).unwrap();
            e.attempted_head = h.fingerprint;
            e.attempted_at = Some(h.updated_at);
        }
        Ok(by_key.into_values().collect())
    }

    async fn active_epic_run_for(
        &self,
        tenant: TenantId,
        task: TaskId,
    ) -> ApiResult<Option<JobId>> {
        Ok(self
            .db
            .query_scalar_opt(
                "SELECT id FROM loop_jobs
                  WHERE tenant_id = $1 AND target_task_id = $2 AND kind = 'epic-run'
                    AND state IN ('queued', 'claimed', 'running', 'waiting_on_human')
                  ORDER BY created_at LIMIT 1",
                params![tenant, task],
            )
            .await?)
    }

    async fn list_reviews_for_workspace(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
        limit: i64,
    ) -> ApiResult<Vec<LoopJob>> {
        Ok(self
            .db
            .query_all(
                "SELECT * FROM loop_jobs
                  WHERE tenant_id = $1 AND workspace_id = $2 AND kind = 'review'
                  ORDER BY created_at DESC LIMIT $3",
                params![tenant, workspace.0, limit],
            )
            .await?)
    }

    async fn list_builds_for_workspace(
        &self,
        tenant: TenantId,
        viewer: nook_types::UserId,
        workspace: WorkspaceId,
        limit: i64,
    ) -> ApiResult<Vec<nook_types::WorkspaceBuildRun>> {
        // `||` and CAST are the concat/coercion BOTH engines run — this file is
        // held to SQL both engines run, like every query here. LEFT JOIN so a
        // run whose card was deleted still lists (key NULL), rather than
        // silently vanishing from the history it is part of. The CASE gates
        // the KEY on the shared visibility rule (MAIN-265): the run row is the
        // workspace's history, but a private card's identity is the owner's —
        // a non-owner sees the run keyless, the way MAIN-86 withholds a
        // private epic's key from its children.
        Ok(self
            .db
            .query_all(
                &format!(
                    "SELECT j.id, j.state,
                        CASE WHEN {vis}
                             THEN (b.key || '-' || CAST(t.number AS text))
                        END AS task_key,
                        j.queued_reason, j.queued_reason_kind,
                        j.created_at
                   FROM loop_jobs j
                   LEFT JOIN tasks t ON t.id = j.target_task_id
                   LEFT JOIN boards b ON b.id = t.board_id
                  WHERE j.tenant_id = $1 AND j.workspace_id = $2 AND j.kind = 'build'
                  ORDER BY j.created_at DESC LIMIT $3",
                    vis = crate::services::tasks::visible_sql("t", "$4"),
                ),
                params![tenant, workspace.0, limit, viewer],
            )
            .await?)
    }

    async fn live_build_states(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
    ) -> ApiResult<Vec<String>> {
        // The same four states `build_run_heads` calls live, spelled the same
        // way: what counts as in flight is one question with one answer.
        Ok(self
            .db
            .query_scalar_all(
                "SELECT state FROM loop_jobs
                  WHERE tenant_id = $1 AND workspace_id = $2 AND kind = 'build'
                    AND state IN ('queued', 'claimed', 'running', 'waiting_on_human')",
                params![tenant, workspace.0],
            )
            .await?)
    }

    async fn list_runs_for_workspace(
        &self,
        tenant: TenantId,
        viewer: nook_types::UserId,
        workspace: WorkspaceId,
        kind: Option<&str>,
        live_only: bool,
        limit: i64,
    ) -> ApiResult<Vec<nook_types::LoopRunSummary>> {
        // LEFT JOINs throughout: a run whose card was deleted, or which never
        // had one (a review run), or which nothing has claimed yet, is still
        // part of the history this lists. The filter is composed here so the
        // placeholder numbering and the bind order cannot drift apart.
        let mut sql = format!(
            "SELECT j.id, j.kind, j.state,
                    CASE WHEN {vis}
                         THEN (b.key || '-' || CAST(t.number AS text))
                    END AS task_key,
                    n.name AS executor_node,
                    j.created_at AS started_at,
                    j.updated_at
               FROM loop_jobs j
               LEFT JOIN tasks t ON t.id = j.target_task_id
               LEFT JOIN boards b ON b.id = t.board_id
               LEFT JOIN nodes n ON n.id = j.executor_node_id
              WHERE j.tenant_id = $1 AND j.workspace_id = $2",
            vis = crate::services::tasks::visible_sql("t", "$4"),
        );
        if live_only {
            sql.push_str(" AND j.state IN ('queued', 'claimed', 'running', 'waiting_on_human')");
        }
        if kind.is_some() {
            sql.push_str(" AND j.kind = $5");
        }
        sql.push_str(" ORDER BY j.created_at DESC, j.id DESC LIMIT $3");
        let mut binds = params![tenant, workspace.0, limit, viewer];
        if let Some(k) = kind {
            binds.extend(params![k]);
        }
        Ok(self.db.query_all(&sql, binds).await?)
    }

    async fn list_for_task(&self, tenant: TenantId, task: TaskId) -> ApiResult<Vec<LoopJob>> {
        Ok(self
            .db
            .query_all(
                "SELECT * FROM loop_jobs WHERE tenant_id = $1 AND target_task_id = $2
                 ORDER BY id DESC",
                params![tenant, task],
            )
            .await?)
    }

    async fn transition(&self, id: JobId, to: &str) -> ApiResult<LoopJob> {
        Ok(self
            .db
            .query_one(
                &format!(
                    "UPDATE loop_jobs SET state = $2, updated_at = {}
                     WHERE id = $1 RETURNING *",
                    type_mapping(self.db.engine()).now()
                ),
                params![id, to],
            )
            .await?)
    }

    async fn claim_for_executor(&self, id: JobId, node: NodeId) -> ApiResult<Option<LoopJob>> {
        Ok(self
            .db
            .query_opt(
                &format!(
                    "UPDATE loop_jobs
                     SET executor_node_id = $2, state = 'claimed', queued_reason = NULL,
                         queued_reason_kind = NULL, updated_at = {}
                     WHERE id = $1 AND state = 'queued'
                     RETURNING *",
                    type_mapping(self.db.engine()).now()
                ),
                params![id, node],
            )
            .await?)
    }

    async fn set_queued_reason(
        &self,
        id: JobId,
        reason: &str,
        kind: Option<QueuedReason>,
    ) -> ApiResult<u64> {
        // `queued_reason IS NULL OR <> $2` rather than `IS DISTINCT FROM`,
        // which SQLite only learned in 3.39 — the parameter is never NULL, so
        // the two are the same test here.
        //
        // The gate rides that same guard rather than widening it (MAIN-494):
        // sentence and gate are decided in ONE branch of the dispatcher, so an
        // unchanged sentence means an unchanged gate, and testing both would
        // only re-stamp `updated_at` — the clock the starvation rule reads.
        Ok(self
            .db
            .exec(
                &format!(
                    "UPDATE loop_jobs SET queued_reason = $2, queued_reason_kind = $3,
                            updated_at = {}
                     WHERE id = $1 AND state = 'queued'
                       AND (queued_reason IS NULL OR queued_reason <> $2)",
                    type_mapping(self.db.engine()).now()
                ),
                params![
                    id,
                    reason,
                    kind.map(nook_db::IntoDbValue::into_db_value)
                        .unwrap_or(nook_db::DbValue::Json(None))
                ],
            )
            .await?)
    }

    async fn reload(&self, id: JobId) -> ApiResult<LoopJob> {
        Ok(self
            .db
            .query_one("SELECT * FROM loop_jobs WHERE id = $1", params![id])
            .await?)
    }

    async fn tenant_and_target_of(
        &self,
        id: JobId,
    ) -> ApiResult<Option<(TenantId, Option<TaskId>)>> {
        Ok(self
            .db
            .query_opt(
                "SELECT tenant_id, target_task_id FROM loop_jobs WHERE id = $1",
                params![id],
            )
            .await?)
    }

    async fn executor_of(&self, tenant: TenantId, id: JobId) -> ApiResult<Option<Option<NodeId>>> {
        // `query_scalar_opt` collapses "no row" and "NULL column" into one
        // `None`, which this caller must tell apart — so select the row and
        // read the column.
        let row: Option<(Option<NodeId>,)> = self
            .db
            .query_opt(
                "SELECT executor_node_id FROM loop_jobs WHERE id = $1 AND tenant_id = $2",
                params![id, tenant],
            )
            .await?;
        Ok(row.map(|(node,)| node))
    }

    async fn in_flight_on_node(&self, node: NodeId) -> ApiResult<Vec<JobId>> {
        Ok(self
            .db
            .query_scalar_all(
                "SELECT id FROM loop_jobs
                 WHERE executor_node_id = $1 AND state IN ('claimed', 'running')",
                params![node],
            )
            .await?)
    }

    async fn queued_in_dispatch_order(&self, tenant: TenantId) -> ApiResult<Vec<JobId>> {
        Ok(self
            .db
            .query_scalar_all(
                // `LEFT JOIN`, because a review run targets a workspace and has
                // no card to read a priority from.
                "SELECT j.id FROM loop_jobs j
                 LEFT JOIN tasks t ON t.id = j.target_task_id
                 WHERE j.tenant_id = $1 AND j.state = 'queued'
                 ORDER BY CASE WHEN COALESCE(t.priority, 0) = 0 THEN 5
                               ELSE t.priority END,
                          j.created_at, j.id",
                params![tenant],
            )
            .await?)
    }

    async fn active_build_for(&self, tenant: TenantId, task: TaskId) -> ApiResult<Option<JobId>> {
        Ok(self
            .db
            .query_scalar_opt(
                "SELECT id FROM loop_jobs
                 WHERE tenant_id = $1 AND target_task_id = $2 AND kind = 'build'
                   AND state IN ('queued', 'claimed', 'running', 'waiting_on_human')
                 LIMIT 1",
                params![tenant, task],
            )
            .await?)
    }

    async fn reap_stale_executors(&self, grace_secs: i64) -> ApiResult<Vec<ReapedJob>> {
        let rows: Vec<(
            JobId,
            TenantId,
            Option<TaskId>,
            chrono::DateTime<chrono::Utc>,
        )> = self
            .db
            .query_all(
                &format!(
                    "UPDATE loop_jobs j
                        SET state = 'failed', updated_at = {now}
                       FROM nodes n
                      WHERE j.executor_node_id = n.id
                        AND j.state IN ('claimed', 'running')
                        AND n.last_seen_at IS NOT NULL
                        AND n.last_seen_at < {cutoff}
                    RETURNING j.id, j.tenant_id, j.target_task_id, n.last_seen_at",
                    now = type_mapping(self.db.engine()).now(),
                    cutoff = time_math(self.db.engine()).now_minus_scaled(
                        &type_mapping(self.db.engine()).cast("$1", "bigint"),
                        "1 second"
                    )
                ),
                params![grace_secs],
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(
                |(id, tenant, target_task_id, node_last_seen_at)| ReapedJob {
                    id,
                    tenant,
                    target_task_id,
                    node_last_seen_at,
                },
            )
            .collect())
    }

    async fn reap_stalled_jobs(&self, stall_secs: i64) -> ApiResult<Vec<StalledJob>> {
        // The silence test, spelled once and re-asserted as the UPDATE's guard.
        // `{n}` is the stall window's placeholder; `{j}` is how the job row is
        // named where the fragment lands (the outer table in the read, the
        // implicit target in the write).
        let silent = |j: &str, n: &str| {
            let cutoff = time_math(self.db.engine()).now_minus_scaled(
                &type_mapping(self.db.engine()).cast(n, "bigint"),
                "1 second",
            );
            format!(
                "{j}.updated_at < {cutoff}
                 AND COALESCE((SELECT MAX(t.at) FROM loop_job_transcript t
                                WHERE t.job_id = {j}.id), {j}.updated_at) < {cutoff}"
            )
        };
        let candidates: Vec<StalledCandidate> = self
            .db
            .query_all(
                &format!(
                    "SELECT j.id, j.tenant_id, j.target_task_id, j.updated_at,
                            (SELECT MAX(t.at) FROM loop_job_transcript t
                              WHERE t.job_id = j.id) AS last_entry_at
                       FROM loop_jobs j
                      WHERE j.state IN ('claimed', 'running') AND {}",
                    silent("j", "$1")
                ),
                params![stall_secs],
            )
            .await?;
        let mut stalled = Vec::new();
        for c in candidates {
            // Re-asserted, not assumed — the same exactly-once property the
            // executor claim has, so every replica may scan. A job that spoke
            // between the read and the write has its transcript (and, on a
            // transition, its `updated_at`) at now, falls out of this guard,
            // and keeps running.
            let failed = self
                .db
                .exec(
                    &format!(
                        "UPDATE loop_jobs SET state = 'failed', updated_at = {now}
                          WHERE id = $1 AND state IN ('claimed', 'running')
                            AND {}",
                        silent("loop_jobs", "$2"),
                        now = type_mapping(self.db.engine()).now(),
                    ),
                    params![c.id, stall_secs],
                )
                .await?
                > 0;
            if failed {
                stalled.push(c.into_stalled());
            }
        }
        Ok(stalled)
    }

    async fn cancel_queued_on_finished_cards(
        &self,
        tenant: TenantId,
    ) -> ApiResult<Vec<EndedQueuedJob>> {
        const FINISHED: &str = "target_task_id IN (
             SELECT t.id FROM tasks t JOIN board_columns c ON c.id = t.column_id
              WHERE c.type IN ('completed', 'canceled'))";
        let candidates: Vec<QueuedCandidate> = self
            .db
            .query_all(
                &format!("{QUEUED_CANDIDATE_COLS} AND {FINISHED}"),
                params![tenant],
            )
            .await?;
        let mut ended = Vec::new();
        for c in candidates {
            // The SAME predicate again, as the guard: two statements end a job
            // exactly as often as one did, because only the caller whose UPDATE
            // matches a still-queued, still-doomed row gets it back.
            if self.claim_ending(FINISHED, params![c.id]).await? {
                ended.push(c.into_ended());
            }
        }
        Ok(ended)
    }

    async fn cancel_starved_queued(
        &self,
        tenant: TenantId,
        starve_secs: i64,
    ) -> ApiResult<Vec<EndedQueuedJob>> {
        let cutoff = |placeholder: &str| {
            time_math(self.db.engine()).now_minus_scaled(
                &type_mapping(self.db.engine()).cast(placeholder, "bigint"),
                "1 second",
            )
        };
        let candidates: Vec<QueuedCandidate> = self
            .db
            .query_all(
                &format!(
                    "{QUEUED_CANDIDATE_COLS}
                       AND queued_reason IS NOT NULL AND updated_at < {}",
                    cutoff("$2")
                ),
                params![tenant, starve_secs],
            )
            .await?;
        let mut ended = Vec::new();
        for c in candidates {
            // Re-asserted, not assumed: a job whose reason moved between the
            // read and the write has its `updated_at` at now, falls out of this
            // guard, and keeps waiting — which is AC-6's negative holding even
            // in the gap between the two statements.
            let guard = format!(
                "queued_reason IS NOT NULL AND updated_at < {}",
                cutoff("$2")
            );
            if self
                .claim_ending(&guard, params![c.id, starve_secs])
                .await?
            {
                ended.push(c.into_ended());
            }
        }
        Ok(ended)
    }

    async fn transcript(&self, id: JobId) -> ApiResult<Vec<LoopJobTranscriptEntry>> {
        Ok(self
            .db
            .query_all(
                "SELECT * FROM loop_job_transcript WHERE job_id = $1 ORDER BY id",
                params![id],
            )
            .await?)
    }

    async fn append_transcript(
        &self,
        id: JobId,
        source: &str,
        content: &str,
    ) -> ApiResult<LoopJobTranscriptEntry> {
        Ok(self
            .db
            .query_one(
                "INSERT INTO loop_job_transcript (id, job_id, source, content)
                 VALUES ($1, $2, $3, $4) RETURNING *",
                params![JobTranscriptId::new(), id, source, content],
            )
            .await?)
    }
}

pub struct DbInteractionRepository {
    db: DbPool,
}

impl DbInteractionRepository {
    pub fn new(db: DbPool) -> Self {
        Self { db }
    }
}

#[async_trait]
impl InteractionRepository for DbInteractionRepository {
    async fn get(&self, tenant: TenantId, id: InteractionId) -> ApiResult<Option<Interaction>> {
        Ok(self
            .db
            .query_opt(
                "SELECT * FROM interactions WHERE id = $1 AND tenant_id = $2",
                params![id, tenant],
            )
            .await?)
    }

    async fn create(&self, new: NewInteraction) -> ApiResult<Interaction> {
        Ok(self
            .db
            .query_one(
                "INSERT INTO interactions
                    (id, tenant_id, job_id, task_id, prompt, choices,
                     requested_by_node_id, requested_by_session_id)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                 RETURNING *",
                params![
                    new.id,
                    new.tenant,
                    new.job_id.map(|x| x.0),
                    new.task_id.map(|x| x.0),
                    new.prompt,
                    new.choices,
                    new.requested_by_node_id.map(|x| x.0),
                    new.requested_by_session_id.map(|x| x.0)
                ],
            )
            .await?)
    }

    async fn list_pending(&self, tenant: TenantId) -> ApiResult<Vec<Interaction>> {
        Ok(self
            .db
            .query_all(
                "SELECT * FROM interactions
                 WHERE tenant_id = $1 AND state = 'pending'
                 ORDER BY created_at",
                params![tenant],
            )
            .await?)
    }

    async fn answer(
        &self,
        id: InteractionId,
        viewer: UserId,
        response: &str,
    ) -> ApiResult<Option<Interaction>> {
        Ok(self
            .db
            .query_opt(
                &format!(
                    "UPDATE interactions
                     SET state = 'answered', answered_by = $2, response = $3,
                         answered_at = {now}, updated_at = {now}
                     WHERE id = $1 AND state = 'pending'
                     RETURNING *",
                    now = type_mapping(self.db.engine()).now()
                ),
                params![id, viewer, response],
            )
            .await?)
    }

    async fn cancel(&self, id: InteractionId) -> ApiResult<Option<Interaction>> {
        Ok(self
            .db
            .query_opt(
                &format!(
                    "UPDATE interactions
                     SET state = 'canceled', updated_at = {}
                     WHERE id = $1 AND state = 'pending'
                     RETURNING *",
                    type_mapping(self.db.engine()).now()
                ),
                params![id],
            )
            .await?)
    }

    async fn cancel_for_job(&self, tenant: TenantId, job: JobId) -> ApiResult<Vec<Interaction>> {
        Ok(self
            .db
            .query_all::<Interaction>(
                &format!(
                    "UPDATE interactions
                     SET state = 'canceled', updated_at = {}
                     WHERE job_id = $1 AND tenant_id = $2 AND state = 'pending'
                     RETURNING *",
                    type_mapping(self.db.engine()).now()
                ),
                params![job, tenant],
            )
            .await?)
    }
}

// ── in-memory fakes (AC-3) ──────────────────────────────────────────────────
//
// Enough behavior that a caller test is worth trusting: tenant scoping, and the
// two `WHERE state = …` guards that make racing writers produce one winner
// rather than a silent overwrite.

use std::sync::Mutex;

#[derive(Default)]
struct FakeJobState {
    jobs: Vec<LoopJob>,
    transcript: Vec<LoopJobTranscriptEntry>,
    /// node → last_seen_at, so the reaper's staleness window can be tested
    /// without a `nodes` table.
    node_last_seen: Vec<(NodeId, chrono::DateTime<chrono::Utc>)>,
    /// Cards the fake should treat as sitting in a terminal column — the real
    /// query's `tasks`/`board_columns` join, which this repository cannot see.
    finished_cards: Vec<TaskId>,
    seq: i64,
}

/// The shared body of both queued-job endings: cancel every `queued` job the
/// predicate picks, reporting what was ended. Mirrors the real guarded
/// `UPDATE … WHERE state = 'queued' … RETURNING`.
fn cancel_queued(jobs: &mut [LoopJob], pick: impl Fn(&LoopJob) -> bool) -> Vec<EndedQueuedJob> {
    let mut out = Vec::new();
    for j in jobs.iter_mut() {
        if j.state != "queued" || !pick(j) {
            continue;
        }
        out.push(EndedQueuedJob {
            id: j.id,
            tenant: j.tenant_id,
            target_task_id: j.target_task_id,
            queued_reason: j.queued_reason.clone(),
            queued_since: j.created_at,
            // Read before the cancel's own write, as the real query's CTE does.
            reason_since: j.updated_at,
        });
        j.state = "canceled".into();
        j.updated_at = chrono::Utc::now();
    }
    out
}

#[derive(Default)]
pub struct FakeLoopJobRepository {
    inner: Mutex<FakeJobState>,
}

impl FakeLoopJobRepository {
    pub fn new() -> Self {
        Self::default()
    }

    /// Tell the fake when a node was last seen — the reaper joins `nodes` for
    /// exactly this.
    pub fn set_node_last_seen(&self, node: NodeId, at: chrono::DateTime<chrono::Utc>) {
        let mut s = self.inner.lock().unwrap();
        s.node_last_seen.retain(|(n, _)| *n != node);
        s.node_last_seen.push((node, at));
    }

    /// Tell the fake a card has reached a terminal column — the board join
    /// AC-1's cancel makes, which this repository has no tables for.
    pub fn set_card_finished(&self, task: TaskId) {
        let mut s = self.inner.lock().unwrap();
        if !s.finished_cards.contains(&task) {
            s.finished_cards.push(task);
        }
    }

    pub fn state_of(&self, id: JobId) -> Option<String> {
        self.inner
            .lock()
            .unwrap()
            .jobs
            .iter()
            .find(|j| j.id == id)
            .map(|j| j.state.clone())
    }

    pub fn queued_reason_of(&self, id: JobId) -> Option<Option<String>> {
        self.inner
            .lock()
            .unwrap()
            .jobs
            .iter()
            .find(|j| j.id == id)
            .map(|j| j.queued_reason.clone())
    }

    pub fn queued_reason_kind_of(&self, id: JobId) -> Option<Option<QueuedReason>> {
        self.inner
            .lock()
            .unwrap()
            .jobs
            .iter()
            .find(|j| j.id == id)
            .map(|j| j.queued_reason_kind.clone())
    }

    /// Force a state directly, bypassing the guards — so a test can set up the
    /// very state a guard exists to protect.
    pub fn force_state(&self, id: JobId, state: &str) {
        let mut s = self.inner.lock().unwrap();
        if let Some(j) = s.jobs.iter_mut().find(|j| j.id == id) {
            j.state = state.to_string();
        }
    }

    /// Push a job's clock back, so a scan that measures silence has something
    /// to measure — `set_node_last_seen`'s twin for job-level progress.
    pub fn force_updated_at(&self, id: JobId, at: chrono::DateTime<chrono::Utc>) {
        let mut s = self.inner.lock().unwrap();
        if let Some(j) = s.jobs.iter_mut().find(|j| j.id == id) {
            j.updated_at = at;
        }
    }
}

#[async_trait]
impl LoopJobRepository for FakeLoopJobRepository {
    async fn get(&self, tenant: TenantId, id: JobId) -> ApiResult<Option<LoopJob>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .jobs
            .iter()
            .find(|j| j.id == id && j.tenant_id == tenant)
            .cloned())
    }

    async fn target_task_of_unscoped(&self, id: JobId) -> ApiResult<Option<TaskId>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .jobs
            .iter()
            .find(|j| j.id == id)
            .and_then(|j| j.target_task_id))
    }

    async fn set_review_verdict(&self, id: JobId, verdict: &str) -> ApiResult<u64> {
        let mut s = self.inner.lock().unwrap();
        let Some(j) = s.jobs.iter_mut().find(|j| {
            j.id == id
                && j.kind == "review"
                && matches!(j.state.as_str(), "claimed" | "running" | "waiting_on_human")
        }) else {
            return Ok(0);
        };
        j.review_verdict = Some(verdict.to_string());
        Ok(1)
    }

    async fn set_build_outcome(&self, id: JobId, outcome: &str) -> ApiResult<u64> {
        let mut s = self.inner.lock().unwrap();
        let Some(j) = s.jobs.iter_mut().find(|j| {
            j.id == id
                && j.kind == "build"
                && j.build_outcome.is_none()
                && matches!(j.state.as_str(), "claimed" | "running" | "waiting_on_human")
        }) else {
            return Ok(0);
        };
        j.build_outcome = Some(outcome.to_string());
        Ok(1)
    }

    async fn record_conflict_rejection(&self, r: ConflictRejection) -> ApiResult<bool> {
        let mut s = self.inner.lock().unwrap();
        if s.jobs.iter().any(|j| {
            j.tenant_id == r.tenant
                && j.workspace_id == Some(r.workspace)
                && j.kind == "review"
                && j.review_pr_number == Some(r.pr)
                && j.review_head_sha.as_deref() == Some(r.head.as_str())
                && j.review_verdict.as_deref() == Some("changes_requested")
        }) {
            return Ok(false);
        }
        let now = chrono::Utc::now();
        s.jobs.push(LoopJob {
            id: r.id,
            tenant_id: r.tenant,
            kind: "review".into(),
            target_task_id: None,
            workspace_id: Some(r.workspace),
            requested_by: r.requested_by,
            state: "completed".into(),
            executor_node_id: None,
            predecessor_job_id: None,
            queued_reason: None,
            queued_reason_kind: None,
            seed: Some(r.seed),
            created_at: now,
            updated_at: now,
            review_pr_number: Some(r.pr),
            review_head_sha: Some(r.head),
            review_verdict: Some("changes_requested".into()),
            review_verdict_source: Some(CONFLICT_VERDICT_SOURCE.into()),
            review_forced: false,
            build_outcome: None,
            build_fingerprint: None,
        });
        Ok(true)
    }

    async fn rejected_review_heads(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
    ) -> ApiResult<Vec<(i64, String)>> {
        let s = self.inner.lock().unwrap();
        let mut newest: std::collections::HashMap<i64, (chrono::DateTime<chrono::Utc>, String)> =
            std::collections::HashMap::new();
        for j in s.jobs.iter().filter(|j| {
            j.tenant_id == tenant
                && j.workspace_id == Some(workspace)
                && j.kind == "review"
                && j.review_verdict.as_deref() == Some("changes_requested")
        }) {
            if let (Some(pr), Some(head)) = (j.review_pr_number, j.review_head_sha.as_ref()) {
                let e = newest.entry(pr).or_insert((j.created_at, head.clone()));
                if j.created_at > e.0 {
                    *e = (j.created_at, head.clone());
                }
            }
        }
        Ok(newest.into_iter().map(|(pr, (_, h))| (pr, h)).collect())
    }

    async fn build_run_heads(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
    ) -> ApiResult<Vec<RunHeads>> {
        // The fake has no tasks table to join a board number from; the tests
        // that need keyed heads drive the real repo. What the fake preserves
        // is the SHAPE the reconciler reads: one entry per targeted card,
        // keyed by a stable stand-in derived from the task id.
        let s = self.inner.lock().unwrap();
        let mut by_key: std::collections::HashMap<i64, RunHeads> = std::collections::HashMap::new();
        let key_of = |t: &TaskId| -> i64 { t.0.as_u64_pair().0 as i64 };
        for j in s.jobs.iter().filter(|j| {
            j.tenant_id == tenant && j.workspace_id == Some(workspace) && j.kind == "build"
        }) {
            let Some(task) = j.target_task_id.as_ref() else {
                continue;
            };
            let k = key_of(task);
            let e = by_key.entry(k).or_insert_with(|| RunHeads {
                item_key: k,
                live_head: None,
                done_head: None,
                attempted_head: None,
                attempted_at: None,
            });
            match j.state.as_str() {
                "queued" | "claimed" | "running" | "waiting_on_human" => {
                    e.live_head = j.build_fingerprint.clone()
                }
                "completed" if j.build_outcome.is_some() => {
                    e.done_head = j.build_fingerprint.clone()
                }
                "failed" | "canceled" | "completed" => {
                    e.attempted_head = j.build_fingerprint.clone();
                    e.attempted_at = Some(j.updated_at);
                }
                _ => {}
            }
        }
        Ok(by_key.into_values().collect())
    }

    async fn active_epic_run_for(
        &self,
        tenant: TenantId,
        task: TaskId,
    ) -> ApiResult<Option<JobId>> {
        let s = self.inner.lock().unwrap();
        Ok(s.jobs
            .iter()
            .filter(|j| {
                j.tenant_id == tenant
                    && j.target_task_id == Some(task)
                    && j.kind == "epic-run"
                    && matches!(
                        j.state.as_str(),
                        "queued" | "claimed" | "running" | "waiting_on_human"
                    )
            })
            .min_by_key(|j| j.created_at)
            .map(|j| j.id))
    }

    async fn list_reviews_for_workspace(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
        limit: i64,
    ) -> ApiResult<Vec<LoopJob>> {
        let s = self.inner.lock().unwrap();
        let mut mine: Vec<LoopJob> = s
            .jobs
            .iter()
            .filter(|j| {
                j.tenant_id == tenant && j.workspace_id == Some(workspace) && j.kind == "review"
            })
            .cloned()
            .collect();
        mine.sort_by_key(|j| std::cmp::Reverse(j.created_at));
        mine.truncate(limit.max(0) as usize);
        Ok(mine)
    }

    async fn list_builds_for_workspace(
        &self,
        tenant: TenantId,
        _viewer: nook_types::UserId,
        workspace: WorkspaceId,
        limit: i64,
    ) -> ApiResult<Vec<nook_types::WorkspaceBuildRun>> {
        let s = self.inner.lock().unwrap();
        let mut mine: Vec<nook_types::WorkspaceBuildRun> = s
            .jobs
            .iter()
            .filter(|j| {
                j.tenant_id == tenant && j.workspace_id == Some(workspace) && j.kind == "build"
            })
            .map(|j| nook_types::WorkspaceBuildRun {
                id: j.id.0,
                state: j.state.clone(),
                // The fake holds no boards to join; the key is the panel's
                // concern, and `None` is a legal row (a deleted card's run).
                task_key: None,
                queued_reason: j.queued_reason.clone(),
                queued_reason_kind: j.queued_reason_kind.clone(),
                created_at: j.created_at,
            })
            .collect();
        mine.sort_by_key(|r| std::cmp::Reverse(r.created_at));
        mine.truncate(limit.max(0) as usize);
        Ok(mine)
    }

    async fn live_build_states(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
    ) -> ApiResult<Vec<String>> {
        let s = self.inner.lock().unwrap();
        Ok(s.jobs
            .iter()
            .filter(|j| {
                j.tenant_id == tenant && j.workspace_id == Some(workspace) && j.kind == "build"
            })
            .filter(|j| {
                matches!(
                    j.state.as_str(),
                    "queued" | "claimed" | "running" | "waiting_on_human"
                )
            })
            .map(|j| j.state.clone())
            .collect())
    }

    async fn list_runs_for_workspace(
        &self,
        tenant: TenantId,
        _viewer: nook_types::UserId,
        workspace: WorkspaceId,
        kind: Option<&str>,
        live_only: bool,
        limit: i64,
    ) -> ApiResult<Vec<nook_types::LoopRunSummary>> {
        let s = self.inner.lock().unwrap();
        let mut mine: Vec<nook_types::LoopRunSummary> = s
            .jobs
            .iter()
            .filter(|j| j.tenant_id == tenant && j.workspace_id == Some(workspace))
            .filter(|j| kind.is_none_or(|k| j.kind == k))
            .filter(|j| {
                !live_only
                    || matches!(
                        j.state.as_str(),
                        "queued" | "claimed" | "running" | "waiting_on_human"
                    )
            })
            .map(|j| nook_types::LoopRunSummary {
                id: j.id,
                kind: j.kind.clone(),
                state: j.state.clone(),
                // The fake holds neither boards nor nodes to join against, and
                // both are legal `None`s on a real row (a review run, a job
                // nothing has claimed).
                task_key: None,
                executor_node: None,
                started_at: j.created_at,
                updated_at: j.updated_at,
                elapsed_seconds: 0,
            })
            .collect();
        mine.sort_by_key(|r| std::cmp::Reverse(r.started_at));
        mine.truncate(limit.max(0) as usize);
        Ok(mine)
    }

    async fn review_run_heads(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
    ) -> ApiResult<Vec<RunHeads>> {
        let s = self.inner.lock().unwrap();
        let mut by_pr: std::collections::HashMap<i64, RunHeads> = std::collections::HashMap::new();
        let mut mine: Vec<&LoopJob> = s
            .jobs
            .iter()
            .filter(|j| {
                j.tenant_id == tenant
                    && j.workspace_id == Some(workspace)
                    && j.kind == "review"
                    && j.review_pr_number.is_some()
            })
            .collect();
        // Oldest first, so the last write per PR is the newest run — the same
        // "newest wins" the SQL gets from MAX(created_at).
        mine.sort_by_key(|j| j.created_at);
        for j in mine {
            let pr = j.review_pr_number.unwrap();
            let e = by_pr.entry(pr).or_insert_with(|| RunHeads {
                item_key: pr,
                live_head: None,
                done_head: None,
                attempted_head: None,
                attempted_at: None,
            });
            match j.state.as_str() {
                "queued" | "claimed" | "running" | "waiting_on_human" => {
                    e.live_head = j.review_head_sha.clone()
                }
                // A completed run with no VERDICT reviewed nothing — a pass
                // that died politely still exits zero, and counting its head as
                // done is how a PR goes silently unreviewed until the next push.
                "completed" if j.review_verdict.is_some() => {
                    e.done_head = j.review_head_sha.clone()
                }
                "failed" => {
                    e.attempted_head = j.review_head_sha.clone();
                    e.attempted_at = Some(j.updated_at);
                }
                "completed" => {
                    // No verdict: the run concluded nothing, whatever its exit.
                    e.attempted_head = j.review_head_sha.clone();
                    e.attempted_at = Some(j.updated_at);
                }
                _ => {}
            }
        }
        Ok(by_pr.into_values().collect())
    }

    async fn recorded_review_verdicts(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
    ) -> ApiResult<Vec<RecordedVerdict>> {
        let s = self.inner.lock().unwrap();
        let mut newest: std::collections::HashMap<i64, &LoopJob> = std::collections::HashMap::new();
        for j in s.jobs.iter().filter(|j| {
            j.tenant_id == tenant
                && j.workspace_id == Some(workspace)
                && j.kind == "review"
                && j.state == "completed"
                && j.review_pr_number.is_some()
                && j.review_head_sha.is_some()
                && j.review_verdict.as_deref().is_some_and(|v| v != "skipped")
        }) {
            let e = newest.entry(j.review_pr_number.unwrap()).or_insert(j);
            // The SQL's tiebreak: created_at, then the time-ordered id.
            if (j.created_at, j.id.0) > (e.created_at, e.id.0) {
                *e = j;
            }
        }
        Ok(newest
            .into_values()
            .map(|j| RecordedVerdict {
                review_pr_number: j.review_pr_number.unwrap(),
                review_head_sha: j.review_head_sha.clone().unwrap(),
                review_verdict: j.review_verdict.clone().unwrap(),
            })
            .collect())
    }

    async fn live_review_run_for(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
        pr: i64,
    ) -> ApiResult<Option<JobId>> {
        let s = self.inner.lock().unwrap();
        Ok(s.jobs
            .iter()
            .find(|j| {
                j.tenant_id == tenant
                    && j.workspace_id == Some(workspace)
                    && j.kind == "review"
                    && j.review_pr_number == Some(pr)
                    && matches!(
                        j.state.as_str(),
                        "queued" | "claimed" | "running" | "waiting_on_human"
                    )
            })
            .map(|j| j.id))
    }

    async fn create(&self, new: NewLoopJob) -> ApiResult<LoopJob> {
        let now = chrono::Utc::now();
        let job = LoopJob {
            id: new.id,
            tenant_id: new.tenant,
            kind: new.kind,
            target_task_id: new.target_task_id,
            workspace_id: new.workspace_id,
            requested_by: new.requested_by,
            state: "queued".into(),
            executor_node_id: None,
            predecessor_job_id: new.predecessor_job_id,
            queued_reason: None,
            queued_reason_kind: None,
            seed: new.seed,
            created_at: now,
            updated_at: now,
            review_pr_number: None,
            review_head_sha: None,
            review_verdict: None,
            review_verdict_source: None,
            review_forced: new.review_forced,
            build_outcome: None,
            build_fingerprint: None,
        };
        self.inner.lock().unwrap().jobs.push(job.clone());
        Ok(job)
    }

    async fn list_for_task(&self, tenant: TenantId, task: TaskId) -> ApiResult<Vec<LoopJob>> {
        let s = self.inner.lock().unwrap();
        let mut out: Vec<LoopJob> = s
            .jobs
            .iter()
            .filter(|j| j.tenant_id == tenant && j.target_task_id == Some(task))
            .cloned()
            .collect();
        out.sort_by_key(|j| std::cmp::Reverse(j.id.0));
        Ok(out)
    }

    async fn transition(&self, id: JobId, to: &str) -> ApiResult<LoopJob> {
        let mut s = self.inner.lock().unwrap();
        let j = s
            .jobs
            .iter_mut()
            .find(|j| j.id == id)
            .ok_or(crate::error::ApiError::NotFound)?;
        j.state = to.to_string();
        j.updated_at = chrono::Utc::now();
        Ok(j.clone())
    }

    async fn claim_for_executor(&self, id: JobId, node: NodeId) -> ApiResult<Option<LoopJob>> {
        let mut s = self.inner.lock().unwrap();
        Ok(s.jobs
            .iter_mut()
            // `AND state = 'queued'`: exactly one of two racing dispatchers
            // flips the row; the other matches nothing.
            .find(|j| j.id == id && j.state == "queued")
            .map(|j| {
                j.executor_node_id = Some(node);
                j.state = "claimed".into();
                j.queued_reason = None;
                j.queued_reason_kind = None;
                j.updated_at = chrono::Utc::now();
                j.clone()
            }))
    }

    async fn set_queued_reason(
        &self,
        id: JobId,
        reason: &str,
        kind: Option<QueuedReason>,
    ) -> ApiResult<u64> {
        let mut s = self.inner.lock().unwrap();
        Ok(
            match s.jobs.iter_mut().find(|j| {
                j.id == id && j.state == "queued" && j.queued_reason.as_deref() != Some(reason)
            }) {
                Some(j) => {
                    j.queued_reason = Some(reason.to_string());
                    j.queued_reason_kind = kind;
                    j.updated_at = chrono::Utc::now();
                    1
                }
                None => 0,
            },
        )
    }

    async fn reload(&self, id: JobId) -> ApiResult<LoopJob> {
        self.inner
            .lock()
            .unwrap()
            .jobs
            .iter()
            .find(|j| j.id == id)
            .cloned()
            .ok_or(crate::error::ApiError::NotFound)
    }

    async fn tenant_and_target_of(
        &self,
        id: JobId,
    ) -> ApiResult<Option<(TenantId, Option<TaskId>)>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .jobs
            .iter()
            .find(|j| j.id == id)
            .map(|j| (j.tenant_id, j.target_task_id)))
    }

    async fn executor_of(&self, tenant: TenantId, id: JobId) -> ApiResult<Option<Option<NodeId>>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .jobs
            .iter()
            .find(|j| j.id == id && j.tenant_id == tenant)
            .map(|j| j.executor_node_id))
    }

    async fn in_flight_on_node(&self, node: NodeId) -> ApiResult<Vec<JobId>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .jobs
            .iter()
            .filter(|j| {
                j.executor_node_id == Some(node)
                    && matches!(j.state.as_str(), "claimed" | "running")
            })
            .map(|j| j.id)
            .collect())
    }

    async fn queued_in_dispatch_order(&self, tenant: TenantId) -> ApiResult<Vec<JobId>> {
        // No tasks table here, so every job reads as unset priority — the age
        // half of the rule is what the fake preserves. Tests about priority
        // drive the real repository.
        let s = self.inner.lock().unwrap();
        let mut queued: Vec<&LoopJob> = s
            .jobs
            .iter()
            .filter(|j| j.tenant_id == tenant && j.state == "queued")
            .collect();
        queued.sort_by_key(|j| (j.created_at, j.id.0));
        Ok(queued.into_iter().map(|j| j.id).collect())
    }

    async fn active_build_for(&self, tenant: TenantId, task: TaskId) -> ApiResult<Option<JobId>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .jobs
            .iter()
            .find(|j| {
                j.tenant_id == tenant
                    && j.target_task_id == Some(task)
                    && j.kind == "build"
                    && matches!(
                        j.state.as_str(),
                        "queued" | "claimed" | "running" | "waiting_on_human"
                    )
            })
            .map(|j| j.id))
    }

    async fn reap_stale_executors(&self, grace_secs: i64) -> ApiResult<Vec<ReapedJob>> {
        let mut s = self.inner.lock().unwrap();
        let cutoff = chrono::Utc::now() - chrono::Duration::seconds(grace_secs);
        let seen: Vec<(NodeId, chrono::DateTime<chrono::Utc>)> = s.node_last_seen.clone();
        let mut out = Vec::new();
        for j in s.jobs.iter_mut() {
            if !matches!(j.state.as_str(), "claimed" | "running") {
                continue;
            }
            let Some(node) = j.executor_node_id else {
                continue;
            };
            // The join is INNER and `last_seen_at IS NOT NULL`: a node that has
            // never reported does not strand its jobs here.
            let Some((_, last_seen)) = seen.iter().find(|(n, _)| *n == node) else {
                continue;
            };
            if *last_seen >= cutoff {
                continue;
            }
            j.state = "failed".into();
            j.updated_at = chrono::Utc::now();
            out.push(ReapedJob {
                id: j.id,
                tenant: j.tenant_id,
                target_task_id: j.target_task_id,
                node_last_seen_at: *last_seen,
            });
        }
        Ok(out)
    }

    async fn reap_stalled_jobs(&self, stall_secs: i64) -> ApiResult<Vec<StalledJob>> {
        let mut s = self.inner.lock().unwrap();
        let cutoff = chrono::Utc::now() - chrono::Duration::seconds(stall_secs);
        let last_entry: Vec<(JobId, chrono::DateTime<chrono::Utc>)> = s
            .transcript
            .iter()
            .map(|e| (e.job_id, e.at))
            .fold(Vec::new(), |mut acc, (job, at)| {
                match acc.iter_mut().find(|(j, _)| *j == job) {
                    Some((_, newest)) if *newest < at => *newest = at,
                    Some(_) => {}
                    None => acc.push((job, at)),
                }
                acc
            });
        let mut out = Vec::new();
        for j in s.jobs.iter_mut() {
            if !matches!(j.state.as_str(), "claimed" | "running") {
                continue;
            }
            let progress = last_entry
                .iter()
                .find(|(id, _)| *id == j.id)
                .map(|(_, at)| *at)
                .unwrap_or(j.updated_at)
                .max(j.updated_at);
            if progress >= cutoff {
                continue;
            }
            j.state = "failed".into();
            j.updated_at = chrono::Utc::now();
            out.push(StalledJob {
                id: j.id,
                tenant: j.tenant_id,
                target_task_id: j.target_task_id,
                last_progress_at: progress,
            });
        }
        Ok(out)
    }

    async fn cancel_queued_on_finished_cards(
        &self,
        tenant: TenantId,
    ) -> ApiResult<Vec<EndedQueuedJob>> {
        let mut s = self.inner.lock().unwrap();
        let finished = s.finished_cards.clone();
        Ok(cancel_queued(&mut s.jobs, |j| {
            j.tenant_id == tenant && j.target_task_id.is_some_and(|t| finished.contains(&t))
        }))
    }

    async fn cancel_starved_queued(
        &self,
        tenant: TenantId,
        starve_secs: i64,
    ) -> ApiResult<Vec<EndedQueuedJob>> {
        let mut s = self.inner.lock().unwrap();
        let cutoff = chrono::Utc::now() - chrono::Duration::seconds(starve_secs);
        Ok(cancel_queued(&mut s.jobs, |j| {
            j.tenant_id == tenant && j.queued_reason.is_some() && j.updated_at < cutoff
        }))
    }

    async fn transcript(&self, id: JobId) -> ApiResult<Vec<LoopJobTranscriptEntry>> {
        let s = self.inner.lock().unwrap();
        Ok(s.transcript
            .iter()
            .filter(|e| e.job_id == id)
            .cloned()
            .collect())
    }

    async fn append_transcript(
        &self,
        id: JobId,
        source: &str,
        content: &str,
    ) -> ApiResult<LoopJobTranscriptEntry> {
        let mut s = self.inner.lock().unwrap();
        s.seq += 1;
        let entry = LoopJobTranscriptEntry {
            id: JobTranscriptId::new(),
            job_id: id,
            source: source.to_string(),
            content: content.to_string(),
            at: chrono::Utc::now(),
        };
        s.transcript.push(entry.clone());
        Ok(entry)
    }
}

#[derive(Default)]
pub struct FakeInteractionRepository {
    inner: Mutex<Vec<Interaction>>,
}

impl FakeInteractionRepository {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn state_of(&self, id: InteractionId) -> Option<String> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .find(|i| i.id == id)
            .map(|i| i.state.clone())
    }
}

#[async_trait]
impl InteractionRepository for FakeInteractionRepository {
    async fn get(&self, tenant: TenantId, id: InteractionId) -> ApiResult<Option<Interaction>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .iter()
            .find(|i| i.id == id && i.tenant_id == tenant)
            .cloned())
    }

    async fn create(&self, new: NewInteraction) -> ApiResult<Interaction> {
        let now = chrono::Utc::now();
        let row = Interaction {
            id: new.id,
            tenant_id: new.tenant,
            job_id: new.job_id,
            task_id: new.task_id,
            prompt: new.prompt,
            choices: new.choices,
            state: "pending".into(),
            requested_by_node_id: new.requested_by_node_id,
            requested_by_session_id: new.requested_by_session_id,
            answered_by: None,
            response: None,
            created_at: now,
            updated_at: now,
            answered_at: None,
        };
        self.inner.lock().unwrap().push(row.clone());
        Ok(row)
    }

    async fn list_pending(&self, tenant: TenantId) -> ApiResult<Vec<Interaction>> {
        let s = self.inner.lock().unwrap();
        let mut out: Vec<Interaction> = s
            .iter()
            .filter(|i| i.tenant_id == tenant && i.state == "pending")
            .cloned()
            .collect();
        out.sort_by_key(|i| i.created_at);
        Ok(out)
    }

    async fn answer(
        &self,
        id: InteractionId,
        viewer: UserId,
        response: &str,
    ) -> ApiResult<Option<Interaction>> {
        let mut s = self.inner.lock().unwrap();
        Ok(s.iter_mut()
            // `AND state = 'pending'`: the second of two racing answers matches
            // nothing rather than overwriting the first.
            .find(|i| i.id == id && i.state == "pending")
            .map(|i| {
                let now = chrono::Utc::now();
                i.state = "answered".into();
                i.answered_by = Some(viewer);
                i.response = Some(response.to_string());
                i.answered_at = Some(now);
                i.updated_at = now;
                i.clone()
            }))
    }

    async fn cancel(&self, id: InteractionId) -> ApiResult<Option<Interaction>> {
        let mut s = self.inner.lock().unwrap();
        Ok(s.iter_mut()
            .find(|i| i.id == id && i.state == "pending")
            .map(|i| {
                i.state = "canceled".into();
                i.updated_at = chrono::Utc::now();
                i.clone()
            }))
    }

    async fn cancel_for_job(&self, tenant: TenantId, job: JobId) -> ApiResult<Vec<Interaction>> {
        let mut s = self.inner.lock().unwrap();
        let mut out = Vec::new();
        for i in s.iter_mut() {
            if i.tenant_id == tenant && i.job_id == Some(job) && i.state == "pending" {
                i.state = "canceled".into();
                i.updated_at = chrono::Utc::now();
                out.push(i.clone());
            }
        }
        Ok(out)
    }
}
