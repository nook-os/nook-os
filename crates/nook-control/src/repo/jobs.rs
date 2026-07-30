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
use nook_db::{params, Db, DbPool, Postgres, TimeMath, TypeMapping};
use nook_types::*;

use crate::error::ApiResult;

/// A job to enqueue.
#[derive(Debug, Clone)]
pub struct NewLoopJob {
    pub id: JobId,
    pub tenant: TenantId,
    pub kind: String,
    pub target_task_id: TaskId,
    pub workspace_id: Option<WorkspaceId>,
    pub requested_by: UserId,
    pub seed: Option<String>,
    /// Set only by a re-run, which records what it descends from.
    pub predecessor_job_id: Option<JobId>,
}

/// A job the reaper found stranded on a node that stopped reporting, with the
/// moment that node was last seen — the transcript line quotes it.
#[derive(Debug, Clone)]
pub struct ReapedJob {
    pub id: JobId,
    pub tenant: TenantId,
    pub target_task_id: TaskId,
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
    async fn target_task_of_unscoped(&self, id: JobId) -> ApiResult<Option<TaskId>>;

    async fn create(&self, new: NewLoopJob) -> ApiResult<LoopJob>;

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
    /// missing job means no nudge, never a failed append.
    async fn tenant_and_target_of(&self, id: JobId) -> ApiResult<Option<(TenantId, TaskId)>>;

    /// Which node a job was placed on. The outer `Option` is "no such job";
    /// the inner is "not placed yet".
    async fn executor_of(&self, tenant: TenantId, id: JobId) -> ApiResult<Option<Option<NodeId>>>;

    /// Jobs still believed to be running on a node — what a disconnect strands.
    async fn in_flight_on_node(&self, node: NodeId) -> ApiResult<Vec<JobId>>;

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
        Ok(self
            .db
            .query_scalar_opt::<TaskId>(
                "SELECT target_task_id FROM loop_jobs WHERE id = $1",
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
                     state, predecessor_job_id, seed)
                 VALUES ($1, $2, $3, $4, $5, $6, 'queued', $7, $8)
                 RETURNING *",
                params![
                    new.id,
                    new.tenant,
                    new.kind,
                    new.target_task_id,
                    new.workspace_id.map(|w| w.0),
                    new.requested_by,
                    new.predecessor_job_id.map(|p| p.0),
                    new.seed
                ],
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
                    Postgres.now()
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
                    Postgres.now()
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
                    Postgres.now()
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

    async fn tenant_and_target_of(&self, id: JobId) -> ApiResult<Option<(TenantId, TaskId)>> {
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

    async fn reap_stale_executors(&self, grace_secs: i64) -> ApiResult<Vec<ReapedJob>> {
        let rows: Vec<(JobId, TenantId, TaskId, chrono::DateTime<chrono::Utc>)> = self
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
                    now = Postgres.now(),
                    cutoff = Postgres.now_minus_scaled("$1::bigint", "1 second")
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
                    now = Postgres.now()
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
                    Postgres.now()
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
                    Postgres.now()
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
            .map(|j| j.target_task_id))
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
        };
        self.inner.lock().unwrap().jobs.push(job.clone());
        Ok(job)
    }

    async fn list_for_task(&self, tenant: TenantId, task: TaskId) -> ApiResult<Vec<LoopJob>> {
        let s = self.inner.lock().unwrap();
        let mut out: Vec<LoopJob> = s
            .jobs
            .iter()
            .filter(|j| j.tenant_id == tenant && j.target_task_id == task)
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

    async fn tenant_and_target_of(&self, id: JobId) -> ApiResult<Option<(TenantId, TaskId)>> {
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
