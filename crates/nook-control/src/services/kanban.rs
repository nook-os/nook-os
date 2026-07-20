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
        // Default to the board's first column.
        let column_id: ColumnId = match req.column_id {
            Some(c) => c,
            None => {
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

        let task = sqlx::query_as(
            "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, description, position, workspace_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING *",
        )
        .bind(TaskId::new())
        .bind(tenant)
        .bind(board)
        .bind(column_id)
        .bind(&req.title)
        .bind(&req.description)
        .bind(max_pos.unwrap_or(-1) + 1)
        .bind(req.workspace_id)
        .fetch_one(&self.db)
        .await
        .map_err(ApiError::from)?;
        Ok(task)
    }

    async fn update_task(
        &self,
        tenant: TenantId,
        task: TaskId,
        req: UpdateTaskRequest,
    ) -> ProviderResult<TaskItem> {
        let updated = sqlx::query_as(
            "UPDATE tasks SET
                title = COALESCE($3, title),
                description = COALESCE($4, description),
                column_id = COALESCE($5, column_id),
                position = COALESCE($6, position),
                assignee_user_id = COALESCE($7, assignee_user_id),
                updated_at = now()
             WHERE id = $1 AND tenant_id = $2
             RETURNING *",
        )
        .bind(task)
        .bind(tenant)
        .bind(&req.title)
        .bind(&req.description)
        .bind(req.column_id)
        .bind(req.position)
        .bind(req.assignee_user_id)
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
