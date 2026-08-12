//! Task and board data access (MAIN-248).
//!
//! Everything the kanban surface reads or writes about **boards, columns,
//! tasks, labels and task comments** lives behind [`TaskRepository`]. Before
//! this the same forty-odd queries were inlined across `services/kanban.rs`,
//! `services/taskwork.rs` and `services/tasks.rs`.
//!
//! **Methods are intent-named and coarse.** No `query(sql)` escape, no `sqlx`
//! type in any signature. Where the old code leaned on a driver detail the trait
//! states the intent instead — [`TaskRepository::update_fields`] returns
//! `Ok(None)` for "no row matched", leaving the caller to decide whether that
//! was a lost optimistic-concurrency race or a missing task, exactly as before.
//!
//! **The create transaction is one method.** `create_task` allocates the board
//! number under `FOR UPDATE`, inserts the task, and attaches its labels — all in
//! a single transaction, because the lock is what makes `NOOK-7` get allocated
//! exactly once and the shared transaction is what stops the pick query seeing a
//! task for a moment without the labels it was filed with. Splitting that across
//! three trait calls would reintroduce both races, so [`TaskRepository::create_task`]
//! takes the whole intent and owns the transaction (AC-1's "multi-table writes
//! stay inside one method").
//!
//! **One impl over the engine-agnostic `DbPool`** ([`DbTaskRepository`]), row
//! mapping inside it, no per-engine branch and no dialect dispatch — that layer
//! is underneath us (NG-1), and a per-engine impl is a later, hotspot-proven
//! escape hatch (NG-3). The `type_mapping(self.db.engine()).now()` / `.cast()` calls came in with the
//! moved SQL unchanged; replacing them is the dialect sweep's job.

use async_trait::async_trait;
use nook_db::dialect::{atomic_claim, ci_match, time_math, type_mapping};
use nook_db::{params, Db, DbPool, DbValue};
use nook_types::*;
use std::collections::HashMap;
use uuid::Uuid;

use crate::error::ApiResult;

/// A parent task's key-building fields plus the columns that decide whether the
/// viewer may see it at all — enough to build `BOARD-N` and to redact it.
pub type ParentInfo = (Option<i32>, String, Option<UserId>, Option<UserId>);

/// One label row as stored, with the task it hangs off.
pub type TaskLabelRow = (
    Uuid,
    Uuid,
    TenantId,
    String,
    String,
    chrono::DateTime<chrono::Utc>,
);

/// What a new task is made from. Coarse on purpose: the whole intent arrives in
/// one value so the create transaction has everything it needs.
#[derive(Debug, Clone)]
pub struct NewTask {
    pub tenant: TenantId,
    pub board: BoardId,
    pub column_id: ColumnId,
    pub title: String,
    pub description: Option<String>,
    pub position: i32,
    pub workspace_id: Option<Uuid>,
    pub priority: i32,
    pub type_: String,
    pub visibility: String,
    pub created_by: Option<Uuid>,
    pub parent_task_id: Option<Uuid>,
    /// Lower-cased, non-empty names. Created if new, attached inside the same
    /// transaction as the task.
    pub labels: Vec<String>,
}

/// The tri-state edit `update_task` performs. `None` leaves a field alone;
/// the two `set_*` flags carry "the caller mentioned this field", which is how a
/// deliberate *clear* is told apart from an omission — `COALESCE` cannot express
/// that, which is why those two are flags rather than options.
#[derive(Debug, Clone, Default)]
pub struct TaskEdit {
    pub title: Option<String>,
    pub description: Option<String>,
    pub column_id: Option<Uuid>,
    pub position: Option<i32>,
    pub assignee_user_id: Option<Uuid>,
    pub priority: Option<i32>,
    pub set_workspace: bool,
    pub workspace_id: Option<Uuid>,
    /// The optimistic-concurrency precondition (MAIN-36). `None` is unguarded;
    /// otherwise the row updates only while `updated_at` still equals this.
    pub expected_updated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub type_: Option<String>,
    pub visibility: Option<String>,
    pub set_parent: bool,
    pub parent_task_id: Option<Uuid>,
}

/// The fields `start_work` stamps on a task once its worktree and session exist.
#[derive(Debug, Clone)]
pub struct StartedWork {
    pub workspace_id: Uuid,
    pub node_id: NodeId,
    pub branch: String,
    pub worktree_path: String,
    pub session_id: Option<Uuid>,
    pub column_id: ColumnId,
    /// Bound as a bare uuid, exactly as the call site did — the checkout may
    /// legitimately be absent.
    pub checkout_id: Option<Uuid>,
    /// The lease this claim carries (MAIN-229), in seconds from now.
    pub claim_ttl_secs: i64,
}

/// One card the claim reaper requeued, with the evidence that its worker was
/// gone — enough to write the card comment without a second read.
#[derive(Debug, Clone)]
pub struct LapsedClaim {
    pub task: TaskId,
    pub tenant: TenantId,
    pub session: SessionId,
    pub session_status: String,
    pub node_last_seen_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// One card whose claim ran past `max_claim_secs` while its worker still looked
/// alive — escalated to a human, never moved.
#[derive(Debug, Clone)]
pub struct CappedClaim {
    pub task: TaskId,
    pub tenant: TenantId,
    pub claim_expires_at: chrono::DateTime<chrono::Utc>,
}

/// Everything the pick query filters on, already validated and resolved by the
/// caller. The route keeps the 400s (an unknown column type, an unresolvable
/// parent) because those are policy; this carries only what the SQL binds.
#[derive(Debug, Clone, Default)]
pub struct PickParams {
    pub board: Option<String>,
    pub workspace: Option<Uuid>,
    pub column_type: Option<String>,
    pub priority: Option<i32>,
    pub unassigned_only: bool,
    pub assignee: Option<Uuid>,
    pub labels: Vec<String>,
    pub not_labels: Vec<String>,
    pub is_blocked: Option<bool>,
    pub created_after: Option<chrono::DateTime<chrono::Utc>>,
    pub limit: i64,
    pub archived: bool,
    /// Already wrapped in `%…%` by the caller; `None` disables the clause.
    pub q: Option<String>,
    pub types: Vec<String>,
    pub parent: Option<Uuid>,
    pub backlog: bool,
    /// Include tasks in a `completed`/`canceled` column (MAIN-464). Off by
    /// default for the same reason `backlog` is: a finished card is not work.
    pub done: bool,
    pub visibility: Vec<String>,
    /// The node asking. A card DISPATCHED to a machine is that machine's to
    /// take; an undispatched card is anybody's. `None` disables the clause, so
    /// a human's `nook tasks` still sees the whole board.
    pub node: Option<Uuid>,
}

/// What a new task comment is made from.
#[derive(Debug, Clone)]
pub struct NewComment {
    pub tenant: TenantId,
    pub task: TaskId,
    pub author_type: String,
    pub author_id: Option<Uuid>,
    pub author_name: String,
    pub body_md: String,
}

#[async_trait]
pub trait TaskRepository: Send + Sync {
    // ---- enrichment reads (batched; an N+1 here is a board render) ---------

    /// Board keys for a set of boards, one row per board rather than per task.
    async fn board_keys(&self, board_ids: &[Uuid]) -> ApiResult<HashMap<Uuid, Option<String>>>;

    /// Every label attached to any of these tasks, ordered by label name.
    async fn labels_for_tasks(&self, task_ids: &[Uuid]) -> ApiResult<Vec<TaskLabelRow>>;

    /// Recent task titles a deployment operator may be shown, newest first,
    /// capped.
    ///
    /// A `private` task is never included, even when the org has switched the
    /// policy on (MAIN-76 AC-4). The exclusion lives in the projection because
    /// the policy is ADDITIVE — it adds titles, it does not filter them — so a
    /// private card must simply never be selected here. A filter applied by the
    /// caller instead would fail open the first time somebody forgot it.
    async fn operator_visible_titles(&self, tenant: TenantId, limit: i64)
        -> ApiResult<Vec<String>>;

    /// Parent number + visibility columns for a set of parents, one query for
    /// the whole batch.
    async fn parent_info(&self, parent_ids: &[Uuid]) -> ApiResult<HashMap<Uuid, ParentInfo>>;

    // ---- resolution --------------------------------------------------------

    /// A task id, confirmed to live in this tenant. A uuid is not an
    /// authorisation, so this is tenant-scoped like the key lookup.
    async fn id_by_uuid(&self, tenant: TenantId, id: Uuid) -> ApiResult<Option<TaskId>>;

    /// A task id from its human key — `("NOOK", 42)`, board key matched
    /// case-insensitively.
    async fn id_by_key(
        &self,
        tenant: TenantId,
        board_key: &str,
        number: i32,
    ) -> ApiResult<Option<TaskId>>;

    // ---- columns -----------------------------------------------------------

    /// The column of `column_type` on this board, lowest position winning when
    /// a board has two of a type.
    async fn column_of_type(
        &self,
        board: BoardId,
        column_type: &str,
    ) -> ApiResult<Option<ColumnId>>;

    /// A column by name, case-insensitively.
    async fn column_by_name(&self, board: BoardId, name: &str) -> ApiResult<Option<ColumnId>>;

    /// The Nth column by position, for the positional fallback.
    async fn column_at_position(&self, board: BoardId, offset: i64) -> ApiResult<Option<ColumnId>>;

    /// The board's first column by position.
    async fn first_column(&self, board: BoardId) -> ApiResult<Option<ColumnId>>;

    // ---- boards ------------------------------------------------------------

    async fn list_local_boards(&self, tenant: TenantId) -> ApiResult<Vec<Board>>;
    async fn get_board(&self, tenant: TenantId, board: BoardId) -> ApiResult<Option<Board>>;
    async fn board_columns(&self, board: BoardId) -> ApiResult<Vec<BoardColumn>>;
    async fn board_tasks(&self, board: BoardId) -> ApiResult<Vec<TaskItem>>;

    /// The `KanbanProvider` name that owns a task, via its board.
    async fn board_provider_for_task(
        &self,
        tenant: TenantId,
        task_id: TaskId,
    ) -> ApiResult<Option<String>>;

    // ---- task reads --------------------------------------------------------

    async fn get_row(&self, tenant: TenantId, id: TaskId) -> ApiResult<Option<TaskItem>>;

    /// `(type, board, parent)` — the three fields `update_task` needs before it
    /// can decide anything. One read, so the column-type resolution, the
    /// epic-retype guard and the parent validation cannot disagree.
    async fn task_shape(
        &self,
        tenant: TenantId,
        id: TaskId,
    ) -> ApiResult<Option<(String, BoardId, Option<TaskId>)>>;

    /// How many tasks hang off this one — the epic-retype guard names the count.
    async fn count_children(&self, id: TaskId) -> ApiResult<i64>;

    /// The highest `position` in a column, for appending.
    async fn max_position_in_column(&self, column: ColumnId) -> ApiResult<Option<i32>>;

    /// This workspace's cards that carry a recorded PR — `(id, number, pr_url)`
    /// — the candidate set for REPAIR items (MAIN-458 AC-1b).
    ///
    /// Narrowed by the same two exclusions `pick_tasks` applies to the FRESH
    /// side, because they are properties of the card rather than of how the
    /// work was sourced (MAIN-496 AC-3): a card in a `completed`/`canceled`
    /// column is finished, and a `blocked` card is one a human has been asked
    /// about. Without them the two queued-job endings feed themselves — the
    /// reaper cancels a repair run, this query hands the same item straight
    /// back, and the cycle repeats forever on a card nobody is being helped by.
    async fn tasks_with_pr(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
    ) -> ApiResult<Vec<(TaskId, i64, String)>>;

    /// The same cards, narrowed to those still IN FLIGHT — in a column whose
    /// type is neither `completed` nor `canceled` — as `(id, pr_url)`.
    ///
    /// The narrowing is MAIN-491's NG-5 expressed as a query rather than as a
    /// filter every caller has to remember: a card that already reached Done is
    /// outside the merge sweep's candidate set, so no forge call is even spent
    /// asking about it. That also bounds the per-pass cost to open work rather
    /// than to every card the workspace has ever shipped.
    async fn in_flight_tasks_with_pr(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
    ) -> ApiResult<Vec<(TaskId, String)>>;

    // ---- task writes -------------------------------------------------------

    /// Allocate the board number, insert the task, and attach its labels — in
    /// **one transaction**. See the module docs for why this is a single method.
    async fn create_task(&self, new: NewTask) -> ApiResult<TaskItem>;

    /// The tri-state edit. `Ok(None)` means no row matched: the caller decides
    /// whether that is a lost race or a missing task, which is exactly the
    /// distinction it made before.
    async fn update_fields(
        &self,
        tenant: TenantId,
        id: TaskId,
        edit: TaskEdit,
    ) -> ApiResult<Option<TaskItem>>;

    async fn clear_assignee(&self, tenant: TenantId, id: TaskId) -> ApiResult<TaskItem>;
    async fn set_priority(
        &self,
        tenant: TenantId,
        id: TaskId,
        priority: i32,
    ) -> ApiResult<TaskItem>;

    /// Triage → Todo: place the work on a node and move it.
    /// Pin a task to a machine, and OPTIONALLY move it.
    ///
    /// `column: None` is dispatch's case. It used to be mandatory, so
    /// dispatching always relocated the card to Todo — which quietly pulled an
    /// urgent ticket out of In Review on a mis-click, and had nothing to do with
    /// choosing a machine. Placement and position are separate decisions now.
    async fn assign_node_and_column(
        &self,
        id: TaskId,
        node: NodeId,
        column: Option<ColumnId>,
    ) -> ApiResult<TaskItem>;

    /// Stamp the worktree, branch, session and column a started task now has.
    async fn record_started_work(&self, id: TaskId, work: StartedWork) -> ApiResult<TaskItem>;

    async fn set_pr_url(&self, id: TaskId, url: &str, column: ColumnId) -> ApiResult<TaskItem>;

    /// Record `url` on a card that has NO recorded PR and is still in flight,
    /// reporting whether THIS call wrote it (MAIN-491 AC-2's self-heal).
    ///
    /// Guarded rather than unconditional on both counts. `pr_url IS NULL` keeps
    /// the two joins in the order the contract states — a recorded PR is the
    /// card's own answer and a body scan never overrides it — and makes the
    /// write the exactly-once fence across replicas. The in-flight test is
    /// NG-5: a card that already reached Done is not touched, not even to
    /// stamp a URL on it.
    async fn backfill_pr_url(&self, tenant: TenantId, id: TaskId, url: &str) -> ApiResult<bool>;

    /// Move a card to `column` — clearing its claim lease — only while it is
    /// still in a non-completed column. `None` means it was not, so nothing was
    /// written (MAIN-491 AC-3/AC-8).
    ///
    /// The guard is what makes the sweep safe on every replica AND idempotent
    /// across passes: the first update matches and every later one does not, so
    /// "did I move it?" and "may I comment?" are the same question with one
    /// answer. Clearing the lease in the same statement follows `set_column`'s
    /// rule — a completed card holds no worker — spelled out here because this
    /// destination is never a `started` column.
    async fn complete_if_in_flight(
        &self,
        tenant: TenantId,
        id: TaskId,
        column: ColumnId,
    ) -> ApiResult<Option<TaskItem>>;

    /// Forget the worktree — both the checkout id and the legacy string pair.
    async fn clear_worktree(&self, id: TaskId) -> ApiResult<TaskItem>;

    /// Record where a loop BUILD run works (MAIN-480 AC-4). Narrower than
    /// [`record_started_work`] on purpose: a build run stamps only the
    /// worktree, never the branch, session or column — the skill owns the
    /// branch and the converger owns the column.
    async fn record_loop_worktree(
        &self,
        id: TaskId,
        node: NodeId,
        path: &str,
    ) -> ApiResult<TaskItem>;

    /// Every worktree path this node is recorded as holding — the set a
    /// reconnecting node's report is checked against (MAIN-480 AC-1).
    async fn worktree_paths_on_node(&self, node: NodeId) -> ApiResult<Vec<String>>;

    /// The subset of those whose card has NOT finished — what the compose-stack
    /// sweep protects (MAIN-507 AC-6). A card in review has a finished build and
    /// a live worktree, and a repair run reuses both, so "the build is over" is
    /// not the test; reaching a terminal column is.
    async fn active_worktree_paths_on_node(&self, node: NodeId) -> ApiResult<Vec<String>>;

    /// The complement: every worktree on this node whose card IS finished, with
    /// the card that records it (MAIN-537 AC-4).
    ///
    /// The card move prunes the tree of the card it moved; this answers the
    /// other question — which of the trees a long-lived node holds belong to a
    /// card that finished while nothing was listening. Both are the same test
    /// (`completed`/`canceled`), asked from opposite ends.
    async fn finished_worktrees_on_node(
        &self,
        node: NodeId,
    ) -> ApiResult<Vec<(TenantId, TaskId, String)>>;

    /// Move a card to `column`. Leaving a `started` column clears the claim
    /// lease (MAIN-229 AC-2) — the destination's type decides, in the same
    /// statement, so every mover (drag, `/move`, bulk) is covered by one rule.
    async fn set_column(&self, id: TaskId, column: ColumnId) -> ApiResult<TaskItem>;

    // ---- claim leases (MAIN-229) ------------------------------------------

    /// Push a leased card's expiry out to `ttl_secs` from now. `None` when the
    /// card does not exist in the tenant; `Some(None)` when it exists but is
    /// unleased, which the route turns into a 409.
    async fn renew_claim(
        &self,
        id: TaskId,
        tenant: TenantId,
        ttl_secs: i64,
    ) -> ApiResult<Option<Option<TaskItem>>>;

    /// Requeue every **leased** card in a `started` column whose worker is
    /// provably gone — its bound session exited/errored, or that session's node
    /// has not been seen for `session_grace_secs`. One guarded `UPDATE …
    /// RETURNING`, so replicas cannot double-requeue and a card whose lease was
    /// cleared between scan and update falls out of the guard untouched.
    async fn reap_lapsed_claims(&self, session_grace_secs: i64) -> ApiResult<Vec<LapsedClaim>>;

    /// Leased cards in a `started` column past their cap that the reap above did
    /// NOT take — the wedged-but-alive shape. Read-only: escalation labels and
    /// notifies, it never moves the card.
    async fn capped_claims(&self) -> ApiResult<Vec<CappedClaim>>;

    /// Attach `name` to `task`, reporting whether THIS call is the one that
    /// attached it. The `(task_id, label_id)` primary key makes that the
    /// exactly-once fence for escalation across replicas.
    async fn attach_label_once(
        &self,
        tenant: TenantId,
        task_id: TaskId,
        name: &str,
    ) -> ApiResult<bool>;

    // ---- comments and labels ----------------------------------------------

