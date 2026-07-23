//! Kanban federation. External boards remain authoritative; NookOS presents
//! one unified experience. `local` is a full provider backed by Postgres;
//! external providers are registered but unconfigured in milestone 1.

use async_trait::async_trait;
use nook_types::{
    Board, BoardDetail, BoardId, ColumnId, CreateTaskRequest, TaskId, TaskItem, TenantId,
    UpdateTaskRequest,
};
use sqlx::PgPool;

use crate::error::{ApiError, ApiResult};

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("provider '{0}' is not configured")]
    NotConfigured(&'static str),
    #[error(transparent)]
    Api(#[from] ApiError),
}

pub type ProviderResult<T> = Result<T, ProviderError>;

#[async_trait]
pub trait KanbanProvider: Send + Sync {
    fn id(&self) -> &'static str;
    async fn list_boards(&self, tenant: TenantId) -> ProviderResult<Vec<Board>>;
    async fn board_detail(&self, tenant: TenantId, board: BoardId) -> ProviderResult<BoardDetail>;
    async fn create_task(
        &self,
        tenant: TenantId,
        board: BoardId,
        req: CreateTaskRequest,
    ) -> ProviderResult<TaskItem>;
    async fn update_task(
        &self,
        tenant: TenantId,
        task: TaskId,
        req: UpdateTaskRequest,
    ) -> ProviderResult<TaskItem>;
}

// ── Local boards (Postgres) ─────────────────────────────────────────────────

pub struct LocalBoardProvider {
    pub db: PgPool,
}

#[async_trait]
impl KanbanProvider for LocalBoardProvider {
    fn id(&self) -> &'static str {
        "local"
    }

    async fn list_boards(&self, tenant: TenantId) -> ProviderResult<Vec<Board>> {
        let boards = sqlx::query_as(
            "SELECT * FROM boards WHERE tenant_id = $1 AND provider = 'local' ORDER BY created_at",
        )
        .bind(tenant)
        .fetch_all(&self.db)
        .await
        .map_err(ApiError::from)?;
        Ok(boards)
    }

    async fn board_detail(&self, tenant: TenantId, board: BoardId) -> ProviderResult<BoardDetail> {
        let b: Board = sqlx::query_as("SELECT * FROM boards WHERE id = $1 AND tenant_id = $2")
            .bind(board)
            .bind(tenant)
            .fetch_optional(&self.db)
            .await
            .map_err(ApiError::from)?
            .ok_or(ApiError::NotFound)?;
        let columns = sqlx::query_as(
            "SELECT * FROM board_columns WHERE board_id = $1 ORDER BY position, name",
        )
        .bind(board)
        .fetch_all(&self.db)
        .await
        .map_err(ApiError::from)?;
        let tasks =
            sqlx::query_as("SELECT * FROM tasks WHERE board_id = $1 ORDER BY position, created_at")
                .bind(board)
                .fetch_all(&self.db)
                .await
                .map_err(ApiError::from)?;
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
        req: CreateTaskRequest,
    ) -> ProviderResult<TaskItem> {
        // Explicit id wins; then a semantic type, which is what automation
        // knows; then the board's first column.
        let column_id: ColumnId = match (req.column_id, req.column_type.as_deref()) {
            (Some(c), _) => c,
            (None, Some(ct)) => crate::services::tasks::column_of_type(&self.db, board, ct).await?,
            (None, None) => {
                let (id,): (ColumnId,) = sqlx::query_as(
                    "SELECT id FROM board_columns WHERE board_id = $1 ORDER BY position LIMIT 1",
                )
                .bind(board)
                .fetch_optional(&self.db)
                .await
                .map_err(ApiError::from)?
                .ok_or_else(|| ApiError::BadRequest("board has no columns".into()))?;
                id
            }
        };
        let (max_pos,): (Option<i32>,) =
            sqlx::query_as("SELECT max(position) FROM tasks WHERE column_id = $1")
                .bind(column_id)
                .fetch_one(&self.db)
                .await
                .map_err(ApiError::from)?;

        // Number allocation and the insert share one transaction, and the
        // board row is locked while it happens. Without the lock two concurrent
        // creates read the same `next_number` and one of them then violates the
        // unique index — which is a 500 for something the caller did nothing
        // wrong to cause. `FOR UPDATE` makes the second create wait rather than
        // fail, so `NOOK-7` is allocated exactly once.
        let mut tx = self.db.begin().await.map_err(ApiError::from)?;
        let (number,): (i32,) = sqlx::query_as(
            "UPDATE boards SET next_number = next_number + 1
             WHERE id = (SELECT id FROM boards WHERE id = $1 FOR UPDATE)
             RETURNING next_number - 1",
        )
        .bind(board)
        .fetch_one(&mut *tx)
        .await
        .map_err(ApiError::from)?;

        let task: TaskItem = sqlx::query_as(
            "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, description,
                                position, workspace_id, priority, number)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING *",
        )
        .bind(TaskId::new())
        .bind(tenant)
        .bind(board)
        .bind(column_id)
        .bind(&req.title)
        .bind(&req.description)
        .bind(max_pos.unwrap_or(-1) + 1)
        .bind(req.workspace_id)
        .bind(req.priority.unwrap_or(0).clamp(0, 4))
        .bind(number)
        .fetch_one(&mut *tx)
        .await
        .map_err(ApiError::from)?;

        // Labels by NAME, created if new: a filer knows `agent-ready`, not its
        // uuid. Inside the transaction so a task never exists momentarily
        // without the labels it was filed with — the pick query would otherwise
        // have a window in which it sees unlabelled work.
        for name in req.labels.iter().map(|l| l.trim().to_lowercase()) {
            if name.is_empty() {
                continue;
            }
            let (label_id,): (uuid::Uuid,) = sqlx::query_as(
                "INSERT INTO labels (id, tenant_id, name) VALUES ($1, $2, $3)
                 ON CONFLICT (tenant_id, name) DO UPDATE SET name = EXCLUDED.name
                 RETURNING id",
            )
            .bind(uuid::Uuid::now_v7())
            .bind(tenant)
            .bind(&name)
            .fetch_one(&mut *tx)
            .await
            .map_err(ApiError::from)?;
            sqlx::query(
                "INSERT INTO task_labels (task_id, label_id) VALUES ($1, $2)
                 ON CONFLICT DO NOTHING",
            )
            .bind(task.id)
            .bind(label_id)
            .execute(&mut *tx)
            .await
            .map_err(ApiError::from)?;
        }

        tx.commit().await.map_err(ApiError::from)?;
        Ok(task)
    }

    async fn update_task(
        &self,
        tenant: TenantId,
        task: TaskId,
        req: UpdateTaskRequest,
    ) -> ProviderResult<TaskItem> {
        // A type given instead of an id is resolved against the task's OWN
        // board — the caller knows "move it to started", not which board this
        // task happens to live on.
        let column_id = match (req.column_id, req.column_type.as_deref()) {
            (Some(c), _) => Some(c),
            (None, Some(ct)) => {
                let (board,): (BoardId,) =
                    sqlx::query_as("SELECT board_id FROM tasks WHERE id = $1 AND tenant_id = $2")
                        .bind(task)
                        .bind(tenant)
                        .fetch_optional(&self.db)
                        .await
                        .map_err(ApiError::from)?
                        .ok_or(ApiError::NotFound)?;
                Some(crate::services::tasks::column_of_type(&self.db, board, ct).await?)
            }
            (None, None) => None,
        };

        let updated = sqlx::query_as(
            "UPDATE tasks SET
                title = COALESCE($3, title),
                description = COALESCE($4, description),
                column_id = COALESCE($5, column_id),
                position = COALESCE($6, position),
                assignee_user_id = COALESCE($7, assignee_user_id),
                priority = COALESCE($8, priority),
                updated_at = now()
             WHERE id = $1 AND tenant_id = $2
             RETURNING *",
        )
        .bind(task)
        .bind(tenant)
        .bind(&req.title)
        .bind(&req.description)
        .bind(column_id)
        .bind(req.position)
        .bind(req.assignee_user_id)
        .bind(req.priority.map(|p| p.clamp(0, 4)))
        .fetch_optional(&self.db)
        .await
        .map_err(ApiError::from)?
        .ok_or(ApiError::NotFound)?;
        Ok(updated)
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
                _: CreateTaskRequest,
            ) -> ProviderResult<TaskItem> {
                Err(ProviderError::NotConfigured($id))
            }
            async fn update_task(
                &self,
                _: TenantId,
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
    pub fn new(db: PgPool) -> Self {
        Self {
            providers: vec![
                std::sync::Arc::new(LocalBoardProvider { db }),
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
