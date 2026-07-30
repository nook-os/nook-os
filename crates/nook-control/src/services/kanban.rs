//! Kanban federation. External boards remain authoritative; NookOS presents
//! one unified experience. `local` is a full provider backed by Postgres;
//! external providers are registered but unconfigured in milestone 1.

use async_trait::async_trait;
use nook_types::{
    Board, BoardDetail, BoardId, ColumnId, CreateTaskRequest, TaskId, TaskItem, TenantId,
    UpdateTaskRequest, UserId,
};

use crate::error::{ApiError, ApiResult};

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("provider '{0}' is not configured")]
    NotConfigured(&'static str),
    #[error(transparent)]
    Api(#[from] ApiError),
}

pub type ProviderResult<T> = Result<T, ProviderError>;

/// The allowed issue types (MAIN-59). The DB CHECK is the backstop; validating
/// here turns an invalid value into a clean 400 rather than a database error.
pub const TASK_TYPES: &[&str] = &["task", "bug", "epic", "story", "chore"];

/// Reject an out-of-set `type` with a 400 (AC-2) — never silently coerce it.
fn validate_task_type(t: Option<&str>) -> ApiResult<()> {
    if let Some(v) = t {
        if !TASK_TYPES.contains(&v) {
            return Err(ApiError::BadRequest(format!(
                "invalid task type {v:?} — one of {}",
                TASK_TYPES.join(", ")
            )));
        }
    }
    Ok(())
}

/// The allowed task visibilities (MAIN-76). `team` is the default; the DB CHECK
/// is the backstop.
pub const TASK_VISIBILITIES: &[&str] = &["private", "team", "org"];

/// Reject an out-of-set `visibility` with a 400 — never silently coerce it.
fn validate_visibility(v: Option<&str>) -> ApiResult<()> {
    if let Some(val) = v {
        if !TASK_VISIBILITIES.contains(&val) {
            return Err(ApiError::BadRequest(format!(
                "invalid visibility {val:?} — one of {}",
                TASK_VISIBILITIES.join(", ")
            )));
        }
    }
    Ok(())
}

/// Resolve and validate a `parent` reference (MAIN-81 AC-2). The parent must
/// resolve (uuid or key, tenant-scoped) to a `type='epic'` task on the SAME
/// board that the caller can SEE (MAIN-76: a private epic the caller neither
/// created nor is assigned is refused as unfound, so parenting cannot confirm
/// its existence), and the task being parented must not itself be an epic (no
/// nesting, which also makes cycles impossible). Every failure is a 400 naming
/// the rule.
async fn validate_parent(
    repo: &dyn crate::repo::tasks::TaskRepository,
    tenant: TenantId,
    viewer: UserId,
    board: BoardId,
    parent_ref: &str,
    self_is_epic: bool,
) -> ApiResult<TaskId> {
    if self_is_epic {
        return Err(ApiError::BadRequest(
            "an epic cannot have a parent — epics do not nest".into(),
        ));
    }
    let parent_id = crate::services::tasks::resolve_id(repo, tenant, parent_ref)
        .await
        .map_err(|_| {
            ApiError::BadRequest(format!(
                "parent {parent_ref:?} is not a task in this tenant"
            ))
        })?;
    let parent = repo.get_row(tenant, parent_id).await?;
    let parent = parent.ok_or_else(|| {
        ApiError::BadRequest(format!(
            "parent {parent_ref:?} is not a task in this tenant"
        ))
    })?;
    // A private epic the caller cannot see is treated as not a task in this
    // tenant — parenting must not leak or confirm it (MAIN-76).
    if !crate::services::tasks::visible_to(&parent, viewer) {
        return Err(ApiError::BadRequest(format!(
            "parent {parent_ref:?} is not a task in this tenant"
        )));
    }
    if parent.type_ != "epic" {
        return Err(ApiError::BadRequest(format!(
            "parent {parent_ref:?} is not an epic — a task can only hang off a type='epic' task"
        )));
    }
    if parent.board_id != board {
        return Err(ApiError::BadRequest(
            "parent epic is on a different board — a task and its epic must share a board".into(),
        ));
    }
    Ok(parent_id)
}

#[async_trait]
pub trait KanbanProvider: Send + Sync {
    fn id(&self) -> &'static str;
    async fn list_boards(&self, tenant: TenantId) -> ProviderResult<Vec<Board>>;
    async fn board_detail(&self, tenant: TenantId, board: BoardId) -> ProviderResult<BoardDetail>;
    // `created_by`: the per-tenant `users.id` of the creator, stamped so a
    // `private` card is owned. `None` only for non-user callers.
    async fn create_task(
        &self,
        tenant: TenantId,
        board: BoardId,
        created_by: Option<UserId>,
        req: CreateTaskRequest,
    ) -> ProviderResult<TaskItem>;
    // `viewer` (MAIN-76/81): the caller, for the parent-epic visibility check
    // when `req.parent` sets a new parent. `None` for a non-user caller.
    async fn update_task(
        &self,
        tenant: TenantId,
        viewer: Option<UserId>,
        task: TaskId,
        req: UpdateTaskRequest,
    ) -> ProviderResult<TaskItem>;
}

// ── Local boards (Postgres) ─────────────────────────────────────────────────

pub struct LocalBoardProvider {
    pub repo: std::sync::Arc<dyn crate::repo::tasks::TaskRepository>,
}

#[async_trait]
impl KanbanProvider for LocalBoardProvider {
    fn id(&self) -> &'static str {
        "local"
    }

    async fn list_boards(&self, tenant: TenantId) -> ProviderResult<Vec<Board>> {
        Ok(self.repo.list_local_boards(tenant).await?)
    }

    async fn board_detail(&self, tenant: TenantId, board: BoardId) -> ProviderResult<BoardDetail> {
        let b = self
            .repo
            .get_board(tenant, board)
            .await?
            .ok_or(ApiError::NotFound)?;
        let columns = self.repo.board_columns(board).await?;
        let tasks = self.repo.board_tasks(board).await?;
        Ok(BoardDetail {
            board: b,
            columns,
            tasks,
        })
    }

    async fn create_task(
        &self,
        tenant: TenantId,
        board: BoardId,
        created_by: Option<UserId>,
        req: CreateTaskRequest,
    ) -> ProviderResult<TaskItem> {
        validate_task_type(req.type_.as_deref())?;
        validate_visibility(req.visibility.as_deref())?;
        // Resolve + validate the epic parent, if any (MAIN-81). Done before the
        // insert transaction so a bad parent is a clean 400, not a rolled-back
        // half-write. The creator is the viewer for the visibility check; a
        // non-user caller (created_by None) sees only non-private epics.
        let viewer = created_by.unwrap_or(UserId(uuid::Uuid::nil()));
        let parent_task_id = match req.parent.as_deref() {
            Some(p) => Some(
                validate_parent(
                    self.repo.as_ref(),
                    tenant,
                    viewer,
                    board,
                    p,
                    req.type_.as_deref() == Some("epic"),
                )
                .await?,
            ),
            None => None,
        };
        // Explicit id wins; then a semantic type, which is what automation
        // knows; then the board's first column.
        let column_id: ColumnId = match (req.column_id, req.column_type.as_deref()) {
            (Some(c), _) => c,
            (None, Some(ct)) => {
                crate::services::tasks::column_of_type(self.repo.as_ref(), board, ct).await?
            }
            (None, None) => self
                .repo
                .first_column(board)
                .await?
                .ok_or_else(|| ApiError::BadRequest("board has no columns".into()))?,
        };
        let max_pos = self.repo.max_position_in_column(column_id).await?;

        // Number allocation and the insert share one transaction, and the
        // board row is locked while it happens. Without the lock two concurrent
        // creates read the same `next_number` and one of them then violates the
        // unique index — which is a 500 for something the caller did nothing
        // wrong to cause. `FOR UPDATE` makes the second create wait rather than
        // fail, so `NOOK-7` is allocated exactly once.
        // Number allocation, the insert, and the labels are ONE repository call
        // because they are one transaction: the board row is locked while the
        // number is taken (so `NOOK-7` is allocated exactly once), and the
        // labels land inside it (so the pick query never sees the task for a
        // moment without the labels it was filed with).
        let task = self
            .repo
            .create_task(crate::repo::tasks::NewTask {
                tenant,
                board,
                column_id,
                title: req.title.clone(),
                description: req.description.clone(),
                position: max_pos.unwrap_or(-1) + 1,
                workspace_id: req.workspace_id.map(|w| w.0),
                priority: req.priority.unwrap_or(0).clamp(0, 4),
                // Omitted → the column DEFAULT ('task'); validated above (AC-2).
                type_: req.type_.as_deref().unwrap_or("task").to_string(),
                // Omitted → the column DEFAULT ('team'), reproducing today's behaviour.
                visibility: req.visibility.as_deref().unwrap_or("team").to_string(),
                created_by: created_by.map(|u| u.0),
                parent_task_id: parent_task_id.map(|t| t.0),
                // Labels by NAME, created if new: a filer knows `agent-ready`,
                // not its uuid. Empty names dropped here, as before.
                labels: req
                    .labels
                    .iter()
                    .map(|l| l.trim().to_lowercase())
                    .filter(|l| !l.is_empty())
                    .collect(),
            })
            .await?;
        Ok(task)
    }

    async fn update_task(
        &self,
        tenant: TenantId,
        viewer: Option<UserId>,
        task: TaskId,
        req: UpdateTaskRequest,
    ) -> ProviderResult<TaskItem> {
        validate_task_type(req.type_.as_deref())?;
        validate_visibility(req.visibility.as_deref())?;

        // Load the task's current type/board/parent once — the column-type
        // resolution, the epic-retype guard, and the parent validation all need
        // it, and one read keeps them consistent.
        let (cur_type, cur_board, cur_parent) = self
            .repo
            .task_shape(tenant, task)
            .await?
            .ok_or(ApiError::NotFound)?;

        // AC-3: changing an epic's type away from `epic` while it still has
        // children is refused (naming the count) — the children would be
        // orphaned onto a non-epic parent.
        if cur_type == "epic" && req.type_.as_deref().is_some_and(|t| t != "epic") {
            let children = self.repo.count_children(task).await?;
            if children > 0 {
                return Err(ApiError::BadRequest(format!(
                    "cannot change this epic's type: it still has {children} child ticket(s) — \
                     detach them first"
                ))
                .into());
            }
        }

        // Parent tri-state (AC-2/AC-3): absent = leave, null = detach, a ref =
        // validate + set. `self_is_epic` is the EFFECTIVE type after this patch.
        let effective_is_epic = req.type_.as_deref().unwrap_or(&cur_type) == "epic";
        let viewer = viewer.unwrap_or(UserId(uuid::Uuid::nil()));
        let (set_parent, parent_val): (bool, Option<TaskId>) = match &req.parent {
            None => (false, None),
            Some(None) => (true, None),
            Some(Some(p)) => (
                true,
                Some(
                    validate_parent(
                        self.repo.as_ref(),
                        tenant,
                        viewer,
                        cur_board,
                        p,
                        effective_is_epic,
                    )
                    .await?,
                ),
            ),
        };
        // An epic may not KEEP a parent: retyping to epic while it has one (and
        // not detaching it in the same patch) is refused (AC-2).
        if effective_is_epic && cur_parent.is_some() && !(set_parent && parent_val.is_none()) {
            return Err(ApiError::BadRequest(
                "an epic cannot have a parent — detach it before making it an epic".into(),
            )
            .into());
        }

        // A type given instead of an id is resolved against the task's OWN board.
        let column_id = match (req.column_id, req.column_type.as_deref()) {
            (Some(c), _) => Some(c),
            (None, Some(ct)) => Some(
                crate::services::tasks::column_of_type(self.repo.as_ref(), cur_board, ct).await?,
            ),
            (None, None) => None,
        };

        let updated = self
            .repo
            .update_fields(
                tenant,
                task,
                crate::repo::tasks::TaskEdit {
                    title: req.title.clone(),
                    description: req.description.clone(),
                    column_id: column_id.map(|c| c.0),
                    position: req.position,
                    assignee_user_id: req.assignee_user_id.map(|u| u.0),
                    priority: req.priority.map(|p| p.clamp(0, 4)),
                    // The flag, not the value: COALESCE reads a NULL as "leave
                    // it", which is exactly the instruction to clear it.
                    set_workspace: req.workspace_id.is_some(),
                    workspace_id: req.workspace_id.flatten().map(|w| w.0),
                    expected_updated_at: req.expected_updated_at,
                    type_: req.type_.clone(),
                    visibility: req.visibility.clone(),
                    set_parent,
                    parent_task_id: parent_val.map(|t| t.0),
                },
            )
            .await?;

        if let Some(t) = updated {
            return Ok(t);
        }

        // Zero rows. Without a precondition that can only mean the task is gone
        // (404). With one, tell a lost race (409, carrying the CURRENT task so
        // the caller can reconcile without a second round-trip) apart from a
        // task that truly no longer exists (404).
        let current = self.repo.get_row(tenant, task).await?;
        match current {
            Some(cur) if req.expected_updated_at.is_some() => {
                let body = serde_json::to_string(&cur)
                    .unwrap_or_else(|_| "the task changed under this edit".into());
                Err(ProviderError::Api(ApiError::Conflict(body)))
            }
            _ => Err(ProviderError::Api(ApiError::NotFound)),
        }
    }
}