    async fn insert_agent_comment(
        &self,
        tenant: TenantId,
        task_id: TaskId,
        author_id: Uuid,
        author_name: &str,
        body_md: &str,
    ) -> ApiResult<TaskComment>;

    /// Get-or-create a label by name and attach it. The two statements travel
    /// together because the upsert exists only to feed the attach.
    async fn attach_label(&self, tenant: TenantId, task_id: TaskId, name: &str) -> ApiResult<()>;
    async fn detach_label(&self, tenant: TenantId, task_id: TaskId, name: &str) -> ApiResult<()>;

    // ---- checkout reads that task lifecycle needs --------------------------
    //
    // These read `node_workspaces`, not tasks. They are here because AC-4
    // requires `taskwork.rs` to hold no queries, and the task lifecycle is what
    // asks them. MAIN-251's `WorkspaceRepository` is the natural eventual home;
    // grouped and named so moving them later is mechanical.

    /// The present (non-missing) checkout at a path on a node.
    async fn present_checkout_at(
        &self,
        node_id: NodeId,
        path: &str,
    ) -> ApiResult<Option<NodeWorkspaceId>>;

    /// `(path, node)` for a checkout that is still present.
    async fn checkout_location(
        &self,
        checkout: NodeWorkspaceId,
    ) -> ApiResult<Option<(String, NodeId)>>;

    /// The path of a workspace's **clone** on a node — never a worktree, and
    /// deterministic rather than bare discovery order.
    async fn clone_path_on_node(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
        node: NodeId,
    ) -> ApiResult<Option<String>>;

    /// Any known git remote for a workspace, for deriving a compare URL.
    async fn git_remote_for_workspace(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
    ) -> ApiResult<Option<String>>;

    // ---- the pick query (MAIN-249) -----------------------------------------

    /// The filtered pick, before enrichment. One method: the filter is a single
    /// SQL statement whose clauses interlock (the backlog exclusion is lifted by
    /// a parent filter, the visibility predicate ANDs with the explicit one), so
    /// splitting it would be splitting one question into pieces that cannot be
    /// recombined.
    async fn pick_tasks(
        &self,
        tenant: TenantId,
        viewer: UserId,
        p: PickParams,
    ) -> ApiResult<Vec<TaskItem>>;

    /// A column's type, unscoped — the caller already holds a task that names it.
    async fn column_type_of(&self, column: ColumnId) -> ApiResult<Option<String>>;

    /// A board's automation rules blob (MAIN-256 moved this off `triggers.rs`).
    async fn board_automation(
        &self,
        board: BoardId,
        tenant: TenantId,
    ) -> ApiResult<Option<serde_json::Value>>;

    /// A task's visibility, unscoped — board automation already holds the task
    /// and only needs to know whether announcing it would leak a private card.
    async fn visibility_of(&self, task: TaskId) -> ApiResult<Option<String>>;

    /// Key parts plus title, for naming a task in an automated message. The
    /// board key is nullable, so an unkeyed board yields `(None, …)` rather
    /// than dropping the row.
    async fn task_ref(&self, task: TaskId) -> ApiResult<Option<(Option<String>, i32, String)>>;

    /// A column's type, scoped through its board's tenant. `board_columns` has
    /// no `tenant_id` of its own, so this is the tenant-safe form.
    async fn column_type_in_tenant(
        &self,
        column: ColumnId,
        tenant: TenantId,
    ) -> ApiResult<Option<String>>;

    async fn board_of_task(&self, id: TaskId) -> ApiResult<Option<BoardId>>;

    /// Take the work atomically: assign and move in one statement whose WHERE
    /// still tests "unassigned", so two agents racing cannot both win. `None`
    /// means the row did not match — the caller distinguishes a lost race from a
    /// missing task by reading the current assignee.
    ///
    /// `lease_secs` is `Some` only when the claim also moves the card into a
    /// `started` column — that is what makes it an agent claim carrying a lease
    /// (MAIN-229 AC-2). `None` leaves `claim_expires_at` exactly as it was.
    async fn claim_task(
        &self,
        id: TaskId,
        tenant: TenantId,
        assignee: UserId,
        column: Option<ColumnId>,
        lease_secs: Option<i64>,
    ) -> ApiResult<Option<TaskItem>>;

    async fn assignee_of(&self, id: TaskId, tenant: TenantId) -> ApiResult<Option<Option<UserId>>>;

    /// Clear the assignee — and with it the claim lease, since an unclaimed card
    /// is by definition not held (MAIN-229 AC-2). Reports the row when there was
    /// one. Used by both the single-task release and the bulk unassign; the
    /// latter ignores the returned row.
    async fn release_assignment(&self, id: TaskId, tenant: TenantId)
        -> ApiResult<Option<TaskItem>>;

    /// Release a claim ONLY while `holder` still holds it under a lease
    /// (MAIN-489 AC-7). The lease is the fence the claim reaper draws: a card a
    /// human dragged into progress has none, and a card a human took over has
    /// another assignee — so neither is undone by a run of ours dying. `true`
    /// when the release actually happened.
    async fn release_claim_of(
        &self,
        id: TaskId,
        tenant: TenantId,
        holder: UserId,
    ) -> ApiResult<bool>;

    /// The card's consecutive concluded-nothing build count (MAIN-489 AC-4).
    /// `0` for a card that has none, and for one that is gone.
    async fn build_failures(&self, id: TaskId, tenant: TenantId) -> ApiResult<i32>;

    /// Add one to that count, and report the new total.
    async fn bump_build_failures(&self, id: TaskId, tenant: TenantId) -> ApiResult<i32>;

    /// Zero that count, but only when it has reached `at_least` — `0` zeroes
    /// unconditionally. The threshold is what makes one call serve both resets:
    /// an outcome spends whatever is there, while a card re-entering the pick
    /// with a FULL set can only have got there through a human's hand (the
    /// `blocked` label lifted, or the card named to the manual trigger), so its
    /// strikes are spent too (AC-5/AC-6).
    async fn clear_build_failures(
        &self,
        id: TaskId,
        tenant: TenantId,
        at_least: i32,
    ) -> ApiResult<()>;

    /// Attach or detach the `agent-ready` label, creating it if new. One method
    /// because the upsert exists only to feed the attach/detach.
    async fn set_agent_ready(&self, tenant: TenantId, id: TaskId, on: bool) -> ApiResult<()>;

    // ---- labels ------------------------------------------------------------

    async fn list_labels(&self, tenant: TenantId) -> ApiResult<Vec<Label>>;
    async fn upsert_label(&self, tenant: TenantId, name: &str, color: &str) -> ApiResult<Label>;
    async fn delete_label(&self, id: Uuid, tenant: TenantId) -> ApiResult<u64>;
    async fn label_id_by_uuid(&self, id: Uuid, tenant: TenantId) -> ApiResult<Option<Uuid>>;
    async fn label_id_by_name(&self, tenant: TenantId, name: &str) -> ApiResult<Option<Uuid>>;
    async fn labels_of_task(&self, task: TaskId) -> ApiResult<Vec<Label>>;

    /// Rows affected, so a caller only records an event when something actually
    /// changed — an agent re-applying a label every poll would otherwise flood
    /// the timeline a human reads.
    async fn attach_label_id(&self, task: TaskId, label: Uuid) -> ApiResult<u64>;
    async fn detach_label_id(&self, task: TaskId, label: Uuid) -> ApiResult<u64>;

    /// `(title, number, board key)` — enough to name a task in a notification
    /// without its title when the card is private.
    async fn task_naming(
        &self,
        task: TaskId,
    ) -> ApiResult<Option<(String, Option<i32>, Option<String>)>>;

    // ---- boards ------------------------------------------------------------

    async fn create_board(
        &self,
        tenant: TenantId,
        workspace: Option<Uuid>,
        name: &str,
        key: &str,
    ) -> ApiResult<Board>;

    async fn create_column(
        &self,
        board: BoardId,
        name: &str,
        position: i32,
        type_: &str,
    ) -> ApiResult<BoardColumn>;

    async fn set_archived(
        &self,
        id: TaskId,
        tenant: TenantId,
        archived: bool,
    ) -> ApiResult<Option<TaskItem>>;

    /// Archive every unarchived task in a column, returning what moved so the
    /// caller can publish one change per card.
    async fn archive_all_in_column(
        &self,
        column: ColumnId,
        tenant: TenantId,
    ) -> ApiResult<Vec<TaskId>>;

    async fn delete_task(&self, id: TaskId, tenant: TenantId) -> ApiResult<u64>;

    async fn update_board(
        &self,
        id: BoardId,
        tenant: TenantId,
        name: &str,
        key: Option<String>,
        automation: Option<serde_json::Value>,
    ) -> ApiResult<Option<Board>>;

    async fn board_key_taken(&self, tenant: TenantId, key: &str) -> ApiResult<bool>;

    // ---- comments and relations -------------------------------------------

    async fn comments_of(&self, task: TaskId) -> ApiResult<Vec<TaskComment>>;

    async fn create_comment(&self, new: NewComment) -> ApiResult<TaskComment>;

    /// Keep the body a description replace is about to destroy (MAIN-470
    /// AC-3) — the undo a whole-body PATCH otherwise lacks.
    async fn add_description_revision(
        &self,
        tenant: TenantId,
        task: TaskId,
        body: &str,
        author: Option<Uuid>,
    ) -> ApiResult<()>;

    /// Newest first: the reader is undoing the most recent clobber.
    async fn description_revisions_of(
        &self,
        tenant: TenantId,
        task: TaskId,
    ) -> ApiResult<Vec<TaskDescriptionRevision>>;

    /// `(visibility, number, board key)` — what decides whether a notification
    /// may carry an excerpt, and how to name the card if not.
    async fn task_visibility_naming(
        &self,
        task: TaskId,
        tenant: TenantId,
    ) -> ApiResult<Option<(String, Option<i32>, Option<String>)>>;

    async fn update_comment(
        &self,
        id: Uuid,
        tenant: TenantId,
        body_md: &str,
    ) -> ApiResult<TaskComment>;

    async fn delete_comment(&self, id: Uuid, tenant: TenantId) -> ApiResult<()>;

    /// `(author, task)` for a comment — the ownership check editing and deleting
    /// both route through.
    async fn comment_author(
        &self,
        id: Uuid,
        tenant: TenantId,
    ) -> ApiResult<Option<(Option<Uuid>, TaskId)>>;

    async fn upsert_relation(
        &self,
        tenant: TenantId,
        from: TaskId,
        to: TaskId,
        kind: &str,
    ) -> ApiResult<TaskRelation>;

    /// Does `start` reach `target` through `blocks` edges? A cycle is a deadlock
    /// nothing can ever pick up, so this is the guard that refuses one.
    async fn blocks_reaches(&self, start: TaskId, target: TaskId) -> ApiResult<bool>;

    async fn delete_relation(&self, id: Uuid, tenant: TenantId) -> ApiResult<u64>;

    /// An epic's children, filtered by the same visibility predicate the list
    /// and board reads enforce.
    async fn epic_children(&self, parent: TaskId, viewer: UserId) -> ApiResult<Vec<EpicChild>>;

    async fn related_tasks(&self, task: TaskId, viewer: UserId) -> ApiResult<Vec<RelatedTask>>;

    /// The keys of tasks whose worktree is that exact directory on that node.
    /// `worktree_path` is a plain string, not an FK, so a checkout can be
    /// reclaimed out from under a task — this is what lets the reaper say which
    /// tasks it just orphaned instead of dropping the row silently (MAIN-220).
    async fn task_keys_at_worktree(&self, node: NodeId, path: &str) -> ApiResult<Vec<String>>;

    /// One task's board key (`MAIN-42`), tenant-scoped. What a loop job quotes
    /// when it narrates which ticket it is working (MAIN-255).
    async fn key_of(&self, tenant: TenantId, id: TaskId) -> ApiResult<Option<String>>;

    // ---- board and column administration (MAIN-249) ------------------------

    async fn delete_board(&self, id: BoardId, tenant: TenantId) -> ApiResult<u64>;

    /// Does this tenant own the board? The ownership gate every column write
    /// runs first.
    async fn board_in_tenant(&self, id: BoardId, tenant: TenantId) -> ApiResult<bool>;

    async fn max_column_position(&self, board: BoardId) -> ApiResult<Option<i32>>;

    /// Add a column at an explicit position, letting the column type default —
    /// distinct from [`TaskRepository::create_column`], which the board
    /// bootstrap uses to set the type as well.
    async fn append_column(
        &self,
        board: BoardId,
        name: &str,
        position: i32,
    ) -> ApiResult<BoardColumn>;

    /// Rename/reposition a column, scoped through its board's tenant because
    /// `board_columns` has no `tenant_id` of its own.
    async fn update_column(
        &self,
        id: ColumnId,
        tenant: TenantId,
        name: Option<String>,
        position: Option<i32>,
    ) -> ApiResult<Option<BoardColumn>>;

    async fn delete_column(&self, id: ColumnId, tenant: TenantId) -> ApiResult<u64>;

    async fn board_provider(&self, id: BoardId, tenant: TenantId) -> ApiResult<Option<String>>;
}

/// The real implementation, over the engine-agnostic pool.
pub struct DbTaskRepository {
    db: DbPool,
}

impl DbTaskRepository {
    pub fn new(db: DbPool) -> Self {
        Self { db }
    }
}

#[async_trait]
impl TaskRepository for DbTaskRepository {
    async fn board_keys(&self, board_ids: &[Uuid]) -> ApiResult<HashMap<Uuid, Option<String>>> {
        Ok(self
            .db
            .query_all::<(Uuid, Option<String>)>(
                "SELECT id, key FROM boards WHERE id = ANY($1)",
                params![board_ids],
            )
            .await?
            .into_iter()
            .collect())
    }

    async fn operator_visible_titles(
        &self,
        tenant: TenantId,
        limit: i64,
    ) -> ApiResult<Vec<String>> {
        let rows: Vec<(String,)> = self
            .db
            .query_all(
                &format!(
                    "SELECT title FROM tasks
             WHERE tenant_id = $1 AND {public}
             ORDER BY created_at DESC LIMIT $2",
                    // No viewer here on purpose — the digest is not scoped to a
                    // reader, so every private card drops (MAIN-265).
                    public = crate::services::tasks::public_only_sql(""),
                ),
                params![tenant, limit],
            )
            .await?;
        Ok(rows.into_iter().map(|(t,)| t).collect())
    }

    async fn labels_for_tasks(&self, task_ids: &[Uuid]) -> ApiResult<Vec<TaskLabelRow>> {
        Ok(self
            .db
            .query_all(
                "SELECT tl.task_id, l.id, l.tenant_id, l.name, l.color, l.created_at
             FROM task_labels tl
             JOIN labels l ON l.id = tl.label_id
             WHERE tl.task_id = ANY($1)
             ORDER BY l.name",
                params![task_ids],
            )
            .await?)
    }

    async fn parent_info(&self, parent_ids: &[Uuid]) -> ApiResult<HashMap<Uuid, ParentInfo>> {
        if parent_ids.is_empty() {
            return Ok(HashMap::new());
        }
        Ok(self
            .db
            .query_all::<(Uuid, Option<i32>, String, Option<UserId>, Option<UserId>)>(
                "SELECT id, number, visibility, created_by, assignee_user_id
             FROM tasks WHERE id = ANY($1)",
                params![parent_ids],
            )
            .await?
            .into_iter()
            .map(|(id, number, visibility, created_by, assignee)| {
                (id, (number, visibility, created_by, assignee))
            })
            .collect())
    }

    async fn id_by_uuid(&self, tenant: TenantId, id: Uuid) -> ApiResult<Option<TaskId>> {
        Ok(self
            .db
            .query_scalar_opt(
                "SELECT id FROM tasks WHERE id = $1 AND tenant_id = $2",
                params![id, tenant],
            )
            .await?)
    }

    async fn id_by_key(
        &self,
        tenant: TenantId,
        board_key: &str,
        number: i32,
    ) -> ApiResult<Option<TaskId>> {
        Ok(self
            .db
            .query_scalar_opt(
                "SELECT t.id FROM tasks t
         JOIN boards b ON b.id = t.board_id
         WHERE t.tenant_id = $1 AND upper(b.key) = upper($2) AND t.number = $3",
                params![tenant, board_key, number],
            )
            .await?)
    }

    async fn column_of_type(
        &self,
        board: BoardId,
        column_type: &str,
    ) -> ApiResult<Option<ColumnId>> {
        Ok(self
            .db
            .query_scalar_opt(
                "SELECT id FROM board_columns WHERE board_id = $1 AND type = $2
         ORDER BY position LIMIT 1",
                params![board, column_type],
            )
            .await?)
    }

    async fn column_by_name(&self, board: BoardId, name: &str) -> ApiResult<Option<ColumnId>> {
        Ok(self
            .db
            .query_scalar_opt(
                "SELECT id FROM board_columns WHERE board_id = $1 AND lower(name) = lower($2)",
                params![board, name],
            )
            .await?)
    }

    async fn column_at_position(&self, board: BoardId, offset: i64) -> ApiResult<Option<ColumnId>> {
        Ok(self
            .db
            .query_scalar_opt(
                "SELECT id FROM board_columns WHERE board_id = $1 ORDER BY position OFFSET $2 LIMIT 1",
                params![board, offset],
            )
            .await?)
    }

    async fn first_column(&self, board: BoardId) -> ApiResult<Option<ColumnId>> {
        Ok(self
            .db
            .query_scalar_opt(
                "SELECT id FROM board_columns WHERE board_id = $1 ORDER BY position LIMIT 1",
                params![board],
            )
            .await?)
    }

    async fn list_local_boards(&self, tenant: TenantId) -> ApiResult<Vec<Board>> {
        Ok(self
            .db
            .query_all(
                "SELECT * FROM boards WHERE tenant_id = $1 AND provider = 'local' ORDER BY created_at",
                params![tenant],
            )
            .await?)
    }

    async fn get_board(&self, tenant: TenantId, board: BoardId) -> ApiResult<Option<Board>> {
        Ok(self
            .db
            .query_opt(
                "SELECT * FROM boards WHERE id = $1 AND tenant_id = $2",
                params![board, tenant],
            )
            .await?)
    }

    async fn board_columns(&self, board: BoardId) -> ApiResult<Vec<BoardColumn>> {
        Ok(self
            .db
            .query_all(
                "SELECT * FROM board_columns WHERE board_id = $1 ORDER BY position, name",
                params![board],
            )
            .await?)
    }

    async fn board_tasks(&self, board: BoardId) -> ApiResult<Vec<TaskItem>> {
        Ok(self
            .db
            .query_all(
                "SELECT * FROM tasks WHERE board_id = $1 ORDER BY position, created_at",
                params![board],
            )
            .await?)
    }

