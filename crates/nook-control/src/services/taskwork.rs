//! Kanban-driven work: triage dispatch, start-work (worktree + session),
//! submit-PR, prune. Shared by REST handlers and MCP tools so an AI can drive
//! the same lifecycle a human can.

use nook_proto::ControlToNode;
use nook_types::*;

use crate::error::{ApiError, ApiResult};
use crate::events::{self, EventDraft};
use crate::services::{core, identity::slugify};
use crate::state::AppState;

async fn load_task(state: &AppState, tenant: TenantId, id: TaskId) -> ApiResult<TaskItem> {
    sqlx::query_as("SELECT * FROM tasks WHERE id = $1 AND tenant_id = $2")
        .bind(id)
        .bind(tenant)
        .fetch_optional(&state.db)
        .await?
        .ok_or(ApiError::NotFound)
}

/// Resolve a column on the task's board by name (case-insensitive), falling
/// back to a positional index when the name isn't found.
async fn column_id(
    state: &AppState,
    board_id: BoardId,
    name: &str,
    fallback_pos: i32,
) -> ApiResult<ColumnId> {
    if let Some((id,)) = sqlx::query_as::<_, (ColumnId,)>(
        "SELECT id FROM board_columns WHERE board_id = $1 AND lower(name) = lower($2)",
    )
    .bind(board_id)
    .bind(name)
    .fetch_optional(&state.db)
    .await?
    {
        return Ok(id);
    }
    sqlx::query_as::<_, (ColumnId,)>(
        "SELECT id FROM board_columns WHERE board_id = $1 ORDER BY position OFFSET $2 LIMIT 1",
    )
    .bind(board_id)
    .bind(fallback_pos.max(0) as i64)
    .fetch_optional(&state.db)
    .await?
    .map(|(id,)| id)
    .ok_or_else(|| ApiError::BadRequest("board has no columns".into()))
}

/// Triage → Todo: the scheduler picks the best online node by resources
/// (preferring one that already hosts the task's workspace, if any).
pub async fn dispatch(state: &AppState, tenant: TenantId, task_id: TaskId) -> ApiResult<TaskItem> {
    let task = load_task(state, tenant, task_id).await?;
    let node = crate::services::schedule::pick(state, tenant, task.workspace_id).await?;
    let todo = column_id(state, task.board_id, "Todo", 1).await?;

    let updated: TaskItem = sqlx::query_as(
        "UPDATE tasks SET assigned_node_id = $2, column_id = $3, updated_at = now()
         WHERE id = $1 RETURNING *",
    )
    .bind(task_id)
    .bind(node)
    .bind(todo)
    .fetch_one(&state.db)
    .await?;

    events::record(
        state,
        tenant,
        EventDraft::new("task.dispatched")
            .payload(serde_json::json!({ "task_id": task_id, "node_id": node })),
    )
    .await;
    Ok(updated)
}

pub struct StartWork {
    pub node_id: Option<NodeId>,
    pub runtime: String,
    pub branch: Option<String>,
    pub workspace_id: Option<WorkspaceId>,
}

/// Todo → In Progress: create a worktree for the task's branch and start a
/// session in it. Returns the linked task and its session.
pub async fn start_work(
    state: &AppState,
    tenant: TenantId,
    user: Option<UserId>,
    task_id: TaskId,
    req: StartWork,
) -> ApiResult<(TaskItem, Session)> {
    let task = load_task(state, tenant, task_id).await?;
    if task.worktree_path.is_some() {
        return Err(ApiError::Conflict(
            "work already started for this task".into(),
        ));
    }
    let node_id = req
        .node_id
        .or(task.assigned_node_id)
        .ok_or_else(|| ApiError::BadRequest("no node — dispatch the task or pick one".into()))?;
    let workspace_id = req
        .workspace_id
        .or(task.workspace_id)
        .ok_or_else(|| ApiError::BadRequest("link a workspace first (clone or pick one)".into()))?;
    let branch = req
        .branch
        .filter(|b| !b.trim().is_empty())
        .unwrap_or_else(|| slugify(&task.title));

    // A checkout of this workspace must exist on the node to worktree from.
    let repo_path: Option<(String,)> = sqlx::query_as(
        "SELECT path FROM node_workspaces
         WHERE tenant_id = $1 AND workspace_id = $2 AND node_id = $3
         ORDER BY discovered_at LIMIT 1",
    )
    .bind(tenant)
    .bind(workspace_id)
    .bind(node_id)
    .fetch_optional(&state.db)
    .await?;
    let Some((repo_path,)) = repo_path else {
        return Err(ApiError::BadRequest(
            "that workspace has no checkout on that node — clone it there first".into(),
        ));
    };

    // Create the worktree for the task branch.
    let rx = state
        .registry
        .request_op(node_id, |request_id| ControlToNode::AddWorktree {
            request_id,
            repo_path,
            branch: branch.clone(),
        })
        .ok_or_else(|| ApiError::BadRequest("node is offline".into()))?;
    let op = tokio::time::timeout(std::time::Duration::from_secs(30), rx)
        .await
        .map_err(|_| ApiError::BadRequest("node did not answer in time".into()))?
        .map_err(|_| ApiError::BadRequest("node disconnected".into()))?;
    if !op.ok {
        return Err(ApiError::BadRequest(format!(
            "worktree failed: {}",
            op.message
        )));
    }
    let worktree_path = op
        .path
        .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("node returned no worktree path")))?;

    // Start a session pinned to the new worktree checkout.
    let session = core::create_session_at(
        state,
        tenant,
        user,
        workspace_id,
        node_id,
        &req.runtime,
        Some(task.title.clone()),
        &worktree_path,
    )
    .await?;

    let in_progress = column_id(state, task.board_id, "In Progress", 2).await?;
    let updated: TaskItem = sqlx::query_as(
        "UPDATE tasks SET workspace_id = $2, assigned_node_id = $3, branch = $4,
                worktree_path = $5, worktree_node_id = $3, session_id = $6,
                column_id = $7, updated_at = now()
         WHERE id = $1 RETURNING *",
    )
    .bind(task_id)
    .bind(workspace_id)
    .bind(node_id)
    .bind(&branch)
    .bind(&worktree_path)
    .bind(session.id)
    .bind(in_progress)
    .fetch_one(&state.db)
    .await?;

    events::record(
        state,
        tenant,
        EventDraft::new("task.work_started")
            .workspace(workspace_id)
            .node(node_id)
            .session(session.id)
            .payload(serde_json::json!({ "task_id": task_id, "branch": branch })),
    )
    .await;
    Ok((updated, session))
}

