//! Session reads and creation (MAIN-245).
//!
//! Split verbatim out of `services/core.rs`. The four `create_*` entry points
//! live together because they share the same placement and runtime rules and
//! differ only in who is asking — a worktree session, an ad-hoc one, an
//! auth-flow one.

use nook_db::{params, Db, DbPool, Postgres, TypeMapping};
use nook_types::*;

use crate::error::ApiResult;

/// List a tenant's sessions, optionally scoped to a single creator (MAIN-133).
/// `creator = Some(user)` returns only sessions that user started — a member's
/// own view, which naturally excludes `created_by NULL` (legacy/MCP) rows since
/// `NULL = user` is never true. `creator = None` returns all sessions (the
/// owner/admin metadata view, and the unchanged view MCP/dispatcher get). This
/// is the metadata/list layer only; content access stays with `session_guard`.
pub async fn list_sessions(
    db: &DbPool,
    tenant: TenantId,
    workspace: Option<WorkspaceId>,
    active_only: bool,
    creator: Option<UserId>,
) -> ApiResult<Vec<Session>> {
    let mut sql = String::from("SELECT * FROM sessions WHERE tenant_id = $1");
    let mut n = 1;
    if workspace.is_some() {
        n += 1;
        sql.push_str(&format!(" AND workspace_id = ${n}"));
    }
    if creator.is_some() {
        n += 1;
        sql.push_str(&format!(" AND created_by = ${n}"));
    }
    if active_only {
        sql.push_str(" AND status IN ('starting', 'running', 'detached')");
    }
    sql.push_str(" ORDER BY created_at DESC");
    // Binds follow the same order the placeholders were numbered above.
    let mut binds = params![tenant];
    if let Some(w) = workspace {
        binds.extend(params![w]);
    }
    if let Some(c) = creator {
        binds.extend(params![c]);
    }
    let mut sessions = db.query_all::<Session>(&sql, binds).await?;
    hydrate_checkouts(db, &mut sessions).await?;
    Ok(sessions)
}