    async fn board_provider_for_task(
        &self,
        tenant: TenantId,
        task_id: TaskId,
    ) -> ApiResult<Option<String>> {
        Ok(self
            .db
            .query_scalar_opt(
                "SELECT b.provider FROM boards b
             JOIN tasks t ON t.board_id = b.id
             WHERE t.id = $1 AND t.tenant_id = $2",
                params![task_id, tenant],
            )
            .await?)
    }

    async fn get_row(&self, tenant: TenantId, id: TaskId) -> ApiResult<Option<TaskItem>> {
        Ok(self
            .db
            .query_opt(
                "SELECT * FROM tasks WHERE id = $1 AND tenant_id = $2",
                params![id, tenant],
            )
            .await?)
    }

    async fn task_shape(
        &self,
        tenant: TenantId,
        id: TaskId,
    ) -> ApiResult<Option<(String, BoardId, Option<TaskId>)>> {
        Ok(self
            .db
            .query_opt(
                "SELECT type, board_id, parent_task_id FROM tasks WHERE id = $1 AND tenant_id = $2",
                params![id, tenant],
            )
            .await?)
    }

    async fn count_children(&self, id: TaskId) -> ApiResult<i64> {
        Ok(self
            .db
            .query_scalar(
                "SELECT count(*) FROM tasks WHERE parent_task_id = $1",
                params![id],
            )
            .await?)
    }

    async fn max_position_in_column(&self, column: ColumnId) -> ApiResult<Option<i32>> {
        Ok(self
            .db
            .query_scalar(
                "SELECT max(position) FROM tasks WHERE column_id = $1",
                params![column],
            )
            .await?)
    }

    async fn tasks_with_pr(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
    ) -> ApiResult<Vec<(TaskId, i64, String)>> {
        // `number` is INT4 on Postgres: decode the column's own width and
        // widen in Rust, or the whole read dies with a ColumnDecode there
        // (SQLite is untyped enough not to notice).
        let rows: Vec<(Uuid, i32, String)> = self
            .db
            .query_all(
                "SELECT t.id, t.number, t.pr_url FROM tasks t
                   JOIN board_columns c ON c.id = t.column_id
                  WHERE t.tenant_id = $1 AND t.workspace_id = $2
                    AND t.pr_url IS NOT NULL AND t.archived_at IS NULL
                    AND c.type NOT IN ('completed', 'canceled')
                    AND NOT EXISTS (
                        SELECT 1 FROM task_labels tl JOIN labels l ON l.id = tl.label_id
                         WHERE tl.task_id = t.id AND l.name = 'blocked')",
                params![tenant, workspace.0],
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|(id, n, url)| (TaskId(id), i64::from(n), url))
            .collect())
    }