// ── External providers (registered, unconfigured in M1) ─────────────────────

macro_rules! stub_provider {
    ($name:ident, $id:literal) => {
        pub struct $name;

        #[async_trait]
        impl KanbanProvider for $name {
            fn id(&self) -> &'static str {
                $id
            }
            async fn list_boards(&self, _: TenantId) -> ProviderResult<Vec<Board>> {
                Err(ProviderError::NotConfigured($id))
            }
            async fn board_detail(&self, _: TenantId, _: BoardId) -> ProviderResult<BoardDetail> {
                Err(ProviderError::NotConfigured($id))
            }
            async fn create_task(
                &self,
                _: TenantId,
                _: BoardId,
                _: Option<UserId>,
                _: CreateTaskRequest,
            ) -> ProviderResult<TaskItem> {
                Err(ProviderError::NotConfigured($id))
            }
            async fn update_task(
                &self,
                _: TenantId,
                _: Option<UserId>,
                _: TaskId,
                _: UpdateTaskRequest,
            ) -> ProviderResult<TaskItem> {
                Err(ProviderError::NotConfigured($id))
            }
        }
    };
}

stub_provider!(JiraProvider, "jira");
stub_provider!(GithubProjectsProvider, "github");
stub_provider!(LinearProvider, "linear");
stub_provider!(TrelloProvider, "trello");

