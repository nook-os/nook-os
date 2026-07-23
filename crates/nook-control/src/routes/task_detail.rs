//! One whole issue: comments, relations, and the detail read the loop depends on.
//!
//! ## On agent identity
//!
//! The spec asked for `author_type = 'agent'` for MCP callers. NookOS has no
//! agent principal — `Principal` is `User | Node`, and MCP authenticates with a
//! person's token — so "this was an agent" is not something the server can
//! know. Inventing a bot identity to make the field look right would be
//! recording a fact nobody established.
//!
//! What it does instead: the author is the real caller, and a client may pass
//! `author_name` to say which tool it was ("loop-build on azul"). The
//! attribution is then honest at both levels — *this person's credential*, used
//! by *this tool* — and no permission hangs on the string, so a client lying
//! about it gains nothing.

use axum::extract::{Path, State};
use axum::Json;
use nook_types::*;

use crate::auth::{AuthCtx, Principal};
use crate::error::{ApiError, ApiResult};
use crate::services::tasks;
use crate::state::AppState;

// ── comments ────────────────────────────────────────────────────────────────

#[utoipa::path(get, path = "/api/v1/tasks/{id}/comments",
    operation_id = "list_comments", params(("id" = String, Path,)),
    responses((status = 200, body = [TaskComment]), (status = 404)))]
pub async fn list_comments(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(ident): Path<String>,
) -> ApiResult<Json<Vec<TaskComment>>> {
    let task = tasks::resolve_id(&state.db, auth.tenant_id, &ident).await?;
    Ok(Json(comments_of(&state, task).await?))
}

pub async fn comments_of(state: &AppState, task: TaskId) -> ApiResult<Vec<TaskComment>> {
    // Oldest first: a comment thread is read as a narrative, and the loop
    // parses the latest verdict by taking the last one.
    let rows: Vec<TaskComment> = sqlx::query_as(
        "SELECT id, tenant_id, task_id, author_type, author_id, author_name,
                body_md, created_at, updated_at
         FROM task_comments WHERE task_id = $1 ORDER BY created_at",
    )
    .bind(task)
    .fetch_all(&state.db)
    .await?;
    Ok(rows)
}

#[utoipa::path(post, path = "/api/v1/tasks/{id}/comments",
    operation_id = "create_comment", params(("id" = String, Path,)),
    request_body = CreateCommentRequest,
    responses((status = 200, body = TaskComment), (status = 404)))]
pub async fn create_comment(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(ident): Path<String>,
    Json(req): Json<CreateCommentRequest>,
) -> ApiResult<Json<TaskComment>> {
    if req.body_md.trim().is_empty() {
        return Err(ApiError::BadRequest("a comment needs a body".into()));
    }
    let task = tasks::resolve_id(&state.db, auth.tenant_id, &ident).await?;

    // See the module note. A node is `system` because a machine reporting on
    // its own work is not a person; a user token is `user` even when a tool is
    // driving it, because that is who authorised it.
    let (author_type, author_id) = match auth.principal {
        Principal::Node(_) => ("system", None),
        Principal::User => ("user", Some(auth.user_id.0)),
    };
    let name = match req.author_name.as_deref().map(str::trim) {
        Some(n) if !n.is_empty() => n.chars().take(80).collect::<String>(),
        _ => display_name(&state, auth.user_id).await,
    };

    let row: TaskComment = sqlx::query_as(
        "INSERT INTO task_comments (id, tenant_id, task_id, author_type, author_id, author_name, body_md)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         RETURNING id, tenant_id, task_id, author_type, author_id, author_name,
                   body_md, created_at, updated_at",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(auth.tenant_id)
    .bind(task)
    .bind(author_type)
    .bind(author_id)
    .bind(&name)
    .bind(&req.body_md)
    .fetch_one(&state.db)
    .await?;

    // Comments are the content; events remain the timeline. Both, so the
    // activity feed still shows that something was said without becoming the
    // place the saying is stored.
    crate::events::record(
        &state,
        auth.tenant_id,
        crate::events::EventDraft::new("task.comment.created")
            .actor("user", auth.user_id.0)
            .payload(serde_json::json!({ "task_id": task, "author": name })),
    )
    .await;

    state.registry.publish(
        auth.tenant_id,
        nook_proto::UiEvent::TaskChanged { task_id: task },
    );
    Ok(Json(row))
}

#[utoipa::path(patch, path = "/api/v1/comments/{id}",
    operation_id = "update_comment", params(("id" = String, Path,)),
    request_body = UpdateCommentRequest,
    responses((status = 200, body = TaskComment), (status = 403), (status = 404)))]