    async fn in_flight_tasks_with_pr(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
    ) -> ApiResult<Vec<(TaskId, String)>> {
        let rows: Vec<(Uuid, String)> = self
            .db
            .query_all(
                "SELECT t.id, t.pr_url FROM tasks t
                   JOIN board_columns c ON c.id = t.column_id
                  WHERE t.tenant_id = $1 AND t.workspace_id = $2
                    AND t.pr_url IS NOT NULL AND t.archived_at IS NULL
                    AND c.type NOT IN ('completed', 'canceled')",
                params![tenant, workspace.0],
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|(id, url)| (TaskId(id), url))
            .collect())
    }

    async fn create_task(&self, new: NewTask) -> ApiResult<TaskItem> {
        // One transaction, and the board row locked while the number is taken.
        // Without the lock two concurrent creates read the same `next_number`
        // and one then violates the unique index — a 500 for something the
        // caller did nothing wrong to cause. `FOR UPDATE` makes the second wait.
        let mut tx = self.db.begin().await.map_err(nook_db::DbError::from)?;
        let number: i32 = tx
            .query_scalar(
                &format!(
                    "UPDATE boards SET next_number = next_number + 1
             WHERE id = (SELECT id FROM boards WHERE id = $1 {lock})
             RETURNING next_number - 1",
                    // The WAITING lock, never the queue's skipping one: a second
                    // creator must block and then read the incremented counter,
                    // where `SKIP LOCKED` would match no row and fail the create.
                    lock = atomic_claim(self.db.engine()).row_lock_clause()
                ),
                params![new.board],
            )
            .await?;

        let task: TaskItem = tx
            .query_one(
                "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, description,
                                position, workspace_id, priority, type, number,
                                visibility, created_by, parent_task_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14) RETURNING *",
                params![
                    TaskId::new(),
                    new.tenant,
                    new.board,
                    new.column_id,
                    &new.title,
                    new.description,
                    new.position,
                    new.workspace_id,
                    new.priority,
                    &new.type_,
                    number,
                    &new.visibility,
                    new.created_by,
                    new.parent_task_id
                ],
            )
            .await?;

        // Inside the transaction so a task never exists momentarily without the
        // labels it was filed with — the pick query would otherwise have a
        // window in which it sees unlabelled work.
        for name in &new.labels {
            let label_id: Uuid = tx
                .query_scalar(
                    "INSERT INTO labels (id, tenant_id, name) VALUES ($1, $2, $3)
                 ON CONFLICT (tenant_id, name) DO UPDATE SET name = EXCLUDED.name
                 RETURNING id",
                    params![Uuid::now_v7(), new.tenant, name],
                )
                .await?;
            tx.exec(
                "INSERT INTO task_labels (task_id, label_id) VALUES ($1, $2)
                 ON CONFLICT DO NOTHING",
                params![task.id, label_id],
            )
            .await?;
        }

        tx.commit().await?;
        Ok(task)
    }

    async fn update_fields(
        &self,
        tenant: TenantId,
        id: TaskId,
        edit: TaskEdit,
    ) -> ApiResult<Option<TaskItem>> {
        Ok(self
            .db
            .query_opt(
                &format!(
                    // Workspace cannot use COALESCE like the rest: COALESCE reads a
                    // NULL as "leave it", which is exactly the instruction to clear
                    // it. The flag says whether the caller mentioned the field at all.
                    //
                    // `$11` is the optimistic-concurrency precondition (MAIN-36): NULL
                    // means "unguarded" (behaviour unchanged), otherwise the row updates
                    // only while its `updated_at` still equals what the caller last saw.
                    // A guarded update that touches 0 rows is a lost race, told apart
                    // from a missing task by the caller.
                    "UPDATE tasks SET
                title = COALESCE($3, title),
                description = COALESCE($4, description),
                column_id = COALESCE($5, column_id),
                position = COALESCE($6, position),
                assignee_user_id = COALESCE($7, assignee_user_id),
                priority = COALESCE($8, priority),
                workspace_id = CASE WHEN $9 THEN $10 ELSE workspace_id END,
                type = COALESCE($12, type),
                visibility = COALESCE($13, visibility),
                parent_task_id = CASE WHEN $14 THEN $15 ELSE parent_task_id END,
                updated_at = {now}
             WHERE id = $1 AND tenant_id = $2
               AND ({guard} IS NULL OR updated_at = $11)
             RETURNING *",
                    now = type_mapping(self.db.engine()).now(),
                    guard = type_mapping(self.db.engine()).cast("$11", "timestamptz")
                ),
                params![
                    id,
                    tenant,
                    edit.title,
                    edit.description,
                    edit.column_id,
                    edit.position,
                    edit.assignee_user_id,
                    edit.priority,
                    edit.set_workspace,
                    edit.workspace_id,
                    edit.expected_updated_at,
                    edit.type_,
                    edit.visibility,
                    edit.set_parent,
                    edit.parent_task_id
                ],
            )
            .await?)
    }

    async fn clear_assignee(&self, tenant: TenantId, id: TaskId) -> ApiResult<TaskItem> {
        Ok(self
            .db
            .query_one(
                &format!(
                    "UPDATE tasks SET assignee_user_id = NULL, claim_expires_at = NULL,
                updated_at = {now}
             WHERE id = $1 AND tenant_id = $2 RETURNING *",
                    now = type_mapping(self.db.engine()).now()
                ),
                params![id, tenant],
            )
            .await?)
    }

    async fn set_priority(
        &self,
        tenant: TenantId,
        id: TaskId,
        priority: i32,
    ) -> ApiResult<TaskItem> {
        Ok(self
            .db
            .query_one(
                &format!(
                    "UPDATE tasks SET priority = $3, updated_at = {now}
             WHERE id = $1 AND tenant_id = $2 RETURNING *",
                    now = type_mapping(self.db.engine()).now()
                ),
                params![id, tenant, priority],
            )
            .await?)
    }

    async fn assign_node_and_column(
        &self,
        id: TaskId,
        node: NodeId,
        column: Option<ColumnId>,
    ) -> ApiResult<TaskItem> {
        Ok(self
            .db
            .query_one(
                &format!(
                    "UPDATE tasks SET assigned_node_id = $2,
                            column_id = COALESCE($3, column_id),
                            updated_at = {}
         WHERE id = $1 RETURNING *",
                    type_mapping(self.db.engine()).now()
                ),
                params![id, node, column.map(|c| c.0)],
            )
            .await?)
    }

    async fn record_started_work(&self, id: TaskId, work: StartedWork) -> ApiResult<TaskItem> {
        Ok(self
            .db
            .query_one(
                &format!(
                    "UPDATE tasks SET workspace_id = $2, assigned_node_id = $3, branch = $4,
                worktree_path = $5, worktree_node_id = $3, session_id = $6,
                column_id = $7, checkout_id = $8, claim_expires_at = {lease},
                updated_at = {now}
         WHERE id = $1 RETURNING *",
                    lease = time_math(self.db.engine()).now_plus_scaled(
                        &type_mapping(self.db.engine()).cast("$9", "bigint"),
                        "1 second"
                    ),
                    now = type_mapping(self.db.engine()).now()
                ),
                params![
                    id,
                    work.workspace_id,
                    work.node_id,
                    &work.branch,
                    &work.worktree_path,
                    work.session_id,
                    work.column_id,
                    work.checkout_id,
                    work.claim_ttl_secs
                ],
            )
            .await?)
    }

    async fn set_pr_url(&self, id: TaskId, url: &str, column: ColumnId) -> ApiResult<TaskItem> {
        Ok(self
            .db
            .query_one(
                &format!(
                    "UPDATE tasks SET pr_url = $2, column_id = $3, claim_expires_at = NULL,
                updated_at = {}
         WHERE id = $1 RETURNING *",
                    type_mapping(self.db.engine()).now()
                ),
                params![id, url, column],
            )
            .await?)
    }

    async fn backfill_pr_url(&self, tenant: TenantId, id: TaskId, url: &str) -> ApiResult<bool> {
        let written = self
            .db
            .exec(
                &format!(
                    "UPDATE tasks SET pr_url = $3, updated_at = {now}
              WHERE id = $1 AND tenant_id = $2 AND pr_url IS NULL AND archived_at IS NULL
                AND EXISTS (SELECT 1 FROM board_columns c
                             WHERE c.id = tasks.column_id
                               AND c.type NOT IN ('completed', 'canceled'))",
                    now = type_mapping(self.db.engine()).now()
                ),
                params![id, tenant, url],
            )
            .await?;
        Ok(written > 0)
    }

    async fn complete_if_in_flight(
        &self,
        tenant: TenantId,
        id: TaskId,
        column: ColumnId,
    ) -> ApiResult<Option<TaskItem>> {
        Ok(self
            .db
            .query_opt(
                &format!(
                    "UPDATE tasks SET column_id = $3, claim_expires_at = NULL, updated_at = {now}
              WHERE id = $1 AND tenant_id = $2 AND archived_at IS NULL
                AND EXISTS (SELECT 1 FROM board_columns c
                             WHERE c.id = tasks.column_id
                               AND c.type NOT IN ('completed', 'canceled'))
              RETURNING *",
                    now = type_mapping(self.db.engine()).now()
                ),
                params![id, tenant, column],
            )
            .await?)
    }

    async fn clear_worktree(&self, id: TaskId) -> ApiResult<TaskItem> {
        Ok(self
            .db
            .query_one(
                &format!(
                    "UPDATE tasks SET checkout_id = NULL, worktree_path = NULL,
                worktree_node_id = NULL, updated_at = {}
         WHERE id = $1 RETURNING *",
                    type_mapping(self.db.engine()).now()
                ),
                params![id],
            )
            .await?)
    }

    async fn record_loop_worktree(
        &self,
        id: TaskId,
        node: NodeId,
        path: &str,
    ) -> ApiResult<TaskItem> {
        Ok(self
            .db
            .query_one(
                &format!(
                    "UPDATE tasks SET worktree_path = $2, worktree_node_id = $3,
                updated_at = {}
         WHERE id = $1 RETURNING *",
                    type_mapping(self.db.engine()).now()
                ),
                params![id, path, node],
            )
            .await?)
    }

    async fn worktree_paths_on_node(&self, node: NodeId) -> ApiResult<Vec<String>> {
        let rows: Vec<(String,)> = self
            .db
            .query_all(
                "SELECT worktree_path FROM tasks
                  WHERE worktree_node_id = $1 AND worktree_path IS NOT NULL",
                params![node],
            )
            .await?;
        Ok(rows.into_iter().map(|(p,)| p).collect())
    }

    async fn active_worktree_paths_on_node(&self, node: NodeId) -> ApiResult<Vec<String>> {
        let rows: Vec<(String,)> = self
            .db
            .query_all(
                "SELECT t.worktree_path FROM tasks t
                   JOIN board_columns c ON c.id = t.column_id
                  WHERE t.worktree_node_id = $1 AND t.worktree_path IS NOT NULL
                    AND t.archived_at IS NULL
                    AND c.type NOT IN ('completed', 'canceled')",
                params![node],
            )
            .await?;
        Ok(rows.into_iter().map(|(p,)| p).collect())
    }

    async fn finished_worktrees_on_node(
        &self,
        node: NodeId,
    ) -> ApiResult<Vec<(TenantId, TaskId, String)>> {
        Ok(self
            .db
            .query_all(
                "SELECT t.tenant_id, t.id, t.worktree_path FROM tasks t
                   JOIN board_columns c ON c.id = t.column_id
                  WHERE t.worktree_node_id = $1 AND t.worktree_path IS NOT NULL
                    AND c.type IN ('completed', 'canceled')",
                params![node],
            )
            .await?)
    }

    async fn set_column(&self, id: TaskId, column: ColumnId) -> ApiResult<TaskItem> {
        // The lease dies with the move UNLESS the destination is itself a
        // `started` column — one statement, so no mover can forget it and a card
        // requeued or sent to review never keeps a claim it no longer has.
        Ok(self
            .db
            .query_one(
                &format!(
                    "UPDATE tasks SET column_id = $2,
                claim_expires_at = CASE
                    WHEN (SELECT type FROM board_columns WHERE id = $2) = 'started'
                    THEN claim_expires_at ELSE NULL END,
                updated_at = {}
         WHERE id = $1 RETURNING *",
                    type_mapping(self.db.engine()).now()
                ),
                params![id, column],
            )
            .await?)
    }

    async fn renew_claim(
        &self,
        id: TaskId,
        tenant: TenantId,
        ttl_secs: i64,
    ) -> ApiResult<Option<Option<TaskItem>>> {
        let renewed: Option<TaskItem> = self
            .db
            .query_opt(
                &format!(
                    "UPDATE tasks SET claim_expires_at = {lease}, updated_at = {now}
         WHERE id = $1 AND tenant_id = $2 AND claim_expires_at IS NOT NULL
         RETURNING *",
                    lease = time_math(self.db.engine()).now_plus_scaled(
                        &type_mapping(self.db.engine()).cast("$3", "bigint"),
                        "1 second"
                    ),
                    now = type_mapping(self.db.engine()).now()
                ),
                params![id, tenant, ttl_secs],
            )
            .await?;
        if renewed.is_some() {
            return Ok(Some(renewed));
        }
        // No row matched: either the card is unleased (exists → `Some(None)`) or
        // it is not this tenant's at all (→ `None`).
        let exists: Option<Uuid> = self
            .db
            .query_scalar_opt(
                "SELECT id FROM tasks WHERE id = $1 AND tenant_id = $2",
                params![id, tenant],
            )
            .await?;
        Ok(exists.map(|_| None))
    }

    async fn reap_lapsed_claims(&self, session_grace_secs: i64) -> ApiResult<Vec<LapsedClaim>> {
        // `claim_expires_at IS NOT NULL` is both the fence (AC-7: an unleased
        // card is never in the candidate set) and the exactly-once guard — the
        // UPDATE clears it, so a second replica's scan matches nothing.
        //
        // The destination is resolved as a scalar subquery over the card's own
        // board, and the matching EXISTS keeps a board with no `unstarted`
        // column out of the set entirely rather than writing a NULL column_id.
        //
        // SELECT-then-UPDATE, not `UPDATE … FROM … RETURNING` (MAIN-355). That
        // spelling is Postgres-only twice over — the alias-without-AS and a
        // RETURNING list naming the joined tables, neither of which SQLite has —
        // and it is what kept `claim_lease` red on the SQLite leg, and main with
        // it. Splitting it costs one round trip per lapsed card, which is a
        // rounding error on a set that is empty almost every scan.
        //
        // EXACTLY-ONCE SURVIVES THE SPLIT because the UPDATE re-checks every
        // predicate rather than trusting the SELECT. Two replicas can both
        // choose the same card; only the one whose UPDATE reports a row treats
        // it as reaped, and the other's matches nothing because
        // `claim_expires_at` is already NULL. That also closes a window the
        // single statement never had to think about: a card that left the
        // `started` column, or whose session came back, between the two
        // statements is no longer requeued on the strength of a stale read.
        let candidates: Vec<(
            TaskId,
            TenantId,
            SessionId,
            String,
            Option<chrono::DateTime<chrono::Utc>>,
        )> = self
            .db
            .query_all(
                &format!(
                    "SELECT t.id, t.tenant_id, s.id, s.status, n.last_seen_at
                       FROM tasks t
                       JOIN sessions s ON s.id = t.session_id
                       JOIN nodes n ON n.id = s.node_id
                      WHERE t.claim_expires_at IS NOT NULL
                        AND EXISTS (SELECT 1 FROM board_columns cur
                                     WHERE cur.id = t.column_id AND cur.type = 'started')
                        AND EXISTS (SELECT 1 FROM board_columns tgt
                                     WHERE tgt.board_id = t.board_id AND tgt.type = 'unstarted')
                        AND (s.status IN ('exited', 'error')
                             OR (n.last_seen_at IS NOT NULL AND n.last_seen_at < {cutoff}))",
                    cutoff = time_math(self.db.engine()).now_minus_scaled(
                        &type_mapping(self.db.engine()).cast("$1", "bigint"),
                        "1 second"
                    )
                ),
                params![session_grace_secs],
            )
            .await?;

        let mut reaped = Vec::new();
        for (task, tenant, session, session_status, node_last_seen_at) in candidates {
            let changed = self
                .db
                .exec(
                    &format!(
                        "UPDATE tasks
                            SET column_id = (SELECT tgt.id FROM board_columns tgt
                                              WHERE tgt.board_id = tasks.board_id
                                                AND tgt.type = 'unstarted'
                                              ORDER BY tgt.position LIMIT 1),
                                assigned_node_id = NULL,
                                session_id = NULL,
                                claim_expires_at = NULL,
                                updated_at = {now}
                          WHERE id = $1
                            AND claim_expires_at IS NOT NULL
                            AND EXISTS (SELECT 1 FROM board_columns cur
                                         WHERE cur.id = tasks.column_id
                                           AND cur.type = 'started')
                            AND EXISTS (SELECT 1 FROM board_columns tgt
                                         WHERE tgt.board_id = tasks.board_id
                                           AND tgt.type = 'unstarted')
                            AND EXISTS (SELECT 1 FROM sessions s
                                          JOIN nodes n ON n.id = s.node_id
                                         WHERE s.id = tasks.session_id
                                           AND (s.status IN ('exited', 'error')
                                                OR (n.last_seen_at IS NOT NULL
                                                    AND n.last_seen_at < {cutoff})))",
                        now = type_mapping(self.db.engine()).now(),
                        cutoff = time_math(self.db.engine()).now_minus_scaled(
                            &type_mapping(self.db.engine()).cast("$2", "bigint"),
                            "1 second"
                        )
                    ),
                    params![task, session_grace_secs],
                )
                .await?;
            if changed > 0 {
                reaped.push(LapsedClaim {
                    task,
                    tenant,
                    session,
                    session_status,
                    node_last_seen_at,
                });
            }
        }
        Ok(reaped)
    }

    async fn capped_claims(&self) -> ApiResult<Vec<CappedClaim>> {
        let rows: Vec<(TaskId, TenantId, chrono::DateTime<chrono::Utc>)> = self
            .db
            .query_all(
                &format!(
                    "SELECT t.id, t.tenant_id, t.claim_expires_at
                       FROM tasks t
                      WHERE t.claim_expires_at IS NOT NULL
                        AND t.claim_expires_at < {now}
                        AND EXISTS (SELECT 1 FROM board_columns cur
                                     WHERE cur.id = t.column_id AND cur.type = 'started')",
                    now = type_mapping(self.db.engine()).now()
                ),
                params![],
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|(task, tenant, claim_expires_at)| CappedClaim {
                task,
                tenant,
                claim_expires_at,
            })
            .collect())
    }

    async fn attach_label_once(
        &self,
        tenant: TenantId,
        task_id: TaskId,
        name: &str,
    ) -> ApiResult<bool> {
        let label_id: Uuid = self
            .db
            .query_scalar(
                "INSERT INTO labels (id, tenant_id, name) VALUES ($1, $2, $3)
             ON CONFLICT (tenant_id, name) DO UPDATE SET name = EXCLUDED.name RETURNING id",
                params![Uuid::now_v7(), tenant, name],
            )
            .await?;
        let inserted = self
            .db
            .exec(
                "INSERT INTO task_labels (task_id, label_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
                params![task_id, label_id],
            )
            .await?;
        Ok(inserted > 0)
    }

    async fn insert_agent_comment(
        &self,
        tenant: TenantId,
        task_id: TaskId,
        author_id: Uuid,
        author_name: &str,
        body_md: &str,
    ) -> ApiResult<TaskComment> {
        Ok(self
            .db
            .query_one(
                "INSERT INTO task_comments (id, tenant_id, task_id, author_type, author_id, author_name, body_md)
             VALUES ($1, $2, $3, 'agent', $4, $5, $6)
             RETURNING id, tenant_id, task_id, author_type, author_id, author_name,
                       body_md, created_at, updated_at",
                params![Uuid::now_v7(), tenant, task_id, author_id, author_name, body_md],
            )
            .await?)
    }

    async fn attach_label(&self, tenant: TenantId, task_id: TaskId, name: &str) -> ApiResult<()> {
        let label_id: Uuid = self
            .db
            .query_scalar(
                "INSERT INTO labels (id, tenant_id, name) VALUES ($1, $2, $3)
             ON CONFLICT (tenant_id, name) DO UPDATE SET name = EXCLUDED.name RETURNING id",
                params![Uuid::now_v7(), tenant, name],
            )
            .await?;
        self.db
            .exec(
                "INSERT INTO task_labels (task_id, label_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
                params![task_id, label_id],
            )
            .await?;
        Ok(())
    }

    async fn detach_label(&self, tenant: TenantId, task_id: TaskId, name: &str) -> ApiResult<()> {
        // A subquery, not `DELETE … USING`: USING is Postgres-only and this
        // file is held to SQL both engines run — the old form errored on
        // SQLite, which surfaced when MAIN-476's card mirror started detaching
        // labels from inside the control plane.
        self.db
            .exec(
                "DELETE FROM task_labels
                  WHERE task_id = $1
                    AND label_id IN (SELECT id FROM labels WHERE tenant_id = $2 AND name = $3)",
                params![task_id, tenant, name],
            )
            .await?;
        Ok(())
    }

    async fn present_checkout_at(
        &self,
        node_id: NodeId,
        path: &str,
    ) -> ApiResult<Option<NodeWorkspaceId>> {
        Ok(self
            .db
            .query_scalar_opt(
                "SELECT id FROM node_workspaces
             WHERE node_id = $1 AND path = $2 AND missing_at IS NULL",
                params![node_id, path],
            )
            .await?)
    }

    async fn checkout_location(
        &self,
        checkout: NodeWorkspaceId,
    ) -> ApiResult<Option<(String, NodeId)>> {
        Ok(self
            .db
            .query_opt(
                "SELECT path, node_id FROM node_workspaces
                 WHERE id = $1 AND missing_at IS NULL",
                params![checkout],
            )
            .await?)
    }

    async fn clone_path_on_node(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
        node: NodeId,
    ) -> ApiResult<Option<String>> {
        Ok(self
            .db
            .query_scalar_opt(
                // MAIN-222 AC-3: worktree from the CLONE, never from a worktree — a
                // deterministic, clone-only pick (kind + missing_at), not bare
                // discovered_at order that a delete/reinsert could reshuffle.
                "SELECT path FROM node_workspaces
         WHERE tenant_id = $1 AND workspace_id = $2 AND node_id = $3
           AND kind = 'clone' AND missing_at IS NULL
         ORDER BY discovered_at LIMIT 1",
                params![tenant, workspace, node],
            )
            .await?)
    }

    async fn git_remote_for_workspace(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
    ) -> ApiResult<Option<String>> {
        Ok(self
            .db
            .query_scalar_opt(
                "SELECT git_remote_url FROM node_workspaces
         WHERE tenant_id = $1 AND workspace_id = $2 AND git_remote_url IS NOT NULL LIMIT 1",
                params![tenant, workspace],
            )
            .await?)
    }

    async fn pick_tasks(
        &self,
        tenant: TenantId,
        viewer: UserId,
        p: PickParams,
    ) -> ApiResult<Vec<TaskItem>> {
        // Bound alongside the lists themselves, and read before they move.
        let (labels_len, types_len, vis_len) = (
            p.labels.len() as i64,
            p.types.len() as i64,
            p.visibility.len() as i64,
        );
        // Only a uuid spelled exactly as `b.id::text` would render it, so the
        // set of boards this matches is the same one Postgres matched before.
        let board_id: Option<uuid::Uuid> = p
            .board
            .as_deref()
            .and_then(|s| uuid::Uuid::parse_str(s).ok())
            .filter(|u| u.to_string() == p.board.as_deref().unwrap_or_default());
        Ok(self
            .db
            .query_all(
                &format!(
                    r#"
        SELECT t.* FROM tasks t
        JOIN boards b ON b.id = t.board_id
        JOIN board_columns c ON c.id = t.column_id
        WHERE t.tenant_id = $1
          -- The id leg compares uuid TO uuid ($24), parsed in Rust, rather than
          -- rendering the column as text (MAIN-435). A bound uuid reaches SQLite
          -- as a 16-byte BLOB even where the column is declared TEXT, so
          -- `CAST(b.id AS text)` yields those raw bytes and never equals the
          -- hyphenated string a caller passes — the board filter silently
          -- matched nothing there. $24 is NULL unless $2 is a uuid in exactly
          -- the canonical spelling `::text` would have produced, so which rows
          -- match is unchanged on Postgres.
          AND ({b_text} IS NULL OR b.id = $24 OR upper(b.key) = upper($2))
          AND ({ws} IS NULL OR t.workspace_id = $3)
          AND ({col_text} IS NULL OR c.type = $4)
          AND ({prio_int}  IS NULL OR t.priority = $5)
          AND (NOT {unassigned_bool} OR t.assignee_user_id IS NULL)
          AND ({assignee} IS NULL OR t.assignee_user_id = $7)
          -- every required label must be present.
          --
          -- The list's LENGTH travels as an ordinary bound integer ($21/$22/$23)
          -- rather than `cardinality(<list>)` (MAIN-435). SQLite has no
          -- `cardinality`, and more to the point a list parameter is expanded
          -- in place to one placeholder per element, so any reference to it
          -- OUTSIDE an `= ANY(…)` reaches the driver as `$a, $b` — a bare
          -- expression list, which is a syntax error wherever one value was
          -- expected. Postgres binds the same integer, so the comparison is
          -- unchanged there.
          AND ($21 = 0 OR (
                SELECT count(DISTINCT l.name) FROM task_labels tl
                JOIN labels l ON l.id = tl.label_id
                WHERE tl.task_id = t.id AND l.name = ANY($8)
              ) = $21)
          -- and none of the excluded ones
          AND NOT EXISTS (
                SELECT 1 FROM task_labels tl
                JOIN labels l ON l.id = tl.label_id
                WHERE tl.task_id = t.id AND l.name = ANY($9))
          -- blocked is DERIVED: an unfinished task pointing here with `blocks`
          AND ({blocked_bool} IS NULL OR $10 = EXISTS (
                SELECT 1 FROM task_relations r
                JOIN tasks bt ON bt.id = r.from_task
                JOIN board_columns bc ON bc.id = bt.column_id
                WHERE r.to_task = t.id AND r.kind = 'blocks'
                  AND bc.type NOT IN ('completed', 'canceled')))
          AND ({created} IS NULL OR t.created_at > $11)
          -- archived work is off the board and NEVER pickable unless explicitly asked for
          AND ({archived_bool} OR t.archived_at IS NULL)
          -- free-text search across title, description, and display key (MAIN-54).
          -- Substring, case-insensitive; ANDs with every filter above.
          AND ({q_text} IS NULL OR (
                    {title_match}
                 OR {desc_match}
                 OR {key_match}))
          -- issue-type filter (MAIN-59) + epic exclusion (MAIN-80): with an
          -- explicit type filter the requested types pass (so `type=epic`
          -- surfaces epics on purpose); with no type filter, everything EXCEPT
          -- `epic` passes — an epic is a container, never a unit of work the
          -- loop should pick. Labels (incl. agent-ready) have no bearing.
          AND (t.type = ANY($15)
               OR ($22 = 0 AND t.type <> 'epic'))
          -- per-task visibility (MAIN-76): a `private` card is seen only by its
          -- creator or assignee; `team`/`org` are tenant-visible. Same predicate
          -- an agent's claim path enforces, so the list never shows work it
          -- could not then start. ONE definition, shared (MAIN-265).
          AND {visible}
          -- explicit visibility filter (MAIN-103 AC-3): ANDs with the viewer
          -- predicate above, so it can only NARROW — `visibility=private` still
          -- shows only the caller's own private cards, never a teammate's.
          AND ($23 = 0 OR t.visibility = ANY($19))
          -- epic children (MAIN-81): when a parent is given, restrict to its
          -- tickets. This spans every column (children live in backlog and on
          -- the board), so it deliberately does NOT constrain the column type.
          -- BOTH this and the visibility predicate apply, so an epic's children
          -- are still filtered to what the viewer may see.
          AND ({parent} IS NULL OR t.parent_task_id = $17)
          -- node affinity. Dispatch used to set `assigned_node_id` and nothing
          -- read it — every builder on every machine saw every card, so
          -- "dispatch to a node" moved a card to Todo and otherwise did
          -- nothing. This is the clause that makes it mean something: asked BY
          -- a node, a dispatched card belongs to the node it names and an
          -- undispatched one is fair game. Asked by a person ($20 IS NULL) the
          -- whole board still comes back, so dispatch narrows the loop's view
          -- without hiding anything from a human.
          AND ($20 IS NULL
               OR t.assigned_node_id IS NULL
               OR t.assigned_node_id = $20)
          -- backlog exclusion (MAIN-80): a `backlog`-type column is the human
          -- refinement space; the loop never draws from it unless `backlog=true`
          -- ($18). AC-3: a `parent=` query (an epic's children, which span
          -- backlog and board — $17 present) LIFTS this exclusion, so listing an
          -- epic's tickets never silently drops the ones still in triage.
          AND ({backlog_bool} OR {parent} IS NOT NULL OR c.type <> 'backlog')
          -- finished-work exclusion (MAIN-464): a card in a `completed` or
          -- `canceled` column is over, and the pick is a question about work
          -- that remains. MAIN-80 excluded the backlog end of the board and
          -- stopped there, so `agent-ready` left on a merged card fed it back
          -- to the next builder — twice in the wild (MAIN-441, MAIN-302).
          -- Labels have no bearing, exactly as with the backlog.
          --
          -- Two lifts, both because the caller has already said it wants the
          -- finished end: `parent=` (an epic's tickets, whose done/total is the
          -- point of listing them — the same lift MAIN-80 AC-3 gives backlog)
          -- and naming one of the two types in `column_type` ($4). Without the
          -- second, `column_type=completed` would answer "none", which is the
          -- same silent lie in the other direction.
          AND ({done_bool}
               OR {parent} IS NOT NULL
               OR {col_text} IN ('completed', 'canceled')
               OR c.type NOT IN ('completed', 'canceled'))
        -- priority 0 means "unset", which sorts last rather than first
        ORDER BY CASE WHEN t.priority = 0 THEN 5 ELSE t.priority END, t.created_at
        LIMIT $12
        "#,
                    ws = type_mapping(self.db.engine()).cast("$3", "uuid"),
                    assignee = type_mapping(self.db.engine()).cast("$7", "uuid"),
                    created = type_mapping(self.db.engine()).cast("$11", "timestamptz"),
                    parent = type_mapping(self.db.engine()).cast("$17", "uuid"),
                    b_text = type_mapping(self.db.engine()).cast("$2", "text"),
                    col_text = type_mapping(self.db.engine()).cast("$4", "text"),
                    prio_int = type_mapping(self.db.engine()).cast("$5", "int"),
                    unassigned_bool = type_mapping(self.db.engine()).cast("$6", "bool"),
                    blocked_bool = type_mapping(self.db.engine()).cast("$10", "bool"),
                    archived_bool = type_mapping(self.db.engine()).cast("$13", "bool"),
                    q_text = type_mapping(self.db.engine()).cast("$14", "text"),
                    title_match = ci_match(self.db.engine()).ci_match("t.title", "$14"),
                    desc_match = ci_match(self.db.engine()).ci_match("t.description", "$14"),
                    key_match = ci_match(self.db.engine()).ci_match(
                        &format!(
                            "(b.key || '-' || {})",
                            type_mapping(self.db.engine()).cast("t.number", "text")
                        ),
                        "$14"
                    ),
                    backlog_bool = type_mapping(self.db.engine()).cast("$18", "bool"),
                    done_bool = type_mapping(self.db.engine()).cast("$25", "bool"),
                    visible = crate::services::tasks::visible_sql("t", "$16"),
                ),
                params![
                    tenant,
                    p.board,
                    p.workspace,
                    p.column_type,
                    p.priority,
                    p.unassigned_only,
                    p.assignee,
                    DbValue::TextList(p.labels),
                    DbValue::TextList(p.not_labels),
                    p.is_blocked,
                    p.created_after,
                    p.limit,
                    p.archived,
                    p.q,
                    DbValue::TextList(p.types),
                    viewer,
                    p.parent,
                    p.backlog,
                    DbValue::TextList(p.visibility),
                    p.node,
                    labels_len,
                    types_len,
                    vis_len,
                    board_id,
                    p.done
                ],
            )
            .await?)
    }

    async fn board_automation(
        &self,
        board: BoardId,
        tenant: TenantId,
    ) -> ApiResult<Option<serde_json::Value>> {
        Ok(self
            .db
            .query_scalar_opt(
                "SELECT automation FROM boards WHERE id = $1 AND tenant_id = $2",
                params![board, tenant],
            )
            .await?)
    }

    async fn visibility_of(&self, task: TaskId) -> ApiResult<Option<String>> {
        Ok(self
            .db
            .query_scalar_opt("SELECT visibility FROM tasks WHERE id = $1", params![task])
            .await?)
    }

    async fn task_ref(&self, task: TaskId) -> ApiResult<Option<(Option<String>, i32, String)>> {
        Ok(self
            .db
            .query_opt(
                "SELECT b.key, t.number, t.title
                   FROM tasks t JOIN boards b ON b.id = t.board_id WHERE t.id = $1",
                params![task],
            )
            .await?)
    }

    async fn column_type_of(&self, column: ColumnId) -> ApiResult<Option<String>> {
        Ok(self
            .db
            .query_scalar_opt(
                "SELECT c.type FROM board_columns c WHERE c.id = $1",
                params![column],
            )
            .await?)
    }

    async fn column_type_in_tenant(
        &self,
        column: ColumnId,
        tenant: TenantId,
    ) -> ApiResult<Option<String>> {
        Ok(self
            .db
            .query_scalar_opt(
                "SELECT c.type FROM board_columns c
         JOIN boards b ON b.id = c.board_id
         WHERE c.id = $1 AND b.tenant_id = $2",
                params![column, tenant],
            )
            .await?)
    }

    async fn board_of_task(&self, id: TaskId) -> ApiResult<Option<BoardId>> {
        Ok(self
            .db
            .query_scalar_opt("SELECT board_id FROM tasks WHERE id = $1", params![id])
            .await?)
    }

    async fn claim_task(
        &self,
        id: TaskId,
        tenant: TenantId,
        assignee: UserId,
        column: Option<ColumnId>,
        lease_secs: Option<i64>,
    ) -> ApiResult<Option<TaskItem>> {
        Ok(self
            .db
            .query_opt(
                &format!(
                    "UPDATE tasks SET
             assignee_user_id = $1,
             column_id = coalesce($2, column_id),
             claim_expires_at = CASE WHEN {ttl} IS NULL THEN claim_expires_at ELSE {lease} END,
             updated_at = {now}
         WHERE id = $3 AND tenant_id = $4 AND assignee_user_id IS NULL
         RETURNING *",
                    ttl = type_mapping(self.db.engine()).cast("$5", "bigint"),
                    lease = time_math(self.db.engine()).now_plus_scaled(
                        &type_mapping(self.db.engine()).cast("$5", "bigint"),
                        "1 second"
                    ),
                    now = type_mapping(self.db.engine()).now()
                ),
                params![assignee, column.map(|c| c.0), id, tenant, lease_secs],
            )
            .await?)
    }

    async fn assignee_of(&self, id: TaskId, tenant: TenantId) -> ApiResult<Option<Option<UserId>>> {
        Ok(self
            .db
            .query_scalar_opt(
                "SELECT assignee_user_id FROM tasks WHERE id = $1 AND tenant_id = $2",
                params![id, tenant],
            )
            .await?)
    }

    async fn release_assignment(
        &self,
        id: TaskId,
        tenant: TenantId,
    ) -> ApiResult<Option<TaskItem>> {
        Ok(self
            .db
            .query_opt(
                &format!(
                    "UPDATE tasks SET assignee_user_id = NULL, claim_expires_at = NULL,
                updated_at = {}
         WHERE id = $1 AND tenant_id = $2 RETURNING *",
                    type_mapping(self.db.engine()).now()
                ),
                params![id, tenant],
            )
            .await?)
    }

    async fn release_claim_of(
        &self,
        id: TaskId,
        tenant: TenantId,
        holder: UserId,
    ) -> ApiResult<bool> {
        let rows = self
            .db
            .exec(
                &format!(
                    "UPDATE tasks SET assignee_user_id = NULL, claim_expires_at = NULL,
                updated_at = {}
         WHERE id = $1 AND tenant_id = $2 AND assignee_user_id = $3
           AND claim_expires_at IS NOT NULL",
                    type_mapping(self.db.engine()).now()
                ),
                params![id, tenant, holder],
            )
            .await?;
        Ok(rows > 0)
    }

    async fn build_failures(&self, id: TaskId, tenant: TenantId) -> ApiResult<i32> {
        Ok(self
            .db
            .query_scalar_opt(
                "SELECT build_failure_strikes FROM tasks WHERE id = $1 AND tenant_id = $2",
                params![id, tenant],
            )
            .await?
            .unwrap_or(0))
    }

    async fn bump_build_failures(&self, id: TaskId, tenant: TenantId) -> ApiResult<i32> {
        Ok(self
            .db
            .query_scalar_opt(
                "UPDATE tasks SET build_failure_strikes = build_failure_strikes + 1
         WHERE id = $1 AND tenant_id = $2 RETURNING build_failure_strikes",
                params![id, tenant],
            )
            .await?
            .unwrap_or(0))
    }

    async fn clear_build_failures(
        &self,
        id: TaskId,
        tenant: TenantId,
        at_least: i32,
    ) -> ApiResult<()> {
        self.db
            .exec(
                "UPDATE tasks SET build_failure_strikes = 0
         WHERE id = $1 AND tenant_id = $2 AND build_failure_strikes >= $3",
                params![id, tenant, at_least],
            )
            .await?;
        Ok(())
    }

    async fn set_agent_ready(&self, tenant: TenantId, id: TaskId, on: bool) -> ApiResult<()> {
        let label_id: Uuid = self
            .db
            .query_scalar(
                "INSERT INTO labels (id, tenant_id, name, color)
         VALUES ($1, $2, 'agent-ready', '#f0a000')
         ON CONFLICT (tenant_id, name) DO UPDATE SET name = EXCLUDED.name
         RETURNING id",
                params![Uuid::now_v7(), tenant],
            )
            .await?;
        if on {
            self.db
                .exec(
                    "INSERT INTO task_labels (task_id, label_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
                    params![id, label_id],
                )
                .await?;
        } else {
            self.db
                .exec(
                    "DELETE FROM task_labels WHERE task_id = $1 AND label_id = $2",
                    params![id, label_id],
                )
                .await?;
        }
        Ok(())
    }

    async fn list_labels(&self, tenant: TenantId) -> ApiResult<Vec<Label>> {
        Ok(self
            .db
            .query_all(
                "SELECT id, tenant_id, name, color, created_at FROM labels
         WHERE tenant_id = $1 ORDER BY name",
                params![tenant],
            )
            .await?)
    }

    async fn upsert_label(&self, tenant: TenantId, name: &str, color: &str) -> ApiResult<Label> {
        Ok(self
            .db
            .query_one(
                "INSERT INTO labels (id, tenant_id, name, color) VALUES ($1, $2, $3, $4)
         ON CONFLICT (tenant_id, name) DO UPDATE SET name = EXCLUDED.name
         RETURNING id, tenant_id, name, color, created_at",
                params![Uuid::now_v7(), tenant, name, color],
            )
            .await?)
    }

    async fn delete_label(&self, id: Uuid, tenant: TenantId) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                "DELETE FROM labels WHERE id = $1 AND tenant_id = $2",
                params![id, tenant],
            )
            .await?)
    }

    async fn label_id_by_uuid(&self, id: Uuid, tenant: TenantId) -> ApiResult<Option<Uuid>> {
        let found: Option<(Uuid,)> = self
            .db
            .query_opt(
                "SELECT id FROM labels WHERE id = $1 AND tenant_id = $2",
                params![id, tenant],
            )
            .await?;
        Ok(found.map(|r| r.0))
    }

    async fn label_id_by_name(&self, tenant: TenantId, name: &str) -> ApiResult<Option<Uuid>> {
        let found: Option<(Uuid,)> = self
            .db
            .query_opt(
                "SELECT id FROM labels WHERE tenant_id = $1 AND name = $2",
                params![tenant, name],
            )
            .await?;
        Ok(found.map(|r| r.0))
    }

    async fn labels_of_task(&self, task: TaskId) -> ApiResult<Vec<Label>> {
        Ok(self
            .db
            .query_all(
                "SELECT l.id, l.tenant_id, l.name, l.color, l.created_at
         FROM task_labels tl JOIN labels l ON l.id = tl.label_id
         WHERE tl.task_id = $1 ORDER BY l.name",
                params![task],
            )
            .await?)
    }

    async fn attach_label_id(&self, task: TaskId, label: Uuid) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                "INSERT INTO task_labels (task_id, label_id) VALUES ($1, $2)
         ON CONFLICT DO NOTHING",
                params![task, label],
            )
            .await?)
    }

    async fn detach_label_id(&self, task: TaskId, label: Uuid) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                "DELETE FROM task_labels WHERE task_id = $1 AND label_id = $2",
                params![task, label],
            )
            .await?)
    }

    async fn task_naming(
        &self,
        task: TaskId,
    ) -> ApiResult<Option<(String, Option<i32>, Option<String>)>> {
        Ok(self
            .db
            .query_opt(
                "SELECT t.title, t.number, b.key FROM tasks t
         JOIN boards b ON b.id = t.board_id
         WHERE t.id = $1",
                params![task],
            )
            .await?)
    }

    async fn create_board(
        &self,
        tenant: TenantId,
        workspace: Option<Uuid>,
        name: &str,
        key: &str,
    ) -> ApiResult<Board> {
        Ok(self
            .db
            .query_one(
                "INSERT INTO boards (id, tenant_id, workspace_id, name, key, provider)
         VALUES ($1, $2, $3, $4, $5, 'local') RETURNING *",
                params![BoardId::new(), tenant, workspace, name, key],
            )
            .await?)
    }

    async fn create_column(
        &self,
        board: BoardId,
        name: &str,
        position: i32,
        type_: &str,
    ) -> ApiResult<BoardColumn> {
        Ok(self
            .db
            .query_one(
                "INSERT INTO board_columns (id, board_id, name, position, type)
             VALUES ($1, $2, $3, $4, $5) RETURNING *",
                params![ColumnId::new(), board, name, position, type_],
            )
            .await?)
    }

    async fn set_archived(
        &self,
        id: TaskId,
        tenant: TenantId,
        archived: bool,
    ) -> ApiResult<Option<TaskItem>> {
        Ok(self
            .db
            .query_opt(
                &format!(
                    "UPDATE tasks SET archived_at = CASE WHEN $3 THEN {now} ELSE NULL END,
                          updated_at = {now}
         WHERE id = $1 AND tenant_id = $2 RETURNING *",
                    now = type_mapping(self.db.engine()).now()
                ),
                params![id, tenant, archived],
            )
            .await?)
    }

    async fn archive_all_in_column(
        &self,
        column: ColumnId,
        tenant: TenantId,
    ) -> ApiResult<Vec<TaskId>> {
        Ok(self
            .db
            .query_scalar_all(
                &format!(
                    "UPDATE tasks SET archived_at = {now}, updated_at = {now}
         WHERE column_id = $1 AND tenant_id = $2 AND archived_at IS NULL
         RETURNING id",
                    now = type_mapping(self.db.engine()).now()
                ),
                params![column, tenant],
            )
            .await?)
    }

    async fn delete_task(&self, id: TaskId, tenant: TenantId) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                "DELETE FROM tasks WHERE id = $1 AND tenant_id = $2",
                params![id, tenant],
            )
            .await?)
    }

    async fn update_board(
        &self,
        id: BoardId,
        tenant: TenantId,
        name: &str,
        key: Option<String>,
        automation: Option<serde_json::Value>,
    ) -> ApiResult<Option<Board>> {
        Ok(self
            .db
            .query_opt(
                &format!(
                    "UPDATE boards SET name = $3, key = COALESCE($4, key),
                           automation = COALESCE($5, automation), updated_at = {}
         WHERE id = $1 AND tenant_id = $2 RETURNING *",
                    type_mapping(self.db.engine()).now()
                ),
                params![id, tenant, name, key, automation],
            )
            .await?)
    }

    async fn board_key_taken(&self, tenant: TenantId, key: &str) -> ApiResult<bool> {
        let taken: Option<Uuid> = self
            .db
            .query_scalar_opt(
                "SELECT id FROM boards WHERE tenant_id = $1 AND key = $2",
                params![tenant, key],
            )
            .await?;
        Ok(taken.is_some())
    }

    async fn comments_of(&self, task: TaskId) -> ApiResult<Vec<TaskComment>> {
        Ok(self
            .db
            .query_all(
                "SELECT id, tenant_id, task_id, author_type, author_id, author_name,
                body_md, created_at, updated_at
         FROM task_comments WHERE task_id = $1 ORDER BY created_at",
                params![task],
            )
            .await?)
    }

    async fn create_comment(&self, new: NewComment) -> ApiResult<TaskComment> {
        Ok(self
            .db
            .query_one(
                "INSERT INTO task_comments (id, tenant_id, task_id, author_type, author_id, author_name, body_md)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         RETURNING id, tenant_id, task_id, author_type, author_id, author_name,
                   body_md, created_at, updated_at",
                params![
                    Uuid::now_v7(),
                    new.tenant,
                    new.task,
                    new.author_type,
                    new.author_id,
                    new.author_name,
                    new.body_md
                ],
            )
            .await?)
    }

    async fn add_description_revision(
        &self,
        tenant: TenantId,
        task: TaskId,
        body: &str,
        author: Option<Uuid>,
    ) -> ApiResult<()> {
        self.db
            .exec(
                "INSERT INTO task_description_revisions (id, tenant_id, task_id, body, author_id)
         VALUES ($1, $2, $3, $4, $5)",
                params![Uuid::now_v7(), tenant, task, body, author],
            )
            .await?;
        Ok(())
    }

    async fn description_revisions_of(
        &self,
        tenant: TenantId,
        task: TaskId,
    ) -> ApiResult<Vec<TaskDescriptionRevision>> {
        Ok(self
            .db
            .query_all(
                "SELECT id, tenant_id, task_id, body, author_id, created_at
         FROM task_description_revisions
         WHERE tenant_id = $1 AND task_id = $2
         ORDER BY created_at DESC, id DESC",
                params![tenant, task],
            )
            .await?)
    }

    async fn task_visibility_naming(
        &self,
        task: TaskId,
        tenant: TenantId,
    ) -> ApiResult<Option<(String, Option<i32>, Option<String>)>> {
        Ok(self
            .db
            .query_opt(
                "SELECT t.visibility, t.number, b.key FROM tasks t
         JOIN boards b ON b.id = t.board_id
         WHERE t.id = $1 AND t.tenant_id = $2",
                params![task, tenant],
            )
            .await?)
    }

    async fn update_comment(
        &self,
        id: Uuid,
        tenant: TenantId,
        body_md: &str,
    ) -> ApiResult<TaskComment> {
        Ok(self
            .db
            .query_one(
                &format!(
                    "UPDATE task_comments SET body_md = $1, updated_at = {}
         WHERE id = $2 AND tenant_id = $3
         RETURNING id, tenant_id, task_id, author_type, author_id, author_name,
                   body_md, created_at, updated_at",
                    type_mapping(self.db.engine()).now()
                ),
                params![body_md, id, tenant],
            )
            .await?)
    }

    async fn delete_comment(&self, id: Uuid, tenant: TenantId) -> ApiResult<()> {
        self.db
            .exec(
                "DELETE FROM task_comments WHERE id = $1 AND tenant_id = $2",
                params![id, tenant],
            )
            .await?;
        Ok(())
    }

    async fn comment_author(
        &self,
        id: Uuid,
        tenant: TenantId,
    ) -> ApiResult<Option<(Option<Uuid>, TaskId)>> {
        Ok(self
            .db
            .query_opt(
                "SELECT author_id, task_id FROM task_comments WHERE id = $1 AND tenant_id = $2",
                params![id, tenant],
            )
            .await?)
    }

    async fn upsert_relation(
        &self,
        tenant: TenantId,
        from: TaskId,
        to: TaskId,
        kind: &str,
    ) -> ApiResult<TaskRelation> {
        Ok(self
            .db
            .query_one(
                "INSERT INTO task_relations (id, tenant_id, from_task, to_task, kind)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (from_task, to_task, kind) DO UPDATE SET kind = EXCLUDED.kind
         RETURNING id, tenant_id, from_task, to_task, kind, created_at",
                params![Uuid::now_v7(), tenant, from, to, kind],
            )
            .await?)
    }

    async fn blocks_reaches(&self, start: TaskId, target: TaskId) -> ApiResult<bool> {
        let hit: Option<bool> = self
            .db
            .query_scalar_opt(
                "WITH RECURSIVE reachable(id) AS (
             SELECT to_task FROM task_relations WHERE from_task = $1 AND kind = 'blocks'
             UNION
             SELECT r.to_task FROM task_relations r
             JOIN reachable p ON r.from_task = p.id
             WHERE r.kind = 'blocks'
         )
         SELECT true FROM reachable WHERE id = $2 LIMIT 1",
                params![start, target],
            )
            .await?;
        Ok(hit.is_some())
    }

    async fn delete_relation(&self, id: Uuid, tenant: TenantId) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                "DELETE FROM task_relations WHERE id = $1 AND tenant_id = $2",
                params![id, tenant],
            )
            .await?)
    }

    async fn epic_children(&self, parent: TaskId, viewer: UserId) -> ApiResult<Vec<EpicChild>> {
        Ok(self
            .db
            .query_all(
                &format!(
                    "SELECT t.id,
                    (b.key || '-' || {number}) AS key,
                    t.title, t.type, t.priority,
                    bc.type AS column_type,
                    t.archived_at
             FROM tasks t
             JOIN boards b ON b.id = t.board_id
             JOIN board_columns bc ON bc.id = t.column_id
             WHERE t.parent_task_id = $1
               AND {visible}
             ORDER BY CASE WHEN t.priority = 0 THEN 5 ELSE t.priority END, t.created_at",
                    number = type_mapping(self.db.engine()).cast("t.number", "text"),
                    visible = crate::services::tasks::visible_sql("t", "$2"),
                ),
                params![parent, viewer],
            )
            .await?)
    }

    async fn key_of(&self, tenant: TenantId, id: TaskId) -> ApiResult<Option<String>> {
        Ok(self
            .db
            .query_scalar_opt(
                "SELECT b.key || '-' || t.number
                   FROM tasks t JOIN boards b ON b.id = t.board_id
                  WHERE t.id = $1 AND t.tenant_id = $2",
                params![id, tenant],
            )
            .await?)
    }

    async fn task_keys_at_worktree(&self, node: NodeId, path: &str) -> ApiResult<Vec<String>> {
        let rows: Vec<(Option<String>, Option<i32>)> = self
            .db
            .query_all(
                "SELECT b.key, t.number
                   FROM tasks t JOIN boards b ON b.id = t.board_id
                  WHERE t.worktree_node_id = $1 AND t.worktree_path = $2",
                params![node, path],
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|(key, number)| format!("{}-{}", key.unwrap_or_default(), number.unwrap_or(0)))
            .collect())
    }

    async fn related_tasks(&self, task: TaskId, viewer: UserId) -> ApiResult<Vec<RelatedTask>> {
        Ok(self
            .db
            .query_all(
                &format!(
                    "SELECT r.id AS relation_id, t.id, t.title,
                CASE WHEN b.key IS NOT NULL AND t.number IS NOT NULL
                     THEN b.key || '-' || t.number END AS key,
                CASE WHEN r.kind = 'blocks' AND r.to_task = $1 THEN 'blocked_by'
                     WHEN r.kind = 'blocks' THEN 'blocking'
                     ELSE r.kind END AS kind,
                c.type AS column_type
         FROM task_relations r
         JOIN tasks t ON t.id = CASE WHEN r.from_task = $1 THEN r.to_task ELSE r.from_task END
         JOIN boards b ON b.id = t.board_id
         JOIN board_columns c ON c.id = t.column_id
         WHERE (r.from_task = $1 OR r.to_task = $1)
           AND {visible}
         ORDER BY t.number",
                    visible = crate::services::tasks::visible_sql("t", "$2"),
                ),
                params![task, viewer],
            )
            .await?)
    }

    async fn delete_board(&self, id: BoardId, tenant: TenantId) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                "DELETE FROM boards WHERE id = $1 AND tenant_id = $2",
                params![id, tenant],
            )
            .await?)
    }

    async fn board_in_tenant(&self, id: BoardId, tenant: TenantId) -> ApiResult<bool> {
        let owned: Option<BoardId> = self
            .db
            .query_scalar_opt(
                "SELECT id FROM boards WHERE id = $1 AND tenant_id = $2",
                params![id, tenant],
            )
            .await?;
        Ok(owned.is_some())
    }

    async fn max_column_position(&self, board: BoardId) -> ApiResult<Option<i32>> {
        Ok(self
            .db
            .query_scalar(
                "SELECT max(position) FROM board_columns WHERE board_id = $1",
                params![board],
            )
            .await?)
    }

    async fn append_column(
        &self,
        board: BoardId,
        name: &str,
        position: i32,
    ) -> ApiResult<BoardColumn> {
        Ok(self
            .db
            .query_one(
                "INSERT INTO board_columns (id, board_id, name, position)
         VALUES ($1, $2, $3, $4) RETURNING *",
                params![ColumnId::new(), board, name, position],
            )
            .await?)
    }

    async fn update_column(
        &self,
        id: ColumnId,
        tenant: TenantId,
        name: Option<String>,
        position: Option<i32>,
    ) -> ApiResult<Option<BoardColumn>> {
        Ok(self
            .db
            .query_opt(
                "UPDATE board_columns SET
            name = COALESCE($2, name),
            position = COALESCE($3, position)
         WHERE id = $1 AND board_id IN (SELECT id FROM boards WHERE tenant_id = $4)
         RETURNING *",
                params![id, name, position, tenant],
            )
            .await?)
    }

    async fn delete_column(&self, id: ColumnId, tenant: TenantId) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                "DELETE FROM board_columns
         WHERE id = $1 AND board_id IN (SELECT id FROM boards WHERE tenant_id = $2)",
                params![id, tenant],
            )
            .await?)
    }

    async fn board_provider(&self, id: BoardId, tenant: TenantId) -> ApiResult<Option<String>> {
        Ok(self
            .db
            .query_scalar_opt(
                "SELECT provider FROM boards WHERE id = $1 AND tenant_id = $2",
                params![id, tenant],
            )
            .await?)
    }
}

