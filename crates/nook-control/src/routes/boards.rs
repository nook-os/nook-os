use axum::extract::{Path, State};
use axum::Json;
use nook_types::*;

use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::events::{self, EventDraft};
use crate::services::kanban::provider_err;
use crate::state::AppState;

#[utoipa::path(get, path = "/api/v1/boards",
    operation_id = "list_boards", responses((status = 200, body = [Board])))]
pub async fn list(State(state): State<AppState>, auth: AuthCtx) -> ApiResult<Json<Vec<Board>>> {
    Ok(Json(state.kanban.all_boards(auth.tenant_id).await?))
}

#[utoipa::path(post, path = "/api/v1/boards",
    operation_id = "create_board",
    request_body = CreateBoardRequest,
    responses((status = 200, body = BoardDetail)))]
pub async fn create(
    State(state): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<CreateBoardRequest>,
) -> ApiResult<Json<BoardDetail>> {
    let board: Board = sqlx::query_as(
        "INSERT INTO boards (id, tenant_id, workspace_id, name, provider)
         VALUES ($1, $2, $3, $4, 'local') RETURNING *",
    )
    .bind(BoardId::new())
    .bind(auth.tenant_id)
    .bind(req.workspace_id)
    .bind(&req.name)
    .fetch_one(&state.db)
    .await?;

    let mut columns = Vec::new();
    for (i, name) in ["Triage", "Todo", "In Progress", "Done"].iter().enumerate() {
        let col: BoardColumn = sqlx::query_as(
            "INSERT INTO board_columns (id, board_id, name, position)
             VALUES ($1, $2, $3, $4) RETURNING *",
        )
        .bind(ColumnId::new())
        .bind(board.id)
        .bind(name)
        .bind(i as i32)
        .fetch_one(&state.db)
        .await?;
        columns.push(col);
    }

    Ok(Json(BoardDetail {
        board,
        columns,
        tasks: vec![],
    }))
}

#[utoipa::path(get, path = "/api/v1/boards/{id}",
    operation_id = "get_board",
    params(("id" = String, Path,)),
    responses((status = 200, body = BoardDetail), (status = 404)))]
pub async fn get_one(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<BoardId>,
) -> ApiResult<Json<BoardDetail>> {
    let provider = provider_for_board(&state, auth.tenant_id, id).await?;
    let detail = state
        .kanban
        .get(&provider)
        .ok_or(ApiError::NotFound)?
        .board_detail(auth.tenant_id, id)
        .await
        .map_err(provider_err)?;
    Ok(Json(detail))
}

#[utoipa::path(post, path = "/api/v1/boards/{id}/tasks",
    operation_id = "create_task",
    params(("id" = String, Path,)),
    request_body = CreateTaskRequest,
    responses((status = 200, body = TaskItem)))]
pub async fn create_task(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<BoardId>,
    Json(req): Json<CreateTaskRequest>,
) -> ApiResult<Json<TaskItem>> {
    let provider = provider_for_board(&state, auth.tenant_id, id).await?;
    let workspace_id = req.workspace_id;
    let task = state
        .kanban
        .get(&provider)
        .ok_or(ApiError::NotFound)?
        .create_task(auth.tenant_id, id, req)
        .await
        .map_err(provider_err)?;

    events::record(
        &state,
        auth.tenant_id,
        EventDraft::new("task.created")
            .actor("user", auth.user_id.0)
            .payload(serde_json::json!({ "task_id": task.id, "title": task.title })),
    )
    .await;
    let _ = workspace_id;
    Ok(Json(task))
}

#[utoipa::path(patch, path = "/api/v1/tasks/{id}",
    operation_id = "update_task",
    params(("id" = String, Path,)),
    request_body = UpdateTaskRequest,
    responses((status = 200, body = TaskItem), (status = 404)))]
pub async fn update_task(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<TaskId>,
    Json(req): Json<UpdateTaskRequest>,
) -> ApiResult<Json<TaskItem>> {
    let (board_id,): (BoardId,) =
        sqlx::query_as("SELECT board_id FROM tasks WHERE id = $1 AND tenant_id = $2")
            .bind(id)
            .bind(auth.tenant_id)
            .fetch_optional(&state.db)
            .await?
            .ok_or(ApiError::NotFound)?;
    let provider = provider_for_board(&state, auth.tenant_id, board_id).await?;
    let moved_column = req.column_id.is_some();
    let task = state
        .kanban
        .get(&provider)
        .ok_or(ApiError::NotFound)?
        .update_task(auth.tenant_id, id, req)
        .await
        .map_err(provider_err)?;

    if moved_column {
        events::record(
            &state,
            auth.tenant_id,
            EventDraft::new("task.moved")
                .actor("user", auth.user_id.0)
                .payload(serde_json::json!({ "task_id": task.id, "title": task.title })),
        )
        .await;
    }
    Ok(Json(task))
}

#[utoipa::path(delete, path = "/api/v1/tasks/{id}",
    operation_id = "delete_task",
    params(("id" = String, Path,)),
    responses((status = 204), (status = 404)))]