/// In Progress → Done: record the PR (given or derived from the remote).
pub async fn submit_pr(
    state: &AppState,
    tenant: TenantId,
    task_id: TaskId,
    pr_url: Option<String>,
) -> ApiResult<TaskItem> {
    let task = load_task(state, tenant, task_id).await?;
    let branch = task
        .branch
        .clone()
        .ok_or_else(|| ApiError::BadRequest("task has no branch — start work first".into()))?;

    let url = match pr_url.filter(|u| !u.trim().is_empty()) {
        Some(u) => u,
        None => derive_pr_url(state, &task, &branch)
            .await
            .unwrap_or_else(|| format!("(no remote) branch {branch}")),
    };

    let done = column_id(state, task.board_id, "Done", 3).await?;
    let updated: TaskItem = sqlx::query_as(
        "UPDATE tasks SET pr_url = $2, column_id = $3, updated_at = now()
         WHERE id = $1 RETURNING *",
    )
    .bind(task_id)
    .bind(&url)
    .bind(done)
    .fetch_one(&state.db)
    .await?;

    events::record(
        state,
        tenant,
        EventDraft::new("task.pr_submitted")
            .payload(serde_json::json!({ "task_id": task_id, "pr_url": url })),
    )
    .await;
    Ok(updated)
}

/// Done → prune: remove the task's worktree checkout.
pub async fn prune_worktree(
    state: &AppState,
    tenant: TenantId,
    task_id: TaskId,
) -> ApiResult<TaskItem> {
    let task = load_task(state, tenant, task_id).await?;
    let (Some(path), Some(node_id)) = (task.worktree_path.clone(), task.worktree_node_id) else {
        return Err(ApiError::BadRequest("task has no worktree to prune".into()));
    };

    let rx = state
        .registry
        .request_op(node_id, |request_id| ControlToNode::RemoveWorktree {
            request_id,
            worktree_path: path.clone(),
        })
        .ok_or_else(|| ApiError::BadRequest("node is offline".into()))?;
    let op = tokio::time::timeout(std::time::Duration::from_secs(30), rx)
        .await
        .map_err(|_| ApiError::BadRequest("node did not answer in time".into()))?
        .map_err(|_| ApiError::BadRequest("node disconnected".into()))?;
    if !op.ok {
        return Err(ApiError::BadRequest(format!(
            "prune failed: {}",
            op.message
        )));
    }

    let updated: TaskItem = sqlx::query_as(
        "UPDATE tasks SET worktree_path = NULL, worktree_node_id = NULL, updated_at = now()
         WHERE id = $1 RETURNING *",
    )
    .bind(task_id)
    .fetch_one(&state.db)
    .await?;

    events::record(
        state,
        tenant,
        EventDraft::new("task.worktree_pruned")
            .payload(serde_json::json!({ "task_id": task_id, "path": path })),
    )
    .await;
    Ok(updated)
}

/// Move a task to a named column (drives the board from MCP/AI).
pub async fn move_task(
    state: &AppState,
    tenant: TenantId,
    task_id: TaskId,
    column: &str,
) -> ApiResult<TaskItem> {
    let task = load_task(state, tenant, task_id).await?;
    let col = column_id(state, task.board_id, column, 0).await?;
    Ok(sqlx::query_as(
        "UPDATE tasks SET column_id = $2, updated_at = now() WHERE id = $1 RETURNING *",
    )
    .bind(task_id)
    .bind(col)
    .fetch_one(&state.db)
    .await?)
}

/// Best-effort compare/MR URL from the worktree's git remote.
async fn derive_pr_url(state: &AppState, task: &TaskItem, branch: &str) -> Option<String> {
    let remote: (String,) = sqlx::query_as(
        "SELECT git_remote_url FROM node_workspaces
         WHERE tenant_id = $1 AND workspace_id = $2 AND git_remote_url IS NOT NULL LIMIT 1",
    )
    .bind(task.tenant_id)
    .bind(task.workspace_id?)
    .fetch_optional(&state.db)
    .await
    .ok()??;
    let raw = remote.0;
    // Normalize to https host/path (reuse the discovery normalizer).
    let norm = crate::services::discovery::normalize_remote(&raw); // e.g. github.com/org/repo
    let https = format!("https://{norm}");
    if norm.contains("github.com") {
        Some(format!("{https}/compare/{branch}?expand=1"))
    } else if norm.contains("gitlab") {
        Some(format!(
            "{https}/-/merge_requests/new?merge_request%5Bsource_branch%5D={branch}"
        ))
    } else if norm.contains("bitbucket") {
        Some(format!("{https}/pull-requests/new?source={branch}"))
    } else {
        Some(https)
    }
}