/// An in-memory [`TaskRepository`] for tests that should not need a database
/// (MAIN-248 AC-3).
///
/// Faithful where the behaviour under test lives — per-board number allocation,
/// the tri-state edit and its optimistic-concurrency precondition, label
/// get-or-create, parent/child counting — and deliberately simple elsewhere. It
/// is a test double, not a second Postgres: no foreign keys, no collation, and
/// `board_tasks` sorts by position without reproducing tie-breaks exactly.
///
/// One `Mutex` around the whole state: tests are not contended, and a lock per
/// table invites a deadlock that only shows up in CI.
#[derive(Default)]
pub struct FakeTaskRepository {
    inner: std::sync::Mutex<FakeState>,
}

#[derive(Default)]
struct FakeState {
    boards: Vec<Board>,
    /// `next_number` per board — the counter the real `FOR UPDATE` protects.
    next_number: HashMap<Uuid, i32>,
    columns: Vec<BoardColumn>,
    tasks: Vec<TaskItem>,
    /// `(task, label_name)`
    task_labels: Vec<(Uuid, String)>,
    /// Consecutive build runs that concluded nothing, per task (MAIN-489).
    build_failures: HashMap<Uuid, i32>,
    comments: Vec<TaskComment>,
    desc_revisions: Vec<TaskDescriptionRevision>,
    labels: Vec<Label>,
    relations: Vec<TaskRelation>,
    /// `(checkout, tenant, workspace, node, path, kind, present, remote)`
    checkouts: Vec<Checkout>,
}