/// Fill each session's `checkout` summary from its `checkout_id` (MAIN-222 AC-5)
/// — id, path, branch, kind, and the node it lives on. A small N+1 over a short
/// page of sessions; sessions with no binding (ad-hoc terminals, pruned
/// checkouts) are left `None`.
pub async fn hydrate_checkouts(db: &DbPool, sessions: &mut [Session]) -> ApiResult<()> {
    for s in sessions.iter_mut() {
        let Some(cid) = s.checkout_id else { continue };
        s.checkout = db
            .query_opt::<nook_types::CheckoutSummary>(
                "SELECT nw.id, nw.path, nw.git_branch AS branch, nw.kind, n.name AS node_name
                 FROM node_workspaces nw JOIN nodes n ON n.id = nw.node_id
                 WHERE nw.id = $1",
                params![cid],
            )
            .await?;
    }
    Ok(())
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
    let path: Option<String> = match &req.path {
        Some(p) => {
            state
                .db
                .query_scalar_opt(
                    "SELECT path FROM node_workspaces
             WHERE tenant_id = $1 AND node_id = $2 AND workspace_id = $3 AND path = $4
               AND missing_at IS NULL",
                    params![tenant, req.node_id, req.workspace_id, p],
                )
                .await?
        }
        None => {
            state
                .db
                .query_scalar_opt(
                    // MAIN-222 AC-3: the default "the checkout" is the CLONE,
                    // deterministically — never a worktree a delete/reinsert
                    // happened to order first.
                    "SELECT path FROM node_workspaces
             WHERE tenant_id = $1 AND node_id = $2 AND workspace_id = $3
               AND kind = 'clone' AND missing_at IS NULL
             ORDER BY discovered_at LIMIT 1",
                    params![tenant, req.node_id, req.workspace_id],
                )
                .await?
        }
    };
    let Some(workspace_path) = path else {
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

    // MAIN-222 AC-2: bind the session to the exact checkout row its working
    // directory is, so restart can return to it and the UI can show where it
    // runs. Resolved from the path (present rows only) — whatever picked the
    // path, primary clone or a freshly-created worktree; NULL if no row yet
    // (discovery may not have scanned a just-made worktree) or an ad-hoc $HOME.
    let checkout_id: Option<NodeWorkspaceId> = if workspace_path.is_empty() {
        None
    } else {
        state
            .db
            .query_scalar_opt(
                "SELECT id FROM node_workspaces
                 WHERE node_id = $1 AND path = $2 AND missing_at IS NULL",
                params![node_id, workspace_path],
            )
            .await?
    };

    let session: Session = state
        .db
        .query_one(
            "INSERT INTO sessions (id, tenant_id, workspace_id, node_id, name, runtime, status, created_by, checkout_id)
         VALUES ($1, $2, $3, $4, $5, $6, 'starting', $7, $8) RETURNING *",
            params![
                SessionId::new(),
                tenant,
                workspace_id,
                node_id,
                &name,
                runtime,
                created_by.map(|u| u.0),
                checkout_id.map(|c| c.0)
            ],
        )
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
        state
            .db
            .exec(
                &format!(
                    "UPDATE sessions SET status = 'error', updated_at = {} WHERE id = $1",
                    Postgres.now()
                ),
                params![session.id],
            )
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

/// Open an ad-hoc terminal: a session with no workspace, run in the node's home
/// directory. An empty `workspace_path` is the wire signal for "home" — the node
/// resolves it to `$HOME` before starting the shell.
pub async fn create_ad_hoc_session(
    state: &crate::state::AppState,
    tenant: TenantId,
    created_by: Option<UserId>,
    node_id: NodeId,
    runtime: &str,
    name: Option<String>,
) -> ApiResult<Session> {
    use crate::error::ApiError;

    if !state.registry.node_online(node_id) {
        return Err(ApiError::BadRequest("node is offline".into()));
    }
    let name = name.unwrap_or_else(|| format!("{runtime} · terminal"));
    let session: Session = state
        .db
        .query_one(
            "INSERT INTO sessions (id, tenant_id, workspace_id, node_id, name, runtime, status, created_by)
         VALUES ($1, $2, NULL, $3, $4, $5, 'starting', $6) RETURNING *",
            params![
                SessionId::new(),
                tenant,
                node_id,
                &name,
                runtime,
                created_by.map(|u| u.0)
            ],
        )
        .await?;

    let sent = state.registry.send_to_node(
        node_id,
        nook_proto::ControlToNode::StartSession {
            session_id: session.id,
            runtime: runtime.to_string(),
            // Empty = the node's home directory. See conn.rs StartSession.
            workspace_path: String::new(),
            cols: 120,
            rows: 32,
        },
    );
    if !sent {
        state
            .db
            .exec(
                &format!(
                    "UPDATE sessions SET status = 'error', updated_at = {} WHERE id = $1",
                    Postgres.now()
                ),
                params![session.id],
            )
            .await?;
        return Err(ApiError::BadRequest("node went offline".into()));
    }

    // No `.workspace(...)`: there isn't one. The node is still recorded, so the
    // activity feed reads "terminal opened on <node>".
    crate::events::record(
        state,
        tenant,
        crate::events::EventDraft::new("session.created")
            .node(node_id)
            .session(session.id)
            .payload(serde_json::json!({ "runtime": runtime, "name": name, "ad_hoc": true })),
    )
    .await;

    Ok(session)
}

/// Start a runtime's LOGIN flow in a session on a node (MAIN-126). Like an
/// ad-hoc terminal, but the node runs the runtime's allowlisted login command
/// instead of a shell, so the device code/URL renders in the live session view
/// and the flow completes as it would in a terminal. Authorization of WHO may do
/// this is the caller's job (node-scoped); this only launches it.
pub async fn create_auth_session(
    state: &crate::state::AppState,
    tenant: TenantId,
    created_by: Option<UserId>,
    node_id: NodeId,
    runtime: &str,
) -> ApiResult<Session> {
    use crate::error::ApiError;

    if !state.registry.node_online(node_id) {
        return Err(ApiError::BadRequest("node is offline".into()));
    }
    let name = format!("authorize {runtime}");
    let session: Session = state
        .db
        .query_one(
            "INSERT INTO sessions (id, tenant_id, workspace_id, node_id, name, runtime, status, created_by)
         VALUES ($1, $2, NULL, $3, $4, $5, 'starting', $6) RETURNING *",
            params![
                SessionId::new(),
                tenant,
                node_id,
                &name,
                runtime,
                created_by.map(|u| u.0)
            ],
        )
        .await?;

    let sent = state.registry.send_to_node(
        node_id,
        nook_proto::ControlToNode::StartAuthSession {
            session_id: session.id,
            runtime: runtime.to_string(),
            cols: 120,
            rows: 32,
        },
    );
    if !sent {
        state
            .db
            .exec(
                &format!(
                    "UPDATE sessions SET status = 'error', updated_at = {} WHERE id = $1",
                    Postgres.now()
                ),
                params![session.id],
            )
            .await?;
        return Err(ApiError::BadRequest("node went offline".into()));
    }

    crate::events::record(
        state,
        tenant,
        crate::events::EventDraft::new("session.created")
            .node(node_id)
            .session(session.id)
            .payload(serde_json::json!({ "runtime": runtime, "name": name, "authorize": true })),
    )
    .await;

    Ok(session)
}

/// One session by id, scoped to its tenant. Moved out of `mcp_backend`
/// (MAIN-245), where the identical query appeared three times.
pub async fn get_session(
    db: &DbPool,
    tenant: TenantId,
    id: SessionId,
) -> ApiResult<Option<Session>> {
    Ok(db
        .query_opt(
            "SELECT * FROM sessions WHERE id = $1 AND tenant_id = $2",
            params![id, tenant],
        )
        .await?)
}