pub async fn update_comment(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<uuid::Uuid>,
    Json(req): Json<UpdateCommentRequest>,
) -> ApiResult<Json<TaskComment>> {
    owned_comment(&state, &auth, id).await?;
    let row: TaskComment = sqlx::query_as(
        "UPDATE task_comments SET body_md = $1, updated_at = now()
         WHERE id = $2 AND tenant_id = $3
         RETURNING id, tenant_id, task_id, author_type, author_id, author_name,
                   body_md, created_at, updated_at",
    )
    .bind(&req.body_md)
    .bind(id)
    .bind(auth.tenant_id)
    .fetch_one(&state.db)
    .await?;
    state.registry.publish(
        auth.tenant_id,
        nook_proto::UiEvent::TaskChanged {
            task_id: row.task_id,
        },
    );
    Ok(Json(row))
}

#[utoipa::path(delete, path = "/api/v1/comments/{id}",
    operation_id = "delete_comment", params(("id" = String, Path,)),
    responses((status = 204), (status = 403), (status = 404)))]
pub async fn delete_comment(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<uuid::Uuid>,
) -> ApiResult<axum::http::StatusCode> {
    let task = owned_comment(&state, &auth, id).await?;
    sqlx::query("DELETE FROM task_comments WHERE id = $1 AND tenant_id = $2")
        .bind(id)
        .bind(auth.tenant_id)
        .execute(&state.db)
        .await?;
    state.registry.publish(
        auth.tenant_id,
        nook_proto::UiEvent::TaskChanged { task_id: task },
    );
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// Editing is the author's own right, and nobody else's.
///
/// Returns the comment's task so callers can publish a change for it.
async fn owned_comment(state: &AppState, auth: &AuthCtx, id: uuid::Uuid) -> ApiResult<TaskId> {
    let row: Option<(Option<uuid::Uuid>, TaskId)> = sqlx::query_as(
        "SELECT author_id, task_id FROM task_comments WHERE id = $1 AND tenant_id = $2",
    )
    .bind(id)
    .bind(auth.tenant_id)
    .fetch_optional(&state.db)
    .await?;
    let (author_id, task) = row.ok_or(ApiError::NotFound)?;
    if author_id != Some(auth.user_id.0) {
        return Err(ApiError::ForbiddenMsg(
            "only the author can edit or delete a comment".into(),
        ));
    }
    Ok(task)
}

async fn display_name(state: &AppState, user: UserId) -> String {
    sqlx::query_as::<_, (String,)>("SELECT display_name FROM users WHERE id = $1")
        .bind(user)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .map(|r| r.0)
        .unwrap_or_else(|| "unknown".into())
}

// ── relations ───────────────────────────────────────────────────────────────

#[utoipa::path(post, path = "/api/v1/tasks/{id}/relations",
    operation_id = "create_relation", params(("id" = String, Path,)),
    request_body = CreateRelationRequest,
    responses((status = 200, body = TaskRelation), (status = 409)))]
pub async fn create_relation(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(ident): Path<String>,
    Json(req): Json<CreateRelationRequest>,
) -> ApiResult<Json<TaskRelation>> {
    let from = tasks::resolve_id(&state.db, auth.tenant_id, &ident).await?;
    let to = tasks::resolve_id(&state.db, auth.tenant_id, &req.to_task.to_string()).await?;
    Ok(Json(
        link(&state, auth.tenant_id, from, to, &req.kind).await?,
    ))
}

