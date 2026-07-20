//! Shared queries used by both REST handlers and MCP tools.

use chrono::{DateTime, Utc};
use nook_types::*;
use sqlx::PgPool;

use crate::error::ApiResult;

pub async fn workspace_locations(
    db: &PgPool,
    tenant: TenantId,
    workspace: WorkspaceId,
) -> ApiResult<Vec<WorkspaceLocation>> {
    let rows: Vec<(
        NodeId,
        String,
        String,
        String,
        Option<String>,
        serde_json::Value,
    )> = sqlx::query_as(
        "SELECT n.id, n.name, n.status, nw.path, nw.git_branch, nw.git_status
             FROM node_workspaces nw
             JOIN nodes n ON n.id = nw.node_id
             WHERE nw.tenant_id = $1 AND nw.workspace_id = $2
             ORDER BY n.name",
    )
    .bind(tenant)
    .bind(workspace)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(node_id, node_name, node_status, path, git_branch, git_status)| WorkspaceLocation {
                node_id,
                node_name,
                node_status,
                path,
                git_branch,
                dirty: git_status
                    .get("dirty")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                worktree: git_status
                    .get("worktree")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            },
        )
        .collect())
}

pub async fn list_workspaces(db: &PgPool, tenant: TenantId) -> ApiResult<Vec<WorkspaceDetail>> {
    let workspaces: Vec<Workspace> =
        sqlx::query_as("SELECT * FROM workspaces WHERE tenant_id = $1 ORDER BY name")
            .bind(tenant)
            .fetch_all(db)
            .await?;
    let mut out = Vec::with_capacity(workspaces.len());
    for workspace in workspaces {
        let locations = workspace_locations(db, tenant, workspace.id).await?;
        out.push(WorkspaceDetail {
            workspace,
            locations,
        });
    }
    Ok(out)
}

pub async fn get_workspace(
    db: &PgPool,
    tenant: TenantId,
    id: WorkspaceId,
) -> ApiResult<Option<WorkspaceDetail>> {
    let workspace: Option<Workspace> =
        sqlx::query_as("SELECT * FROM workspaces WHERE tenant_id = $1 AND id = $2")
            .bind(tenant)
            .bind(id)
            .fetch_optional(db)
            .await?;
    match workspace {
        None => Ok(None),
        Some(workspace) => {
            let locations = workspace_locations(db, tenant, workspace.id).await?;
            Ok(Some(WorkspaceDetail {
                workspace,
                locations,
            }))
        }
    }
}

pub async fn list_nodes(db: &PgPool, tenant: TenantId) -> ApiResult<Vec<Node>> {
    Ok(sqlx::query_as(
        "SELECT id, tenant_id, name, hostname, platform, capabilities, resources, status,
                last_seen_at, created_at, updated_at
         FROM nodes WHERE tenant_id = $1 ORDER BY name",
    )
    .bind(tenant)
    .fetch_all(db)
    .await?)
}

pub async fn list_sessions(
    db: &PgPool,
    tenant: TenantId,
    workspace: Option<WorkspaceId>,
    active_only: bool,
) -> ApiResult<Vec<Session>> {
    let mut sql = String::from("SELECT * FROM sessions WHERE tenant_id = $1");
    if workspace.is_some() {
        sql.push_str(" AND workspace_id = $2");
    }
    if active_only {
        sql.push_str(" AND status IN ('starting', 'running', 'detached')");
    }
    sql.push_str(" ORDER BY created_at DESC");
    let mut q = sqlx::query_as(&sql).bind(tenant);
    if let Some(w) = workspace {
        q = q.bind(w);
    }
    Ok(q.fetch_all(db).await?)
}

pub async fn events_page(
    db: &PgPool,
    tenant: TenantId,
    workspace: Option<WorkspaceId>,
    kind_prefix: Option<String>,
    before: Option<DateTime<Utc>>,
    limit: i64,
) -> ApiResult<EventsPage> {
    let limit = limit.clamp(1, 200);
    let events: Vec<Event> = sqlx::query_as(
        "SELECT * FROM events
         WHERE tenant_id = $1
           AND ($2::uuid IS NULL OR workspace_id = $2)
           AND ($3::text IS NULL OR kind LIKE $3 || '%')
           AND ($4::timestamptz IS NULL OR occurred_at < $4)
         ORDER BY occurred_at DESC, id DESC
         LIMIT $5",
    )
    .bind(tenant)
    .bind(workspace)
    .bind(kind_prefix)
    .bind(before)
    .bind(limit)
    .fetch_all(db)
    .await?;
    let next_cursor = if events.len() as i64 == limit {
        events.last().map(|e| e.occurred_at)
    } else {
        None
    };
    Ok(EventsPage {
        events,
        next_cursor,
    })
}

