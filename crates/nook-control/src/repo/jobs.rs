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
    async fn build_run_heads(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
    ) -> ApiResult<Vec<RunHeads>>;

    /// Record what a build run concluded. Guarded on a live build run — an
    /// outcome on a finished or foreign job is a caller bug, answered with 0.
    async fn set_build_outcome(&self, id: JobId, outcome: &str) -> ApiResult<u64>;

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

    /// Record what a review run concluded. Guarded on a live review run — a
    /// verdict on a finished or foreign job is a caller bug, answered with 0.
    async fn set_review_verdict(&self, id: JobId, verdict: &str) -> ApiResult<u64>;

    /// The live epic-run for this epic, if one is in flight (MAIN-144 AC-3) —
    /// the dedupe that keeps "one deliberate enqueue per pass" true.
    async fn active_epic_run_for(&self, tenant: TenantId, task: TaskId)
        -> ApiResult<Option<JobId>>;

    async fn list_for_task(&self, tenant: TenantId, task: TaskId) -> ApiResult<Vec<LoopJob>>;

    async fn transition(&self, id: JobId, to: &str) -> ApiResult<LoopJob>;

    /// Place a job on a node — but only from `queued`, so of two racing
    /// dispatchers exactly one wins and the loser sees `None`.
    async fn claim_for_executor(&self, id: JobId, node: NodeId) -> ApiResult<Option<LoopJob>>;

    /// Explain why a job is still queued. Guarded on `queued` so a job that got
    /// placed in the meantime is not annotated with a stale excuse.
    async fn set_queued_reason(&self, id: JobId, reason: &str) -> ApiResult<u64>;

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

    /// The live build run already on a card, if any (MAIN-383 AC-4) — what the
    /// create path names in its refusal. The 0050 partial unique index is the
    /// atomic version of the same rule.
    async fn active_build_for(&self, tenant: TenantId, task: TaskId) -> ApiResult<Option<JobId>>;

    /// Fail every job whose executor stopped reporting more than `grace_secs`
    /// ago, returning what was reaped. One guarded `UPDATE … RETURNING`, so two
    /// reapers cannot double-fail a job and a job that resumed between scan and
    /// update falls out of the guard untouched.
    async fn reap_stale_executors(&self, grace_secs: i64) -> ApiResult<Vec<ReapedJob>>;

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

impl DbLoopJobRepository {
    pub fn new(db: DbPool) -> Self {
        Self { db }
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
                     state, predecessor_job_id, seed, review_pr_number, review_head_sha, build_fingerprint)
                 VALUES ($1, $2, $3, $4, $5, $6, 'queued', $7, $8, $9, $10, $11)
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
                    new.build_fingerprint
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

    async fn set_review_verdict(&self, id: JobId, verdict: &str) -> ApiResult<u64> {
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
                    AND (j.state = 'failed' OR (j.state = 'completed' AND j.build_outcome IS NULL))
                    AND j.updated_at = (
                        SELECT MAX(k.updated_at) FROM loop_jobs k
                         WHERE k.workspace_id = j.workspace_id
                           AND k.target_task_id = j.target_task_id
                           AND k.kind = 'build'
                           AND ((k.build_fingerprint LIKE 'repair:%')
                                = (j.build_fingerprint LIKE 'repair:%'))
                           AND (k.state = 'failed'
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
                         updated_at = {}
                     WHERE id = $1 AND state = 'queued'
                     RETURNING *",
                    type_mapping(self.db.engine()).now()
                ),
                params![id, node],
            )
            .await?)
    }

    async fn set_queued_reason(&self, id: JobId, reason: &str) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                &format!(
                    "UPDATE loop_jobs SET queued_reason = $2, updated_at = {}
                     WHERE id = $1 AND state = 'queued'",
                    type_mapping(self.db.engine()).now()
                ),
                params![id, reason],
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
    seq: i64,
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

    /// Force a state directly, bypassing the guards — so a test can set up the
    /// very state a guard exists to protect.
    pub fn force_state(&self, id: JobId, state: &str) {
        let mut s = self.inner.lock().unwrap();
        if let Some(j) = s.jobs.iter_mut().find(|j| j.id == id) {
            j.state = state.to_string();
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
                "failed" | "completed" => {
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
                created_at: j.created_at,
            })
            .collect();
        mine.sort_by_key(|r| std::cmp::Reverse(r.created_at));
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
            seed: new.seed,
            created_at: now,
            updated_at: now,
            review_pr_number: None,
            review_head_sha: None,
            review_verdict: None,
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
                j.updated_at = chrono::Utc::now();
                j.clone()
            }))
    }

    async fn set_queued_reason(&self, id: JobId, reason: &str) -> ApiResult<u64> {
        let mut s = self.inner.lock().unwrap();
        Ok(
            match s
                .jobs
                .iter_mut()
                .find(|j| j.id == id && j.state == "queued")
            {
                Some(j) => {
                    j.queued_reason = Some(reason.to_string());
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