/// Create a relation. Shared with MCP so both doors enforce the same rules —
/// notably the cycle check, which is not a nicety: a ring of `blocks` edges is
/// a set of tasks none of which can ever be picked up.
pub async fn link(
    state: &AppState,
    tenant: TenantId,
    from: TaskId,
    to: TaskId,
    kind: &str,
) -> ApiResult<TaskRelation> {
    const KINDS: [&str; 3] = ["blocks", "relates", "duplicates"];
    if !KINDS.contains(&kind) {
        return Err(ApiError::BadRequest(format!(
            "{kind:?} is not a relation kind — expected one of {}",
            KINDS.join(", ")
        )));
    }
    if from == to {
        return Err(ApiError::BadRequest(
            "a task cannot relate to itself".into(),
        ));
    }

    // A blocks-cycle is a deadlock nothing can ever pick up: every task in the
    // ring waits on another member forever. Cheaper to refuse than to explain
    // later why the queue is permanently empty.
    if kind == "blocks" && reaches(state, to, from).await? {
        return Err(ApiError::Conflict(
            "that would create a cycle: the blocked task already blocks this one, \
             directly or through a chain, and neither could ever start"
                .into(),
        ));
    }

    let row: TaskRelation = sqlx::query_as(
        "INSERT INTO task_relations (id, tenant_id, from_task, to_task, kind)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (from_task, to_task, kind) DO UPDATE SET kind = EXCLUDED.kind
         RETURNING id, tenant_id, from_task, to_task, kind, created_at",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(tenant)
    .bind(from)
    .bind(to)
    .bind(kind)
    .fetch_one(&state.db)
    .await?;

    state
        .registry
        .publish(tenant, nook_proto::UiEvent::TaskChanged { task_id: from });
    Ok(row)
}

/// Can `start` reach `target` by following `blocks` edges?
///
/// Recursive in SQL rather than in a loop of round trips: the depth is unknown
/// and each hop would otherwise be a query. `UNION` (not `UNION ALL`) is what
/// terminates it — an existing cycle in the data would otherwise make the walk
/// itself run forever.
async fn reaches(state: &AppState, start: TaskId, target: TaskId) -> ApiResult<bool> {
    let hit: Option<(bool,)> = sqlx::query_as(
        "WITH RECURSIVE reachable(id) AS (
             SELECT to_task FROM task_relations WHERE from_task = $1 AND kind = 'blocks'
             UNION
             SELECT r.to_task FROM task_relations r
             JOIN reachable p ON r.from_task = p.id
             WHERE r.kind = 'blocks'
         )
         SELECT true FROM reachable WHERE id = $2 LIMIT 1",
    )
    .bind(start)
    .bind(target)
    .fetch_optional(&state.db)
    .await?;
    Ok(hit.is_some())
}

#[utoipa::path(delete, path = "/api/v1/relations/{id}",
    operation_id = "delete_relation", params(("id" = String, Path,)),
    responses((status = 204), (status = 404)))]
pub async fn delete_relation(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<uuid::Uuid>,
) -> ApiResult<axum::http::StatusCode> {
    let res = sqlx::query("DELETE FROM task_relations WHERE id = $1 AND tenant_id = $2")
        .bind(id)
        .bind(auth.tenant_id)
        .execute(&state.db)
        .await?;
    if res.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(axum::http::StatusCode::NO_CONTENT)
}

// ── the whole issue ─────────────────────────────────────────────────────────

#[utoipa::path(get, path = "/api/v1/tasks/{id}",
    operation_id = "get_task", params(("id" = String, Path,)),
    responses((status = 200, body = TaskDetail), (status = 404)))]
pub async fn get_task(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(ident): Path<String>,
) -> ApiResult<Json<TaskDetail>> {
    let id = tasks::resolve_id(&state.db, auth.tenant_id, &ident).await?;
    Ok(Json(detail(&state, auth.tenant_id, id).await?))
}

pub async fn detail(state: &AppState, tenant: TenantId, id: TaskId) -> ApiResult<TaskDetail> {
    let task: TaskItem = sqlx::query_as("SELECT * FROM tasks WHERE id = $1 AND tenant_id = $2")
        .bind(id)
        .bind(tenant)
        .fetch_optional(&state.db)
        .await?
        .ok_or(ApiError::NotFound)?;
    let task = tasks::enrich_one(&state.db, &state.cfg.public_base_url, task).await?;

    let related = related_tasks(state, id).await?;
    let blocked_by: Vec<RelatedTask> = related
        .iter()
        .filter(|r| r.kind == "blocked_by")
        .cloned()
        .collect();
    let blocking: Vec<RelatedTask> = related
        .iter()
        .filter(|r| r.kind == "blocking")
        .cloned()
        .collect();
    let other: Vec<RelatedTask> = related
        .iter()
        .filter(|r| r.kind != "blocked_by" && r.kind != "blocking")
        .cloned()
        .collect();

    // Derived, never stored. A blocker is resolved when its column type says
    // the work is finished or abandoned — so moving a blocker to Done unblocks
    // this task with no write here at all, and there is no flag to drift.
    let is_blocked = blocked_by
        .iter()
        .any(|r| r.column_type != "completed" && r.column_type != "canceled");

    Ok(TaskDetail {
        task,
        comments: comments_of(state, id).await?,
        blocked_by,
        blocking,
        related: other,
        is_blocked,
    })
}

/// Both directions in one query, with `kind` rewritten to the reader's point of
/// view: an edge "A blocks B" is `blocking` when you asked about A and
/// `blocked_by` when you asked about B. The raw direction is a fact about the
/// row; what a person needs is which side they are on.
async fn related_tasks(state: &AppState, id: TaskId) -> ApiResult<Vec<RelatedTask>> {
    let rows: Vec<RelatedTask> = sqlx::query_as(
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
         WHERE r.from_task = $1 OR r.to_task = $1
         ORDER BY t.number",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    Ok(rows)
}