/// Create a session and instruct the node to start it. Shared by the REST
/// handler and the MCP backend. Resolves the checkout path from workspace +
/// node (first match), then delegates to [`create_session_at`].
pub async fn create_session(
    state: &crate::state::AppState,
    tenant: TenantId,
    created_by: Option<UserId>,
    req: CreateSessionRequest,
) -> ApiResult<Session> {
    use crate::error::ApiError;

    // Pin to an explicit checkout (e.g. a worktree) when given, validating it
    // belongs to this workspace on this node. Otherwise use the first checkout.
    // LIMIT 1: a workspace can have several checkouts on one node (worktrees).
    let path: Option<(String,)> = match &req.path {
        Some(p) => {
            sqlx::query_as(
                "SELECT path FROM node_workspaces
             WHERE tenant_id = $1 AND node_id = $2 AND workspace_id = $3 AND path = $4",
            )
            .bind(tenant)
            .bind(req.node_id)
            .bind(req.workspace_id)
            .bind(p)
            .fetch_optional(&state.db)
            .await?
        }
        None => {
            sqlx::query_as(
                "SELECT path FROM node_workspaces
             WHERE tenant_id = $1 AND node_id = $2 AND workspace_id = $3
             ORDER BY discovered_at LIMIT 1",
            )
            .bind(tenant)
            .bind(req.node_id)
            .bind(req.workspace_id)
            .fetch_optional(&state.db)
            .await?
        }
    };
    let Some((workspace_path,)) = path else {
        return Err(ApiError::BadRequest(
            "that workspace has no checkout on that node".into(),
        ));
    };
    create_session_at(
        state,
        tenant,
        created_by,
        req.workspace_id,
        req.node_id,
        &req.runtime,
        req.name,
        &workspace_path,
    )
    .await
}

/// Create a session pinned to an explicit checkout path — used by the kanban
/// "start work" flow so the session runs in the freshly-created worktree.
#[allow(clippy::too_many_arguments)]
pub async fn create_session_at(
    state: &crate::state::AppState,
    tenant: TenantId,
    created_by: Option<UserId>,
    workspace_id: WorkspaceId,
    node_id: NodeId,
    runtime: &str,
    name: Option<String>,
    workspace_path: &str,
) -> ApiResult<Session> {
    use crate::error::ApiError;

    if !state.registry.node_online(node_id) {
        return Err(ApiError::BadRequest("node is offline".into()));
    }
    let name = name.unwrap_or_else(|| format!("{runtime} session"));
    let session: Session = sqlx::query_as(
        "INSERT INTO sessions (id, tenant_id, workspace_id, node_id, name, runtime, status, created_by)
         VALUES ($1, $2, $3, $4, $5, $6, 'starting', $7) RETURNING *",
    )
    .bind(SessionId::new())
    .bind(tenant)
    .bind(workspace_id)
    .bind(node_id)
    .bind(&name)
    .bind(runtime)
    .bind(created_by)
    .fetch_one(&state.db)
    .await?;

    let sent = state.registry.send_to_node(
        node_id,
        nook_proto::ControlToNode::StartSession {
            session_id: session.id,
            runtime: runtime.to_string(),
            workspace_path: workspace_path.to_string(),
            cols: 120,
            rows: 32,
        },
    );
    if !sent {
        sqlx::query("UPDATE sessions SET status = 'error', updated_at = now() WHERE id = $1")
            .bind(session.id)
            .execute(&state.db)
            .await?;
        return Err(ApiError::BadRequest("node went offline".into()));
    }

    crate::events::record(
        state,
        tenant,
        crate::events::EventDraft::new("session.created")
            .workspace(workspace_id)
            .node(node_id)
            .session(session.id)
            .payload(serde_json::json!({ "runtime": runtime, "name": name })),
    )
    .await;

    Ok(session)
}

pub async fn list_notes(
    db: &PgPool,
    tenant: TenantId,
    workspace: WorkspaceId,
) -> ApiResult<Vec<Note>> {
    Ok(sqlx::query_as(
        "SELECT * FROM notes WHERE tenant_id = $1 AND workspace_id = $2 ORDER BY updated_at DESC",
    )
    .bind(tenant)
    .bind(workspace)
    .fetch_all(db)
    .await?)
}

pub async fn create_note(
    db: &PgPool,
    tenant: TenantId,
    workspace: WorkspaceId,
    req: CreateNoteRequest,
) -> ApiResult<Note> {
    Ok(sqlx::query_as(
        "INSERT INTO notes (id, tenant_id, workspace_id, title, content_md, kind)
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING *",
    )
    .bind(NoteId::new())
    .bind(tenant)
    .bind(workspace)
    .bind(req.title.unwrap_or_else(|| "Rolling notes".into()))
    .bind(&req.content_md)
    .bind(req.kind.unwrap_or_else(|| "rolling".into()))
    .fetch_one(db)
    .await?)
}
