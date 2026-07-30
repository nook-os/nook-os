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
//! escape hatch (NG-3). The `Postgres.now()` / `.cast()` calls came in with the
//! moved SQL unchanged; replacing them is the dialect sweep's job.

use async_trait::async_trait;
use nook_db::{params, Db, DbPool, Postgres, TypeMapping};
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
}

#[async_trait]
pub trait TaskRepository: Send + Sync {
    // ---- enrichment reads (batched; an N+1 here is a board render) ---------

    /// Board keys for a set of boards, one row per board rather than per task.
    async fn board_keys(&self, board_ids: &[Uuid]) -> ApiResult<HashMap<Uuid, Option<String>>>;

    /// Every label attached to any of these tasks, ordered by label name.
    async fn labels_for_tasks(&self, task_ids: &[Uuid]) -> ApiResult<Vec<TaskLabelRow>>;

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
    async fn assign_node_and_column(
        &self,
        id: TaskId,
        node: NodeId,
        column: ColumnId,
    ) -> ApiResult<TaskItem>;

    /// Stamp the worktree, branch, session and column a started task now has.
    async fn record_started_work(&self, id: TaskId, work: StartedWork) -> ApiResult<TaskItem>;

    async fn set_pr_url(&self, id: TaskId, url: &str, column: ColumnId) -> ApiResult<TaskItem>;

    /// Forget the worktree — both the checkout id and the legacy string pair.
    async fn clear_worktree(&self, id: TaskId) -> ApiResult<TaskItem>;

    async fn set_column(&self, id: TaskId, column: ColumnId) -> ApiResult<TaskItem>;

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

    async fn create_task(&self, new: NewTask) -> ApiResult<TaskItem> {
        // One transaction, and the board row locked while the number is taken.
        // Without the lock two concurrent creates read the same `next_number`
        // and one then violates the unique index — a 500 for something the
        // caller did nothing wrong to cause. `FOR UPDATE` makes the second wait.
        let mut tx = self.db.begin().await?;
        let number: i32 = tx
            .query_scalar(
                "UPDATE boards SET next_number = next_number + 1
             WHERE id = (SELECT id FROM boards WHERE id = $1 FOR UPDATE)
             RETURNING next_number - 1",
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
                    now = Postgres.now(),
                    guard = Postgres.cast("$11", "timestamptz")
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
                    "UPDATE tasks SET assignee_user_id = NULL, updated_at = {now}
             WHERE id = $1 AND tenant_id = $2 RETURNING *",
                    now = Postgres.now()
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
                    now = Postgres.now()
                ),
                params![id, tenant, priority],
            )
            .await?)
    }

    async fn assign_node_and_column(
        &self,
        id: TaskId,
        node: NodeId,
        column: ColumnId,
    ) -> ApiResult<TaskItem> {
        Ok(self
            .db
            .query_one(
                &format!(
                    "UPDATE tasks SET assigned_node_id = $2, column_id = $3, updated_at = {}
         WHERE id = $1 RETURNING *",
                    Postgres.now()
                ),
                params![id, node, column],
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
                column_id = $7, checkout_id = $8, updated_at = {}
         WHERE id = $1 RETURNING *",
                    Postgres.now()
                ),
                params![
                    id,
                    work.workspace_id,
                    work.node_id,
                    &work.branch,
                    &work.worktree_path,
                    work.session_id,
                    work.column_id,
                    work.checkout_id
                ],
            )
            .await?)
    }

    async fn set_pr_url(&self, id: TaskId, url: &str, column: ColumnId) -> ApiResult<TaskItem> {
        Ok(self
            .db
            .query_one(
                &format!(
                    "UPDATE tasks SET pr_url = $2, column_id = $3, updated_at = {}
         WHERE id = $1 RETURNING *",
                    Postgres.now()
                ),
                params![id, url, column],
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
                    Postgres.now()
                ),
                params![id],
            )
            .await?)
    }

    async fn set_column(&self, id: TaskId, column: ColumnId) -> ApiResult<TaskItem> {
        Ok(self
            .db
            .query_one(
                &format!(
                    "UPDATE tasks SET column_id = $2, updated_at = {} WHERE id = $1 RETURNING *",
                    Postgres.now()
                ),
                params![id, column],
            )
            .await?)
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
        self.db
            .exec(
                "DELETE FROM task_labels tl USING labels l
             WHERE tl.label_id = l.id AND tl.task_id = $1
               AND l.tenant_id = $2 AND l.name = $3",
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
    comments: Vec<TaskComment>,
    /// `(checkout, tenant, workspace, node, path, kind, present, remote)`
    checkouts: Vec<Checkout>,
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
        column: ColumnId,
    ) -> ApiResult<TaskItem> {
        let mut st = self.inner.lock().unwrap();
        let t = st
            .tasks
            .iter_mut()
            .find(|t| t.id == id)
            .expect("task exists");
        t.assigned_node_id = Some(node);
        t.column_id = column;
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
        t.updated_at = chrono::Utc::now();
        Ok(t.clone())
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

    async fn set_column(&self, id: TaskId, column: ColumnId) -> ApiResult<TaskItem> {
        let mut st = self.inner.lock().unwrap();
        let t = st
            .tasks
            .iter_mut()
            .find(|t| t.id == id)
            .expect("task exists");
        t.column_id = column;
        t.updated_at = chrono::Utc::now();
        Ok(t.clone())
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
}