pub async fn delete_task(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<TaskId>,
) -> ApiResult<axum::http::StatusCode> {
    let res = sqlx::query("DELETE FROM tasks WHERE id = $1 AND tenant_id = $2")
        .bind(id)
        .bind(auth.tenant_id)
        .execute(&state.db)
        .await?;
    if res.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[utoipa::path(patch, path = "/api/v1/boards/{id}",
    operation_id = "update_board",
    params(("id" = String, Path,)),
    request_body = UpdateBoardRequest,
    responses((status = 200, body = Board), (status = 404)))]
pub async fn update_board(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<BoardId>,
    Json(req): Json<UpdateBoardRequest>,
) -> ApiResult<Json<Board>> {
    let board: Option<Board> = sqlx::query_as(
        "UPDATE boards SET name = $3, updated_at = now()
         WHERE id = $1 AND tenant_id = $2 RETURNING *",
    )
    .bind(id)
    .bind(auth.tenant_id)
    .bind(&req.name)
    .fetch_optional(&state.db)
    .await?;
    board.map(Json).ok_or(ApiError::NotFound)
}

#[utoipa::path(delete, path = "/api/v1/boards/{id}",
    operation_id = "delete_board",
    params(("id" = String, Path,)),
    responses((status = 204), (status = 404)))]
pub async fn delete_board(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<BoardId>,
) -> ApiResult<axum::http::StatusCode> {
    let res = sqlx::query("DELETE FROM boards WHERE id = $1 AND tenant_id = $2")
        .bind(id)
        .bind(auth.tenant_id)
        .execute(&state.db)
        .await?;
    if res.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[utoipa::path(post, path = "/api/v1/boards/{id}/columns",
    operation_id = "add_column",
    params(("id" = String, Path,)),
    request_body = CreateColumnRequest,
    responses((status = 200, body = BoardColumn), (status = 404)))]
pub async fn add_column(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(board_id): Path<BoardId>,
    Json(req): Json<CreateColumnRequest>,
) -> ApiResult<Json<BoardColumn>> {
    // Tenant must own the board.
    let owned: Option<(BoardId,)> =
        sqlx::query_as("SELECT id FROM boards WHERE id = $1 AND tenant_id = $2")
            .bind(board_id)
            .bind(auth.tenant_id)
            .fetch_optional(&state.db)
            .await?;
    if owned.is_none() {
        return Err(ApiError::NotFound);
    }
    let (max_pos,): (Option<i32>,) =
        sqlx::query_as("SELECT max(position) FROM board_columns WHERE board_id = $1")
            .bind(board_id)
            .fetch_one(&state.db)
            .await?;
    let col: BoardColumn = sqlx::query_as(
        "INSERT INTO board_columns (id, board_id, name, position)
         VALUES ($1, $2, $3, $4) RETURNING *",
    )
    .bind(ColumnId::new())
    .bind(board_id)
    .bind(&req.name)
    .bind(max_pos.unwrap_or(-1) + 1)
    .fetch_one(&state.db)
    .await?;
    Ok(Json(col))
}

#[utoipa::path(patch, path = "/api/v1/columns/{id}",
    operation_id = "update_column",
    params(("id" = String, Path,)),
    request_body = UpdateColumnRequest,
    responses((status = 200, body = BoardColumn), (status = 404)))]
pub async fn update_column(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<ColumnId>,
    Json(req): Json<UpdateColumnRequest>,
) -> ApiResult<Json<BoardColumn>> {
    // Column must belong to a board the tenant owns.
    let col: Option<BoardColumn> = sqlx::query_as(
        "UPDATE board_columns SET
            name = COALESCE($2, name),
            position = COALESCE($3, position)
         WHERE id = $1 AND board_id IN (SELECT id FROM boards WHERE tenant_id = $4)
         RETURNING *",
    )
    .bind(id)
    .bind(&req.name)
    .bind(req.position)
    .bind(auth.tenant_id)
    .fetch_optional(&state.db)
    .await?;
    col.map(Json).ok_or(ApiError::NotFound)
}

#[utoipa::path(delete, path = "/api/v1/columns/{id}",
    operation_id = "delete_column",
    params(("id" = String, Path,)),
    responses((status = 204), (status = 404)))]
pub async fn delete_column(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<ColumnId>,
) -> ApiResult<axum::http::StatusCode> {
    // Deleting a column cascades its tasks (schema ON DELETE CASCADE).
    let res = sqlx::query(
        "DELETE FROM board_columns
         WHERE id = $1 AND board_id IN (SELECT id FROM boards WHERE tenant_id = $2)",
    )
    .bind(id)
    .bind(auth.tenant_id)
    .execute(&state.db)
    .await?;
    if res.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(axum::http::StatusCode::NO_CONTENT)
}

async fn provider_for_board(
    state: &AppState,
    tenant: TenantId,
    board: BoardId,
) -> ApiResult<String> {
    let (provider,): (String,) =
        sqlx::query_as("SELECT provider FROM boards WHERE id = $1 AND tenant_id = $2")
            .bind(board)
            .bind(tenant)
            .fetch_optional(&state.db)
            .await?
            .ok_or(ApiError::NotFound)?;
    Ok(provider)
}