/// The fake's reading of the guard both merge-sweep writes carry: the card sits
/// in a column whose type is neither `completed` nor `canceled`. A card whose
/// column is not in the fake's board at all is NOT in flight — the same answer
/// the real `EXISTS` gives when the join finds nothing.
fn in_flight(st: &FakeState, task: &TaskItem) -> bool {
    st.columns
        .iter()
        .any(|c| c.id == task.column_id && !matches!(c.r#type.as_str(), "completed" | "canceled"))
}

struct Checkout {
    id: NodeWorkspaceId,
    tenant: TenantId,
    workspace: WorkspaceId,
    node: NodeId,
    path: String,
    kind: String,
    present: bool,
    remote: Option<String>,
}

impl FakeTaskRepository {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed a board with a key, so tasks on it get real `BOARD-N` keys.
    pub fn with_board(&self, tenant: TenantId, name: &str, key: &str) -> Board {
        let mut st = self.inner.lock().unwrap();
        let now = chrono::Utc::now();
        let b = Board {
            id: BoardId::new(),
            tenant_id: tenant,
            workspace_id: None,
            name: name.into(),
            key: Some(key.into()),
            provider: "local".into(),
            automation: serde_json::json!({}),
            created_at: now,
            updated_at: now,
        };
        st.next_number.insert(b.id.0, 1);
        st.boards.push(b.clone());
        b
    }

    /// Seed a column of a given type.
    pub fn with_column(&self, board: BoardId, name: &str, type_: &str, position: i32) -> ColumnId {
        let mut st = self.inner.lock().unwrap();
        let c = BoardColumn {
            id: ColumnId::new(),
            board_id: board,
            name: name.into(),
            position,
            r#type: type_.into(),
        };
        let id = c.id;
        st.columns.push(c);
        id
    }

    /// Seed a checkout for the worktree lookups.
    #[allow(clippy::too_many_arguments)]
    pub fn with_checkout(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
        node: NodeId,
        path: &str,
        kind: &str,
        present: bool,
        remote: Option<&str>,
    ) -> NodeWorkspaceId {
        let id = NodeWorkspaceId::new();
        self.inner.lock().unwrap().checkouts.push(Checkout {
            id,
            tenant,
            workspace,
            node,
            path: path.into(),
            kind: kind.into(),
            present,
            remote: remote.map(str::to_string),
        });
        id
    }

    /// The label names attached to a task — so a test can assert what a create
    /// actually filed.
    pub fn labels_of(&self, task: TaskId) -> Vec<String> {
        let st = self.inner.lock().unwrap();
        let mut v: Vec<String> = st
            .task_labels
            .iter()
            .filter(|(t, _)| *t == task.0)
            .map(|(_, n)| n.clone())
            .collect();
        v.sort();
        v
    }
}

#[async_trait]
impl TaskRepository for FakeTaskRepository {
    async fn board_keys(&self, board_ids: &[Uuid]) -> ApiResult<HashMap<Uuid, Option<String>>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .boards
            .iter()
            .filter(|b| board_ids.contains(&b.id.0))
            .map(|b| (b.id.0, b.key.clone()))
            .collect())
    }

    async fn operator_visible_titles(
        &self,
        tenant: TenantId,
        limit: i64,
    ) -> ApiResult<Vec<String>> {
        let st = self.inner.lock().unwrap();
        let mut rows: Vec<&TaskItem> = st
            .tasks
            .iter()
            .filter(|t| t.tenant_id == tenant && t.visibility != "private")
            .collect();
        rows.sort_by_key(|t| std::cmp::Reverse(t.created_at));
        Ok(rows
            .into_iter()
            .take(limit.max(0) as usize)
            .map(|t| t.title.clone())
            .collect())
    }

    async fn labels_for_tasks(&self, task_ids: &[Uuid]) -> ApiResult<Vec<TaskLabelRow>> {
        let st = self.inner.lock().unwrap();
        let tenant = st.tasks.first().map(|t| t.tenant_id).unwrap_or_default();
        let mut rows: Vec<TaskLabelRow> = st
            .task_labels
            .iter()
            .filter(|(t, _)| task_ids.contains(t))
            .map(|(t, name)| {
                (
                    *t,
                    // Stable per name, so two tasks with the same label share an id.
                    Uuid::new_v5(&Uuid::NAMESPACE_OID, name.as_bytes()),
                    tenant,
                    name.clone(),
                    "#888888".to_string(),
                    chrono::Utc::now(),
                )
            })
            .collect();
        rows.sort_by(|a, b| a.3.cmp(&b.3));
        Ok(rows)
    }

    async fn parent_info(&self, parent_ids: &[Uuid]) -> ApiResult<HashMap<Uuid, ParentInfo>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .tasks
            .iter()
            .filter(|t| parent_ids.contains(&t.id.0))
            .map(|t| {
                (
                    t.id.0,
                    (
                        t.number,
                        t.visibility.clone(),
                        t.created_by,
                        t.assignee_user_id,
                    ),
                )
            })
            .collect())
    }

    async fn id_by_uuid(&self, tenant: TenantId, id: Uuid) -> ApiResult<Option<TaskId>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .tasks
            .iter()
            .find(|t| t.id.0 == id && t.tenant_id == tenant)
            .map(|t| t.id))
    }

    async fn id_by_key(
        &self,
        tenant: TenantId,
        board_key: &str,
        number: i32,
    ) -> ApiResult<Option<TaskId>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .tasks
            .iter()
            .find(|t| {
                t.tenant_id == tenant
                    && t.number == Some(number)
                    && st.boards.iter().any(|b| {
                        b.id == t.board_id
                            && b.key.as_deref().map(str::to_uppercase)
                                == Some(board_key.to_uppercase())
                    })
            })
            .map(|t| t.id))
    }

    async fn column_of_type(
        &self,
        board: BoardId,
        column_type: &str,
    ) -> ApiResult<Option<ColumnId>> {
        let st = self.inner.lock().unwrap();
        let mut cs: Vec<&BoardColumn> = st
            .columns
            .iter()
            .filter(|c| c.board_id == board && c.r#type == column_type)
            .collect();
        cs.sort_by_key(|c| c.position);
        Ok(cs.first().map(|c| c.id))
    }

    async fn column_by_name(&self, board: BoardId, name: &str) -> ApiResult<Option<ColumnId>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .columns
            .iter()
            .find(|c| c.board_id == board && c.name.to_lowercase() == name.to_lowercase())
            .map(|c| c.id))
    }

    async fn column_at_position(&self, board: BoardId, offset: i64) -> ApiResult<Option<ColumnId>> {
        let st = self.inner.lock().unwrap();
        let mut cs: Vec<&BoardColumn> = st.columns.iter().filter(|c| c.board_id == board).collect();
        cs.sort_by_key(|c| c.position);
        Ok(cs.get(offset as usize).map(|c| c.id))
    }

    async fn first_column(&self, board: BoardId) -> ApiResult<Option<ColumnId>> {
        self.column_at_position(board, 0).await
    }

    async fn list_local_boards(&self, tenant: TenantId) -> ApiResult<Vec<Board>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .boards
            .iter()
            .filter(|b| b.tenant_id == tenant && b.provider == "local")
            .cloned()
            .collect())
    }

    async fn get_board(&self, tenant: TenantId, board: BoardId) -> ApiResult<Option<Board>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .boards
            .iter()
            .find(|b| b.id == board && b.tenant_id == tenant)
            .cloned())
    }

    async fn board_columns(&self, board: BoardId) -> ApiResult<Vec<BoardColumn>> {
        let st = self.inner.lock().unwrap();
        let mut cs: Vec<BoardColumn> = st
            .columns
            .iter()
            .filter(|c| c.board_id == board)
            .cloned()
            .collect();
        cs.sort_by(|a, b| (a.position, &a.name).cmp(&(b.position, &b.name)));
        Ok(cs)
    }

    async fn board_tasks(&self, board: BoardId) -> ApiResult<Vec<TaskItem>> {
        let st = self.inner.lock().unwrap();
        let mut ts: Vec<TaskItem> = st
            .tasks
            .iter()
            .filter(|t| t.board_id == board)
            .cloned()
            .collect();
        ts.sort_by_key(|t| (t.position, t.created_at));
        Ok(ts)
    }

    async fn board_provider_for_task(
        &self,
        tenant: TenantId,
        task_id: TaskId,
    ) -> ApiResult<Option<String>> {
        let st = self.inner.lock().unwrap();
        let Some(t) = st
            .tasks
            .iter()
            .find(|t| t.id == task_id && t.tenant_id == tenant)
        else {
            return Ok(None);
        };
        Ok(st
            .boards
            .iter()
            .find(|b| b.id == t.board_id)
            .map(|b| b.provider.clone()))
    }

    async fn get_row(&self, tenant: TenantId, id: TaskId) -> ApiResult<Option<TaskItem>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .tasks
            .iter()
            .find(|t| t.id == id && t.tenant_id == tenant)
            .cloned())
    }

    async fn task_shape(
        &self,
        tenant: TenantId,
        id: TaskId,
    ) -> ApiResult<Option<(String, BoardId, Option<TaskId>)>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .tasks
            .iter()
            .find(|t| t.id == id && t.tenant_id == tenant)
            .map(|t| (t.type_.clone(), t.board_id, t.parent_task_id)))
    }

    async fn count_children(&self, id: TaskId) -> ApiResult<i64> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .tasks
            .iter()
            .filter(|t| t.parent_task_id == Some(id))
            .count() as i64)
    }

    async fn max_position_in_column(&self, column: ColumnId) -> ApiResult<Option<i32>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .tasks
            .iter()
            .filter(|t| t.column_id == column)
            .map(|t| t.position)
            .max())
    }

    async fn tasks_with_pr(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
    ) -> ApiResult<Vec<(TaskId, i64, String)>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .tasks
            .iter()
            .filter(|t| {
                t.tenant_id == tenant
                    && t.workspace_id == Some(workspace)
                    && t.pr_url.is_some()
                    && t.archived_at.is_none()
                    && in_flight(&st, t)
                    && !st
                        .task_labels
                        .iter()
                        .any(|(task, name)| *task == t.id.0 && name == "blocked")
            })
            .filter_map(|t| Some((t.id, i64::from(t.number?), t.pr_url.clone()?)))
            .collect())
    }

    async fn in_flight_tasks_with_pr(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
    ) -> ApiResult<Vec<(TaskId, String)>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .tasks
            .iter()
            .filter(|t| {
                t.tenant_id == tenant
                    && t.workspace_id == Some(workspace)
                    && t.archived_at.is_none()
                    && in_flight(&st, t)
            })
            .filter_map(|t| Some((t.id, t.pr_url.clone()?)))
            .collect())
    }

    async fn create_task(&self, new: NewTask) -> ApiResult<TaskItem> {
        let mut st = self.inner.lock().unwrap();
        // The counter the real `FOR UPDATE` protects. Held under the same lock
        // as the insert, so the fake allocates each number exactly once too —
        // a fake that handed out duplicates would make a caller test pass while
        // the real race stayed broken.
        let number = *st.next_number.entry(new.board.0).or_insert(1);
        st.next_number.insert(new.board.0, number + 1);

        let now = chrono::Utc::now();
        let task = TaskItem {
            type_: new.type_,
            id: TaskId::new(),
            tenant_id: new.tenant,
            board_id: new.board,
            column_id: new.column_id,
            title: new.title,
            description: new.description,
            position: new.position,
            external_id: None,
            external_url: None,
            assignee_user_id: None,
            visibility: new.visibility,
            created_by: new.created_by.map(UserId),
            workspace_id: new.workspace_id.map(WorkspaceId),
            parent_task_id: new.parent_task_id.map(TaskId),
            assigned_node_id: None,
            claim_expires_at: None,
            branch: None,
            worktree_path: None,
            worktree_node_id: None,
            checkout_id: None,
            session_id: None,
            pr_url: None,
            needs_clone: false,
            priority: new.priority,
            number: Some(number),
            key: None,
            url: None,
            parent_key: None,
            labels: vec![],
            archived_at: None,
            created_at: now,
            updated_at: now,
        };
        for name in &new.labels {
            st.task_labels.push((task.id.0, name.clone()));
        }
        st.tasks.push(task.clone());
        Ok(task)
    }

    async fn update_fields(
        &self,
        tenant: TenantId,
        id: TaskId,
        edit: TaskEdit,
    ) -> ApiResult<Option<TaskItem>> {
        let mut st = self.inner.lock().unwrap();
        let Some(t) = st
            .tasks
            .iter_mut()
            .find(|t| t.id == id && t.tenant_id == tenant)
        else {
            return Ok(None);
        };
        // The precondition, reproduced: a guarded update whose `updated_at` has
        // moved matches no row, which is how the caller detects a lost race.
        if let Some(expected) = edit.expected_updated_at {
            if t.updated_at != expected {
                return Ok(None);
            }
        }
        if let Some(v) = edit.title {
            t.title = v;
        }
        if edit.description.is_some() {
            t.description = edit.description;
        }
        if let Some(v) = edit.column_id {
            t.column_id = ColumnId(v);
        }
        if let Some(v) = edit.position {
            t.position = v;
        }
        if let Some(v) = edit.assignee_user_id {
            t.assignee_user_id = Some(UserId(v));
        }
        if let Some(v) = edit.priority {
            t.priority = v;
        }
        // The flag, not the value: this is the clear-vs-omit distinction.
        if edit.set_workspace {
            t.workspace_id = edit.workspace_id.map(WorkspaceId);
        }
        if let Some(v) = edit.type_ {
            t.type_ = v;
        }
        if let Some(v) = edit.visibility {
            t.visibility = v;
        }
        if edit.set_parent {
            t.parent_task_id = edit.parent_task_id.map(TaskId);
        }
        t.updated_at = chrono::Utc::now();
        Ok(Some(t.clone()))
    }

    async fn clear_assignee(&self, tenant: TenantId, id: TaskId) -> ApiResult<TaskItem> {
        let mut st = self.inner.lock().unwrap();
        let t = st
            .tasks
            .iter_mut()
            .find(|t| t.id == id && t.tenant_id == tenant)
            .expect("task exists");
        t.assignee_user_id = None;
        t.claim_expires_at = None;
        t.updated_at = chrono::Utc::now();
        Ok(t.clone())
    }

    async fn set_priority(
        &self,
        tenant: TenantId,
        id: TaskId,
        priority: i32,
    ) -> ApiResult<TaskItem> {
        let mut st = self.inner.lock().unwrap();
        let t = st
            .tasks
            .iter_mut()
            .find(|t| t.id == id && t.tenant_id == tenant)
            .expect("task exists");
        t.priority = priority;
        t.updated_at = chrono::Utc::now();
        Ok(t.clone())
    }

    async fn assign_node_and_column(
        &self,
        id: TaskId,
        node: NodeId,
        column: Option<ColumnId>,
    ) -> ApiResult<TaskItem> {
        let mut st = self.inner.lock().unwrap();
        let t = st
            .tasks
            .iter_mut()
            .find(|t| t.id == id)
            .expect("task exists");
        t.assigned_node_id = Some(node);
        if let Some(column) = column {
            t.column_id = column;
        }
        t.updated_at = chrono::Utc::now();
        Ok(t.clone())
    }

    async fn record_started_work(&self, id: TaskId, work: StartedWork) -> ApiResult<TaskItem> {
        let mut st = self.inner.lock().unwrap();
        let t = st
            .tasks
            .iter_mut()
            .find(|t| t.id == id)
            .expect("task exists");
        t.workspace_id = Some(WorkspaceId(work.workspace_id));
        t.assigned_node_id = Some(work.node_id);
        t.branch = Some(work.branch);
        t.worktree_path = Some(work.worktree_path);
        t.worktree_node_id = Some(work.node_id);
        t.session_id = work.session_id.map(SessionId);
        t.column_id = work.column_id;
        t.checkout_id = work.checkout_id.map(NodeWorkspaceId);
        t.claim_expires_at =
            Some(chrono::Utc::now() + chrono::Duration::seconds(work.claim_ttl_secs));
        t.updated_at = chrono::Utc::now();
        Ok(t.clone())
    }

    async fn set_pr_url(&self, id: TaskId, url: &str, column: ColumnId) -> ApiResult<TaskItem> {
        let mut st = self.inner.lock().unwrap();
        let t = st
            .tasks
            .iter_mut()
            .find(|t| t.id == id)
            .expect("task exists");
        t.pr_url = Some(url.into());
        t.column_id = column;
        t.claim_expires_at = None;
        t.updated_at = chrono::Utc::now();
        Ok(t.clone())
    }

    async fn backfill_pr_url(&self, tenant: TenantId, id: TaskId, url: &str) -> ApiResult<bool> {
        let mut st = self.inner.lock().unwrap();
        let Some(t) = st
            .tasks
            .iter()
            .find(|t| t.id == id && t.tenant_id == tenant && t.archived_at.is_none())
            .cloned()
        else {
            return Ok(false);
        };
        if t.pr_url.is_some() || !in_flight(&st, &t) {
            return Ok(false);
        }
        let t = st
            .tasks
            .iter_mut()
            .find(|t| t.id == id)
            .expect("just found");
        t.pr_url = Some(url.into());
        t.updated_at = chrono::Utc::now();
        Ok(true)
    }

    async fn complete_if_in_flight(
        &self,
        tenant: TenantId,
        id: TaskId,
        column: ColumnId,
    ) -> ApiResult<Option<TaskItem>> {
        let mut st = self.inner.lock().unwrap();
        let Some(t) = st
            .tasks
            .iter()
            .find(|t| t.id == id && t.tenant_id == tenant && t.archived_at.is_none())
            .cloned()
        else {
            return Ok(None);
        };
        if !in_flight(&st, &t) {
            return Ok(None);
        }
        let t = st
            .tasks
            .iter_mut()
            .find(|t| t.id == id)
            .expect("just found");
        t.column_id = column;
        t.claim_expires_at = None;
        t.updated_at = chrono::Utc::now();
        Ok(Some(t.clone()))
    }

    async fn clear_worktree(&self, id: TaskId) -> ApiResult<TaskItem> {
        let mut st = self.inner.lock().unwrap();
        let t = st
            .tasks
            .iter_mut()
            .find(|t| t.id == id)
            .expect("task exists");
        t.checkout_id = None;
        t.worktree_path = None;
        t.worktree_node_id = None;
        t.updated_at = chrono::Utc::now();
        Ok(t.clone())
    }

    async fn record_loop_worktree(
        &self,
        id: TaskId,
        node: NodeId,
        path: &str,
    ) -> ApiResult<TaskItem> {
        let mut st = self.inner.lock().unwrap();
        let t = st
            .tasks
            .iter_mut()
            .find(|t| t.id == id)
            .expect("task exists");
        t.worktree_path = Some(path.to_string());
        t.worktree_node_id = Some(node);
        t.updated_at = chrono::Utc::now();
        Ok(t.clone())
    }

    async fn worktree_paths_on_node(&self, node: NodeId) -> ApiResult<Vec<String>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .tasks
            .iter()
            .filter(|t| t.worktree_node_id == Some(node))
            .filter_map(|t| t.worktree_path.clone())
            .collect())
    }

    async fn active_worktree_paths_on_node(&self, node: NodeId) -> ApiResult<Vec<String>> {
        let st = self.inner.lock().unwrap();
        let terminal: Vec<ColumnId> = st
            .columns
            .iter()
            .filter(|c| matches!(c.r#type.as_str(), "completed" | "canceled"))
            .map(|c| c.id)
            .collect();
        Ok(st
            .tasks
            .iter()
            .filter(|t| t.worktree_node_id == Some(node))
            .filter(|t| t.archived_at.is_none() && !terminal.contains(&t.column_id))
            .filter_map(|t| t.worktree_path.clone())
            .collect())
    }

    async fn finished_worktrees_on_node(
        &self,
        node: NodeId,
    ) -> ApiResult<Vec<(TenantId, TaskId, String)>> {
        let st = self.inner.lock().unwrap();
        let terminal: Vec<ColumnId> = st
            .columns
            .iter()
            .filter(|c| matches!(c.r#type.as_str(), "completed" | "canceled"))
            .map(|c| c.id)
            .collect();
        Ok(st
            .tasks
            .iter()
            .filter(|t| t.worktree_node_id == Some(node))
            .filter(|t| terminal.contains(&t.column_id))
            .filter_map(|t| Some((t.tenant_id, t.id, t.worktree_path.clone()?)))
            .collect())
    }

    async fn set_column(&self, id: TaskId, column: ColumnId) -> ApiResult<TaskItem> {
        let mut st = self.inner.lock().unwrap();
        let started = st
            .columns
            .iter()
            .any(|c| c.id == column && c.r#type == "started");
        let t = st
            .tasks
            .iter_mut()
            .find(|t| t.id == id)
            .expect("task exists");
        t.column_id = column;
        if !started {
            t.claim_expires_at = None;
        }
        t.updated_at = chrono::Utc::now();
        Ok(t.clone())
    }

    async fn renew_claim(
        &self,
        id: TaskId,
        tenant: TenantId,
        ttl_secs: i64,
    ) -> ApiResult<Option<Option<TaskItem>>> {
        let mut st = self.inner.lock().unwrap();
        let Some(t) = st
            .tasks
            .iter_mut()
            .find(|t| t.id == id && t.tenant_id == tenant)
        else {
            return Ok(None);
        };
        if t.claim_expires_at.is_none() {
            return Ok(Some(None));
        }
        t.claim_expires_at = Some(chrono::Utc::now() + chrono::Duration::seconds(ttl_secs));
        t.updated_at = chrono::Utc::now();
        Ok(Some(Some(t.clone())))
    }

    // The fake has no sessions or nodes to judge liveness by, so the reaper's two
    // scans are only exercised against a real database (`tests/claim_lease.rs`).
    async fn reap_lapsed_claims(&self, _session_grace_secs: i64) -> ApiResult<Vec<LapsedClaim>> {
        Ok(Vec::new())
    }

    async fn capped_claims(&self) -> ApiResult<Vec<CappedClaim>> {
        Ok(Vec::new())
    }

    async fn attach_label_once(
        &self,
        _tenant: TenantId,
        task_id: TaskId,
        name: &str,
    ) -> ApiResult<bool> {
        let mut st = self.inner.lock().unwrap();
        if st
            .task_labels
            .iter()
            .any(|(t, n)| *t == task_id.0 && n == name)
        {
            return Ok(false);
        }
        st.task_labels.push((task_id.0, name.into()));
        Ok(true)
    }

    async fn insert_agent_comment(
        &self,
        tenant: TenantId,
        task_id: TaskId,
        author_id: Uuid,
        author_name: &str,
        body_md: &str,
    ) -> ApiResult<TaskComment> {
        let now = chrono::Utc::now();
        let c = TaskComment {
            id: Uuid::now_v7(),
            tenant_id: tenant,
            task_id,
            author_type: "agent".into(),
            author_id: Some(author_id),
            author_name: author_name.into(),
            body_md: body_md.into(),
            created_at: now,
            updated_at: now,
        };
        self.inner.lock().unwrap().comments.push(c.clone());
        Ok(c)
    }

    async fn attach_label(&self, _tenant: TenantId, task_id: TaskId, name: &str) -> ApiResult<()> {
        let mut st = self.inner.lock().unwrap();
        if !st
            .task_labels
            .iter()
            .any(|(t, n)| *t == task_id.0 && n == name)
        {
            st.task_labels.push((task_id.0, name.into()));
        }
        Ok(())
    }

    async fn detach_label(&self, _tenant: TenantId, task_id: TaskId, name: &str) -> ApiResult<()> {
        let mut st = self.inner.lock().unwrap();
        st.task_labels
            .retain(|(t, n)| !(*t == task_id.0 && n == name));
        Ok(())
    }

    async fn present_checkout_at(
        &self,
        node_id: NodeId,
        path: &str,
    ) -> ApiResult<Option<NodeWorkspaceId>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .checkouts
            .iter()
            .find(|c| c.node == node_id && c.path == path && c.present)
            .map(|c| c.id))
    }

    async fn checkout_location(
        &self,
        checkout: NodeWorkspaceId,
    ) -> ApiResult<Option<(String, NodeId)>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .checkouts
            .iter()
            .find(|c| c.id == checkout && c.present)
            .map(|c| (c.path.clone(), c.node)))
    }

    async fn clone_path_on_node(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
        node: NodeId,
    ) -> ApiResult<Option<String>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .checkouts
            .iter()
            .find(|c| {
                c.tenant == tenant
                    && c.workspace == workspace
                    && c.node == node
                    && c.kind == "clone"
                    && c.present
            })
            .map(|c| c.path.clone()))
    }

    async fn git_remote_for_workspace(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
    ) -> ApiResult<Option<String>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .checkouts
            .iter()
            .find(|c| c.tenant == tenant && c.workspace == workspace && c.remote.is_some())
            .and_then(|c| c.remote.clone()))
    }

    async fn pick_tasks(
        &self,
        tenant: TenantId,
        viewer: UserId,
        p: PickParams,
    ) -> ApiResult<Vec<TaskItem>> {
        let st = self.inner.lock().unwrap();
        let labels_of = |id: Uuid| -> Vec<String> {
            st.task_labels
                .iter()
                .filter(|(t, _)| *t == id)
                .map(|(_, n)| n.clone())
                .collect()
        };
        let col_type = |c: ColumnId| {
            st.columns
                .iter()
                .find(|x| x.id == c)
                .map(|x| x.r#type.clone())
                .unwrap_or_default()
        };
        let mut out: Vec<TaskItem> = st
            .tasks
            .iter()
            .filter(|t| t.tenant_id == tenant)
            .filter(|t| p.workspace.is_none() || t.workspace_id.map(|w| w.0) == p.workspace)
            // Node affinity, mirroring the SQL: asked by a node, a dispatched
            // card is that node's and an undispatched one is anybody's.
            .filter(|t| match p.node {
                None => true,
                Some(n) => {
                    t.assigned_node_id.is_none() || t.assigned_node_id.map(|x| x.0) == Some(n)
                }
            })
            .filter(|t| match &p.column_type {
                None => true,
                Some(ct) => col_type(t.column_id) == *ct,
            })
            .filter(|t| p.priority.is_none() || Some(t.priority) == p.priority)
            .filter(|t| !p.unassigned_only || t.assignee_user_id.is_none())
            .filter(|t| match p.assignee {
                None => true,
                Some(a) => t.assignee_user_id.map(|u| u.0) == Some(a),
            })
            .filter(|t| {
                let have = labels_of(t.id.0);
                p.labels.iter().all(|l| have.contains(l))
                    && !p.not_labels.iter().any(|l| have.contains(l))
            })
            .filter(|t| p.archived || t.archived_at.is_none())
            // The epic exclusion: with an explicit type filter the requested
            // types pass, with none everything except `epic` does.
            .filter(|t| {
                if p.types.is_empty() {
                    t.type_ != "epic"
                } else {
                    p.types.contains(&t.type_)
                }
            })
            // The viewer predicate, and the explicit filter which can only
            // narrow it — never widen.
            .filter(|t| {
                crate::services::tasks::visible_by_cols(
                    &t.visibility,
                    t.created_by,
                    t.assignee_user_id,
                    viewer,
                )
            })
            .filter(|t| p.visibility.is_empty() || p.visibility.contains(&t.visibility))
            .filter(|t| match p.parent {
                None => true,
                Some(par) => t.parent_task_id.map(|x| x.0) == Some(par),
            })
            // The backlog exclusion, lifted by a parent filter.
            .filter(|t| p.backlog || p.parent.is_some() || col_type(t.column_id) != "backlog")
            // The finished-work exclusion (MAIN-464), lifted by a parent filter
            // or by naming one of the two types outright.
            .filter(|t| {
                p.done
                    || p.parent.is_some()
                    || matches!(
                        p.column_type.as_deref(),
                        Some("completed") | Some("canceled")
                    )
                    || !matches!(col_type(t.column_id).as_str(), "completed" | "canceled")
            })
            .cloned()
            .collect();
        // Priority 0 means "unset", which sorts last rather than first.
        out.sort_by_key(|t| (if t.priority == 0 { 5 } else { t.priority }, t.created_at));
        out.truncate(p.limit.max(0) as usize);
        Ok(out)
    }

    async fn board_automation(
        &self,
        board: BoardId,
        tenant: TenantId,
    ) -> ApiResult<Option<serde_json::Value>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .boards
            .iter()
            .find(|b| b.id == board && b.tenant_id == tenant)
            .map(|b| b.automation.clone()))
    }

    async fn visibility_of(&self, task: TaskId) -> ApiResult<Option<String>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .tasks
            .iter()
            .find(|t| t.id == task)
            .map(|t| t.visibility.clone()))
    }

    async fn task_ref(&self, task: TaskId) -> ApiResult<Option<(Option<String>, i32, String)>> {
        let st = self.inner.lock().unwrap();
        let Some(t) = st.tasks.iter().find(|t| t.id == task) else {
            return Ok(None);
        };
        // The real query INNER JOINs boards, so an unknown board yields no row;
        // a board with a NULL key still does, with `None` for the key.
        if !st.boards.iter().any(|b| b.id == t.board_id) {
            return Ok(None);
        }
        let key = st
            .boards
            .iter()
            .find(|b| b.id == t.board_id)
            .and_then(|b| b.key.clone());
        Ok(t.number.map(|n| (key, n, t.title.clone())))
    }

    async fn column_type_of(&self, column: ColumnId) -> ApiResult<Option<String>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .columns
            .iter()
            .find(|c| c.id == column)
            .map(|c| c.r#type.clone()))
    }

    async fn column_type_in_tenant(
        &self,
        column: ColumnId,
        tenant: TenantId,
    ) -> ApiResult<Option<String>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .columns
            .iter()
            .find(|c| {
                c.id == column
                    && st
                        .boards
                        .iter()
                        .any(|b| b.id == c.board_id && b.tenant_id == tenant)
            })
            .map(|c| c.r#type.clone()))
    }

    async fn board_of_task(&self, id: TaskId) -> ApiResult<Option<BoardId>> {
        let st = self.inner.lock().unwrap();
        Ok(st.tasks.iter().find(|t| t.id == id).map(|t| t.board_id))
    }

    async fn claim_task(
        &self,
        id: TaskId,
        tenant: TenantId,
        assignee: UserId,
        column: Option<ColumnId>,
        lease_secs: Option<i64>,
    ) -> ApiResult<Option<TaskItem>> {
        let mut st = self.inner.lock().unwrap();
        let Some(t) = st
            .tasks
            .iter_mut()
            .find(|t| t.id == id && t.tenant_id == tenant)
        else {
            return Ok(None);
        };
        // The "still unassigned" test lives in the same critical section as the
        // write, exactly as the real statement's WHERE does — a fake that
        // checked separately would let both racers win.
        if t.assignee_user_id.is_some() {
            return Ok(None);
        }
        t.assignee_user_id = Some(assignee);
        if let Some(c) = column {
            t.column_id = c;
        }
        if let Some(secs) = lease_secs {
            t.claim_expires_at = Some(chrono::Utc::now() + chrono::Duration::seconds(secs));
        }
        t.updated_at = chrono::Utc::now();
        Ok(Some(t.clone()))
    }

    async fn assignee_of(&self, id: TaskId, tenant: TenantId) -> ApiResult<Option<Option<UserId>>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .tasks
            .iter()
            .find(|t| t.id == id && t.tenant_id == tenant)
            .map(|t| t.assignee_user_id))
    }

    async fn release_assignment(
        &self,
        id: TaskId,
        tenant: TenantId,
    ) -> ApiResult<Option<TaskItem>> {
        let mut st = self.inner.lock().unwrap();
        let Some(t) = st
            .tasks
            .iter_mut()
            .find(|t| t.id == id && t.tenant_id == tenant)
        else {
            return Ok(None);
        };
        t.assignee_user_id = None;
        t.claim_expires_at = None;
        t.updated_at = chrono::Utc::now();
        Ok(Some(t.clone()))
    }

    async fn release_claim_of(
        &self,
        id: TaskId,
        tenant: TenantId,
        holder: UserId,
    ) -> ApiResult<bool> {
        let mut st = self.inner.lock().unwrap();
        let Some(t) = st
            .tasks
            .iter_mut()
            .find(|t| t.id == id && t.tenant_id == tenant)
        else {
            return Ok(false);
        };
        if t.assignee_user_id != Some(holder) || t.claim_expires_at.is_none() {
            return Ok(false);
        }
        t.assignee_user_id = None;
        t.claim_expires_at = None;
        t.updated_at = chrono::Utc::now();
        Ok(true)
    }

    async fn build_failures(&self, id: TaskId, _tenant: TenantId) -> ApiResult<i32> {
        let st = self.inner.lock().unwrap();
        Ok(st.build_failures.get(&id.0).copied().unwrap_or(0))
    }

    async fn bump_build_failures(&self, id: TaskId, _tenant: TenantId) -> ApiResult<i32> {
        let mut st = self.inner.lock().unwrap();
        let n = st.build_failures.entry(id.0).or_insert(0);
        *n += 1;
        Ok(*n)
    }

    async fn clear_build_failures(
        &self,
        id: TaskId,
        _tenant: TenantId,
        at_least: i32,
    ) -> ApiResult<()> {
        let mut st = self.inner.lock().unwrap();
        if st.build_failures.get(&id.0).copied().unwrap_or(0) >= at_least {
            st.build_failures.remove(&id.0);
        }
        Ok(())
    }

    async fn set_agent_ready(&self, _tenant: TenantId, id: TaskId, on: bool) -> ApiResult<()> {
        let mut st = self.inner.lock().unwrap();
        let present = st
            .task_labels
            .iter()
            .any(|(t, n)| *t == id.0 && n == "agent-ready");
        match (on, present) {
            (true, false) => st.task_labels.push((id.0, "agent-ready".into())),
            (false, true) => st
                .task_labels
                .retain(|(t, n)| !(*t == id.0 && n == "agent-ready")),
            _ => {}
        }
        Ok(())
    }

    async fn list_labels(&self, tenant: TenantId) -> ApiResult<Vec<Label>> {
        let st = self.inner.lock().unwrap();
        let mut v: Vec<Label> = st
            .labels
            .iter()
            .filter(|l| l.tenant_id == tenant)
            .cloned()
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(v)
    }

    async fn upsert_label(&self, tenant: TenantId, name: &str, color: &str) -> ApiResult<Label> {
        let mut st = self.inner.lock().unwrap();
        if let Some(l) = st
            .labels
            .iter()
            .find(|l| l.tenant_id == tenant && l.name == name)
        {
            return Ok(l.clone());
        }
        let l = Label {
            id: Uuid::now_v7(),
            tenant_id: tenant,
            name: name.into(),
            color: color.into(),
            created_at: chrono::Utc::now(),
        };
        st.labels.push(l.clone());
        Ok(l)
    }

    async fn delete_label(&self, id: Uuid, tenant: TenantId) -> ApiResult<u64> {
        let mut st = self.inner.lock().unwrap();
        let before = st.labels.len();
        let names: Vec<String> = st
            .labels
            .iter()
            .filter(|l| l.id == id && l.tenant_id == tenant)
            .map(|l| l.name.clone())
            .collect();
        st.labels.retain(|l| !(l.id == id && l.tenant_id == tenant));
        // `task_labels` cascades, so the tasks themselves are untouched.
        st.task_labels.retain(|(_, n)| !names.contains(n));
        Ok((before - st.labels.len()) as u64)
    }

    async fn label_id_by_uuid(&self, id: Uuid, tenant: TenantId) -> ApiResult<Option<Uuid>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .labels
            .iter()
            .find(|l| l.id == id && l.tenant_id == tenant)
            .map(|l| l.id))
    }

    async fn label_id_by_name(&self, tenant: TenantId, name: &str) -> ApiResult<Option<Uuid>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .labels
            .iter()
            .find(|l| l.tenant_id == tenant && l.name == name)
            .map(|l| l.id))
    }

    async fn labels_of_task(&self, task: TaskId) -> ApiResult<Vec<Label>> {
        let st = self.inner.lock().unwrap();
        let mut v: Vec<Label> = st
            .task_labels
            .iter()
            .filter(|(t, _)| *t == task.0)
            .filter_map(|(_, n)| st.labels.iter().find(|l| l.name == *n).cloned())
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(v)
    }

    async fn attach_label_id(&self, task: TaskId, label: Uuid) -> ApiResult<u64> {
        let mut st = self.inner.lock().unwrap();
        let Some(name) = st
            .labels
            .iter()
            .find(|l| l.id == label)
            .map(|l| l.name.clone())
        else {
            return Ok(0);
        };
        if st
            .task_labels
            .iter()
            .any(|(t, n)| *t == task.0 && *n == name)
        {
            return Ok(0); // already attached — the caller records no event
        }
        st.task_labels.push((task.0, name));
        Ok(1)
    }

    async fn detach_label_id(&self, task: TaskId, label: Uuid) -> ApiResult<u64> {
        let mut st = self.inner.lock().unwrap();
        let Some(name) = st
            .labels
            .iter()
            .find(|l| l.id == label)
            .map(|l| l.name.clone())
        else {
            return Ok(0);
        };
        let before = st.task_labels.len();
        st.task_labels
            .retain(|(t, n)| !(*t == task.0 && *n == name));
        Ok((before - st.task_labels.len()) as u64)
    }

    async fn task_naming(
        &self,
        task: TaskId,
    ) -> ApiResult<Option<(String, Option<i32>, Option<String>)>> {
        let st = self.inner.lock().unwrap();
        Ok(st.tasks.iter().find(|t| t.id == task).map(|t| {
            (
                t.title.clone(),
                t.number,
                st.boards
                    .iter()
                    .find(|b| b.id == t.board_id)
                    .and_then(|b| b.key.clone()),
            )
        }))
    }

    async fn create_board(
        &self,
        tenant: TenantId,
        workspace: Option<Uuid>,
        name: &str,
        key: &str,
    ) -> ApiResult<Board> {
        let mut st = self.inner.lock().unwrap();
        let now = chrono::Utc::now();
        let b = Board {
            id: BoardId::new(),
            tenant_id: tenant,
            workspace_id: workspace.map(WorkspaceId),
            name: name.into(),
            key: Some(key.into()),
            provider: "local".into(),
            automation: serde_json::json!({}),
            created_at: now,
            updated_at: now,
        };
        st.next_number.insert(b.id.0, 1);
        st.boards.push(b.clone());
        Ok(b)
    }

    async fn create_column(
        &self,
        board: BoardId,
        name: &str,
        position: i32,
        type_: &str,
    ) -> ApiResult<BoardColumn> {
        let mut st = self.inner.lock().unwrap();
        let c = BoardColumn {
            id: ColumnId::new(),
            board_id: board,
            name: name.into(),
            position,
            r#type: type_.into(),
        };
        st.columns.push(c.clone());
        Ok(c)
    }

    async fn set_archived(
        &self,
        id: TaskId,
        tenant: TenantId,
        archived: bool,
    ) -> ApiResult<Option<TaskItem>> {
        let mut st = self.inner.lock().unwrap();
        let Some(t) = st
            .tasks
            .iter_mut()
            .find(|t| t.id == id && t.tenant_id == tenant)
        else {
            return Ok(None);
        };
        t.archived_at = archived.then(chrono::Utc::now);
        t.updated_at = chrono::Utc::now();
        Ok(Some(t.clone()))
    }

    async fn archive_all_in_column(
        &self,
        column: ColumnId,
        tenant: TenantId,
    ) -> ApiResult<Vec<TaskId>> {
        let mut st = self.inner.lock().unwrap();
        let now = chrono::Utc::now();
        let mut moved = vec![];
        for t in st.tasks.iter_mut() {
            if t.column_id == column && t.tenant_id == tenant && t.archived_at.is_none() {
                t.archived_at = Some(now);
                t.updated_at = now;
                moved.push(t.id);
            }
        }
        Ok(moved)
    }

    async fn delete_task(&self, id: TaskId, tenant: TenantId) -> ApiResult<u64> {
        let mut st = self.inner.lock().unwrap();
        let before = st.tasks.len();
        st.tasks.retain(|t| !(t.id == id && t.tenant_id == tenant));
        Ok((before - st.tasks.len()) as u64)
    }

    async fn update_board(
        &self,
        id: BoardId,
        tenant: TenantId,
        name: &str,
        key: Option<String>,
        automation: Option<serde_json::Value>,
    ) -> ApiResult<Option<Board>> {
        let mut st = self.inner.lock().unwrap();
        let Some(b) = st
            .boards
            .iter_mut()
            .find(|b| b.id == id && b.tenant_id == tenant)
        else {
            return Ok(None);
        };
        b.name = name.into();
        // COALESCE: absent leaves it alone.
        if key.is_some() {
            b.key = key;
        }
        if let Some(a) = automation {
            b.automation = a;
        }
        b.updated_at = chrono::Utc::now();
        Ok(Some(b.clone()))
    }

    async fn board_key_taken(&self, tenant: TenantId, key: &str) -> ApiResult<bool> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .boards
            .iter()
            .any(|b| b.tenant_id == tenant && b.key.as_deref() == Some(key)))
    }

    async fn comments_of(&self, task: TaskId) -> ApiResult<Vec<TaskComment>> {
        let st = self.inner.lock().unwrap();
        let mut v: Vec<TaskComment> = st
            .comments
            .iter()
            .filter(|c| c.task_id == task)
            .cloned()
            .collect();
        v.sort_by_key(|c| c.created_at);
        Ok(v)
    }

    async fn create_comment(&self, new: NewComment) -> ApiResult<TaskComment> {
        let now = chrono::Utc::now();
        let c = TaskComment {
            id: Uuid::now_v7(),
            tenant_id: new.tenant,
            task_id: new.task,
            author_type: new.author_type,
            author_id: new.author_id,
            author_name: new.author_name,
            body_md: new.body_md,
            created_at: now,
            updated_at: now,
        };
        self.inner.lock().unwrap().comments.push(c.clone());
        Ok(c)
    }

    async fn add_description_revision(
        &self,
        tenant: TenantId,
        task: TaskId,
        body: &str,
        author: Option<Uuid>,
    ) -> ApiResult<()> {
        self.inner
            .lock()
            .unwrap()
            .desc_revisions
            .push(TaskDescriptionRevision {
                id: Uuid::now_v7(),
                tenant_id: tenant,
                task_id: task,
                body: body.to_string(),
                author_id: author,
                created_at: chrono::Utc::now(),
            });
        Ok(())
    }

    async fn description_revisions_of(
        &self,
        tenant: TenantId,
        task: TaskId,
    ) -> ApiResult<Vec<TaskDescriptionRevision>> {
        let mut rows: Vec<_> = self
            .inner
            .lock()
            .unwrap()
            .desc_revisions
            .iter()
            .filter(|r| r.tenant_id == tenant && r.task_id == task)
            .cloned()
            .collect();
        // Newest first, id as the tiebreak — the real query's ordering.
        rows.sort_by_key(|r| std::cmp::Reverse((r.created_at, r.id)));
        Ok(rows)
    }

    async fn task_visibility_naming(
        &self,
        task: TaskId,
        tenant: TenantId,
    ) -> ApiResult<Option<(String, Option<i32>, Option<String>)>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .tasks
            .iter()
            .find(|t| t.id == task && t.tenant_id == tenant)
            .map(|t| {
                (
                    t.visibility.clone(),
                    t.number,
                    st.boards
                        .iter()
                        .find(|b| b.id == t.board_id)
                        .and_then(|b| b.key.clone()),
                )
            }))
    }

    async fn update_comment(
        &self,
        id: Uuid,
        tenant: TenantId,
        body_md: &str,
    ) -> ApiResult<TaskComment> {
        let mut st = self.inner.lock().unwrap();
        let c = st
            .comments
            .iter_mut()
            .find(|c| c.id == id && c.tenant_id == tenant)
            .expect("comment exists");
        c.body_md = body_md.into();
        c.updated_at = chrono::Utc::now();
        Ok(c.clone())
    }

    async fn delete_comment(&self, id: Uuid, tenant: TenantId) -> ApiResult<()> {
        let mut st = self.inner.lock().unwrap();
        st.comments
            .retain(|c| !(c.id == id && c.tenant_id == tenant));
        Ok(())
    }

    async fn comment_author(
        &self,
        id: Uuid,
        tenant: TenantId,
    ) -> ApiResult<Option<(Option<Uuid>, TaskId)>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .comments
            .iter()
            .find(|c| c.id == id && c.tenant_id == tenant)
            .map(|c| (c.author_id, c.task_id)))
    }

    async fn upsert_relation(
        &self,
        tenant: TenantId,
        from: TaskId,
        to: TaskId,
        kind: &str,
    ) -> ApiResult<TaskRelation> {
        let mut st = self.inner.lock().unwrap();
        if let Some(r) = st
            .relations
            .iter_mut()
            .find(|r| r.from_task == from && r.to_task == to && r.kind == kind)
        {
            return Ok(r.clone());
        }
        let r = TaskRelation {
            id: Uuid::now_v7(),
            tenant_id: tenant,
            from_task: from,
            to_task: to,
            kind: kind.into(),
            created_at: chrono::Utc::now(),
        };
        st.relations.push(r.clone());
        Ok(r)
    }

    async fn blocks_reaches(&self, start: TaskId, target: TaskId) -> ApiResult<bool> {
        let st = self.inner.lock().unwrap();
        // The recursive CTE, iteratively. `seen` is what stops a pre-existing
        // cycle from making the guard itself run forever.
        let mut seen = std::collections::HashSet::new();
        let mut frontier = vec![start];
        while let Some(cur) = frontier.pop() {
            for r in st
                .relations
                .iter()
                .filter(|r| r.kind == "blocks" && r.from_task == cur)
            {
                if r.to_task == target {
                    return Ok(true);
                }
                if seen.insert(r.to_task) {
                    frontier.push(r.to_task);
                }
            }
        }
        Ok(false)
    }

    async fn delete_relation(&self, id: Uuid, tenant: TenantId) -> ApiResult<u64> {
        let mut st = self.inner.lock().unwrap();
        let before = st.relations.len();
        st.relations
            .retain(|r| !(r.id == id && r.tenant_id == tenant));
        Ok((before - st.relations.len()) as u64)
    }

    async fn epic_children(&self, parent: TaskId, viewer: UserId) -> ApiResult<Vec<EpicChild>> {
        let st = self.inner.lock().unwrap();
        let mut kids: Vec<&TaskItem> = st
            .tasks
            .iter()
            .filter(|t| t.parent_task_id == Some(parent))
            .filter(|t| {
                crate::services::tasks::visible_by_cols(
                    &t.visibility,
                    t.created_by,
                    t.assignee_user_id,
                    viewer,
                )
            })
            .collect();
        kids.sort_by_key(|t| (if t.priority == 0 { 5 } else { t.priority }, t.created_at));
        Ok(kids
            .into_iter()
            .map(|t| EpicChild {
                id: t.id,
                key: st
                    .boards
                    .iter()
                    .find(|b| b.id == t.board_id)
                    .and_then(|b| b.key.clone())
                    .zip(t.number)
                    .map(|(k, n)| format!("{k}-{n}")),
                title: t.title.clone(),
                type_: t.type_.clone(),
                priority: t.priority,
                column_type: st
                    .columns
                    .iter()
                    .find(|c| c.id == t.column_id)
                    .map(|c| c.r#type.clone())
                    .unwrap_or_default(),
                archived_at: t.archived_at,
            })
            .collect())
    }

    async fn key_of(&self, tenant: TenantId, id: TaskId) -> ApiResult<Option<String>> {
        let st = self.inner.lock().unwrap();
        let Some(t) = st
            .tasks
            .iter()
            .find(|t| t.id == id && t.tenant_id == tenant)
        else {
            return Ok(None);
        };
        let key = st
            .boards
            .iter()
            .find(|b| b.id == t.board_id)
            .and_then(|b| b.key.clone());
        // The SQL concatenates; a NULL on either side yields NULL.
        Ok(match (key, t.number) {
            (Some(k), Some(n)) => Some(format!("{k}-{n}")),
            _ => None,
        })
    }

    async fn task_keys_at_worktree(&self, node: NodeId, path: &str) -> ApiResult<Vec<String>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .tasks
            .iter()
            .filter(|t| {
                t.worktree_node_id == Some(node) && t.worktree_path.as_deref() == Some(path)
            })
            .map(|t| {
                let key = st
                    .boards
                    .iter()
                    .find(|b| b.id == t.board_id)
                    .and_then(|b| b.key.clone())
                    .unwrap_or_default();
                format!("{}-{}", key, t.number.unwrap_or(0))
            })
            .collect())
    }

    async fn related_tasks(&self, task: TaskId, viewer: UserId) -> ApiResult<Vec<RelatedTask>> {
        let st = self.inner.lock().unwrap();
        let mut out: Vec<RelatedTask> = st
            .relations
            .iter()
            .filter(|r| r.from_task == task || r.to_task == task)
            .filter_map(|r| {
                let other = if r.from_task == task {
                    r.to_task
                } else {
                    r.from_task
                };
                let t = st.tasks.iter().find(|t| t.id == other)?;
                if !crate::services::tasks::visible_by_cols(
                    &t.visibility,
                    t.created_by,
                    t.assignee_user_id,
                    viewer,
                ) {
                    return None;
                }
                Some(RelatedTask {
                    relation_id: r.id,
                    id: t.id,
                    title: t.title.clone(),
                    key: st
                        .boards
                        .iter()
                        .find(|b| b.id == t.board_id)
                        .and_then(|b| b.key.clone())
                        .zip(t.number)
                        .map(|(k, n)| format!("{k}-{n}")),
                    kind: match (r.kind.as_str(), r.to_task == task) {
                        ("blocks", true) => "blocked_by".into(),
                        ("blocks", false) => "blocking".into(),
                        (k, _) => k.into(),
                    },
                    column_type: st
                        .columns
                        .iter()
                        .find(|c| c.id == t.column_id)
                        .map(|c| c.r#type.clone())
                        .unwrap_or_default(),
                })
            })
            .collect();
        out.sort_by_key(|r| r.key.clone());
        Ok(out)
    }

    async fn delete_board(&self, id: BoardId, tenant: TenantId) -> ApiResult<u64> {
        let mut st = self.inner.lock().unwrap();
        let before = st.boards.len();
        st.boards.retain(|b| !(b.id == id && b.tenant_id == tenant));
        Ok((before - st.boards.len()) as u64)
    }

    async fn board_in_tenant(&self, id: BoardId, tenant: TenantId) -> ApiResult<bool> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .boards
            .iter()
            .any(|b| b.id == id && b.tenant_id == tenant))
    }

    async fn max_column_position(&self, board: BoardId) -> ApiResult<Option<i32>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .columns
            .iter()
            .filter(|c| c.board_id == board)
            .map(|c| c.position)
            .max())
    }

    async fn append_column(
        &self,
        board: BoardId,
        name: &str,
        position: i32,
    ) -> ApiResult<BoardColumn> {
        let mut st = self.inner.lock().unwrap();
        let c = BoardColumn {
            id: ColumnId::new(),
            board_id: board,
            name: name.into(),
            position,
            // The column DEFAULT the INSERT relies on by omitting the field.
            r#type: "unstarted".into(),
        };
        st.columns.push(c.clone());
        Ok(c)
    }

    async fn update_column(
        &self,
        id: ColumnId,
        tenant: TenantId,
        name: Option<String>,
        position: Option<i32>,
    ) -> ApiResult<Option<BoardColumn>> {
        let mut st = self.inner.lock().unwrap();
        let owned: Vec<BoardId> = st
            .boards
            .iter()
            .filter(|b| b.tenant_id == tenant)
            .map(|b| b.id)
            .collect();
        let Some(c) = st
            .columns
            .iter_mut()
            .find(|c| c.id == id && owned.contains(&c.board_id))
        else {
            return Ok(None);
        };
        // COALESCE: absent leaves the field alone.
        if let Some(n) = name {
            c.name = n;
        }
        if let Some(p) = position {
            c.position = p;
        }
        Ok(Some(c.clone()))
    }

    async fn delete_column(&self, id: ColumnId, tenant: TenantId) -> ApiResult<u64> {
        let mut st = self.inner.lock().unwrap();
        let owned: Vec<BoardId> = st
            .boards
            .iter()
            .filter(|b| b.tenant_id == tenant)
            .map(|b| b.id)
            .collect();
        let doomed: Vec<ColumnId> = st
            .columns
            .iter()
            .filter(|c| c.id == id && owned.contains(&c.board_id))
            .map(|c| c.id)
            .collect();
        st.columns.retain(|c| !doomed.contains(&c.id));
        // Deleting a column cascades its tasks (schema ON DELETE CASCADE).
        st.tasks.retain(|t| !doomed.contains(&t.column_id));
        Ok(doomed.len() as u64)
    }

    async fn board_provider(&self, id: BoardId, tenant: TenantId) -> ApiResult<Option<String>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .boards
            .iter()
            .find(|b| b.id == id && b.tenant_id == tenant)
            .map(|b| b.provider.clone()))
    }
}