/// All registered providers. Boards carry their provider id; operations are
/// routed to the matching provider.
pub struct KanbanRegistry {
    providers: Vec<std::sync::Arc<dyn KanbanProvider>>,
}

impl KanbanRegistry {
    pub fn new(repo: std::sync::Arc<dyn crate::repo::tasks::TaskRepository>) -> Self {
        Self {
            providers: vec![
                std::sync::Arc::new(LocalBoardProvider { repo }),
                std::sync::Arc::new(JiraProvider),
                std::sync::Arc::new(GithubProjectsProvider),
                std::sync::Arc::new(LinearProvider),
                std::sync::Arc::new(TrelloProvider),
            ],
        }
    }

    pub fn get(&self, id: &str) -> Option<std::sync::Arc<dyn KanbanProvider>> {
        self.providers.iter().find(|p| p.id() == id).cloned()
    }

    /// Federated board list: every configured provider contributes; the
    /// unconfigured ones are skipped silently.
    pub async fn all_boards(&self, tenant: TenantId) -> ApiResult<Vec<Board>> {
        let mut out = Vec::new();
        for p in &self.providers {
            match p.list_boards(tenant).await {
                Ok(mut boards) => out.append(&mut boards),
                Err(ProviderError::NotConfigured(_)) => {}
                Err(ProviderError::Api(e)) => return Err(e),
            }
        }
        Ok(out)
    }
}

pub fn provider_err(e: ProviderError) -> ApiError {
    match e {
        ProviderError::NotConfigured(id) => {
            ApiError::BadRequest(format!("provider '{id}' is not configured"))
        }
        ProviderError::Api(e) => e,
    }
}
