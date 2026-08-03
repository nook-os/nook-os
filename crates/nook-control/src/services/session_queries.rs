//! Session reads and creation (MAIN-245; queries moved behind
//! [`SessionRepository`] by MAIN-253).
//!
//! What is left here is orchestration, not data access: resolving which
//! checkout to start in, telling the node to start the session, recording the
//! activity event, and undoing the row when the node turned out to be gone. The
//! four `create_*` entry points live together because they share those rules
//! and differ only in who is asking — a worktree session, an ad-hoc terminal,
//! an auth-flow one.

use nook_types::*;

use crate::error::ApiResult;
use crate::repo::sessions::{NewSession, SessionFilter, SessionRepository};
use crate::repo::workspaces::WorkspaceRepository;

/// List a tenant's sessions, optionally scoped to a single creator (MAIN-133).
/// This is the metadata/list layer only; content access stays with
/// `session_guard`.
pub async fn list_sessions(
    sessions: &dyn SessionRepository,
    workspaces: &dyn WorkspaceRepository,
    tenant: TenantId,
    workspace: Option<WorkspaceId>,
    active_only: bool,
    creator: Option<UserId>,
) -> ApiResult<Vec<Session>> {
    let mut rows = sessions
        .list(
            tenant,
            SessionFilter {
                workspace,
                creator,
                active_only,
            },
        )
        .await?;
    hydrate_checkouts(workspaces, &mut rows).await?;
    hydrate_ports(sessions, &mut rows).await?;
    Ok(rows)
}

/// Fill each session's `checkout` summary from its `checkout_id` (MAIN-222 AC-5)
/// — id, path, branch, kind, and the node it lives on. A small N+1 over a short
/// page of sessions; sessions with no binding (ad-hoc terminals, pruned
/// checkouts) are left `None`.
pub async fn hydrate_checkouts(
    workspaces: &dyn WorkspaceRepository,
    sessions: &mut [Session],
) -> ApiResult<()> {
    for s in sessions.iter_mut() {
        let Some(cid) = s.checkout_id else { continue };
        s.checkout = workspaces.checkout_summary(cid).await?;
    }
    Ok(())
}

/// Fill in each session's leased ports (MAIN-301). Not a stored column — a
/// session holds one row per satisfied requirement — so the endpoints that
/// return a `Session` join them in here rather than every caller remembering to.
pub async fn hydrate_ports(
    repo: &dyn crate::repo::sessions::SessionRepository,
    sessions: &mut [Session],
) -> ApiResult<()> {
    for s in sessions.iter_mut() {
        s.leased_ports = repo.leases_of(s.id).await?;
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
    // belongs to this workspace on this node. Otherwise use the clone — a
    // workspace can have several checkouts on one node (worktrees), and
    // MAIN-222 AC-3 makes the default deterministic rather than whichever a
    // delete/reinsert happened to order first.
    let path: Option<String> = match &req.path {
        Some(p) => {
            state
                .workspaces
                .present_checkout_path(tenant, req.workspace_id, req.node_id, p)
                .await?
        }
        None => {
            state
                .workspaces
                .clone_path(tenant, req.workspace_id, req.node_id)
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
        // Hand-started: ad-hoc, and the reconciler must never touch it.
        false,
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
    // Reconciler-owned (MAIN-316). Carried on the INSERT so the unique index
    // refuses a duplicate BEFORE `StartSession` goes to the node — marking
    // afterwards left the losing replica's session running and unattributed.
    managed: bool,
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
    let checkout_id = if workspace_path.is_empty() {
        None
    } else {
        state
            .workspaces
            .present_checkout_id_at(node_id, workspace_path)
            .await?
    };

    let session = state
        .sessions
        .create(NewSession {
            tenant,
            workspace_id: Some(workspace_id),
            node_id,
            name: name.clone(),
            runtime: runtime.to_string(),
            created_by,
            checkout_id,
            managed,
        })
        .await?;

    // Leased AFTER the row exists (the leases reference it) and BEFORE the node
    // is told, so the ports reach the session's environment on its first start
    // rather than a restart. A failure here is a failure to start: the
    // alternative is a session whose app silently falls back to a hardcoded
    // port and collides, which is the bug this replaces.
    let ports = match crate::services::port_leases::lease_for(
        state,
        tenant,
        node_id,
        Some(workspace_id),
        session.id,
    )
    .await
    {
        Ok(p) => p,
        Err(e) => {
            state.sessions.mark_failed_to_start(session.id).await?;
            return Err(e);
        }
    };

    let sent = state.registry.send_to_node(
        node_id,
        nook_proto::ControlToNode::StartSession {
            session_id: session.id,
            runtime: runtime.to_string(),
            workspace_path: workspace_path.to_string(),
            workspace_id: Some(workspace_id),
            cols: 120,
            rows: 32,
            ports: ports.clone(),
        },
    );
    if !sent {
        state.sessions.mark_failed_to_start(session.id).await?;
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
    let session = state
        .sessions
        .create(NewSession {
            tenant,
            workspace_id: None,
            node_id,
            name: name.clone(),
            runtime: runtime.to_string(),
            created_by,
            checkout_id: None,
            // Ad-hoc: an $HOME terminal or a runtime-authorize session.
            managed: false,
        })
        .await?;

    // No workspace, so nothing declares a listener and nothing is leased — an
    // $HOME terminal is not a repo.
    let ports =
        match crate::services::port_leases::lease_for(state, tenant, node_id, None, session.id)
            .await
        {
            Ok(p) => p,
            Err(e) => {
                state.sessions.mark_failed_to_start(session.id).await?;
                return Err(e);
            }
        };

    let sent = state.registry.send_to_node(
        node_id,
        nook_proto::ControlToNode::StartSession {
            session_id: session.id,
            runtime: runtime.to_string(),
            // Empty = the node's home directory. See conn.rs StartSession.
            workspace_path: String::new(),
            // An ad-hoc terminal is in no workspace, so there is no repo whose
            // key it could want.
            workspace_id: None,
            cols: 120,
            rows: 32,
            ports: ports.clone(),
        },
    );
    if !sent {
        state.sessions.mark_failed_to_start(session.id).await?;
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
    let session = state
        .sessions
        .create(NewSession {
            tenant,
            workspace_id: None,
            node_id,
            name: name.clone(),
            runtime: runtime.to_string(),
            created_by,
            checkout_id: None,
            // Ad-hoc: an $HOME terminal or a runtime-authorize session.
            managed: false,
        })
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
        state.sessions.mark_failed_to_start(session.id).await?;
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
    sessions: &dyn SessionRepository,
    tenant: TenantId,
    id: SessionId,
) -> ApiResult<Option<Session>> {
    sessions.get(tenant, id).await
}
