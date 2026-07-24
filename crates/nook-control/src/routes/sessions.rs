use axum::extract::{Path, Query, State};
use axum::Json;
use nook_proto::{ControlToNode, UiEvent, WindowAction};
use nook_types::*;
use serde::Deserialize;

/// Load a session for a SESSION-CONTENT operation, checking membership.
///
/// Every route that reads or writes what is on a tenant's terminal goes through
/// here, so the authorization decision exists in one place instead of being
/// implied by a `WHERE tenant_id = …` in eight of them. Those clauses were
/// already correct; what they could not do is say *why* — a reviewer could not
/// tell a deliberate isolation boundary from an ordinary scoping habit, and a
/// route added later would look consistent while checking nothing.
///
/// Deliberately 403 and not 404. The caller learns they may not have it, rather
/// than that it does not exist — the refusal message is uniform (see
/// `session_guard`), and session ids are v7 uuids, so this trades an
/// unexploitable existence signal for an error somebody can act on.
async fn session_for_content(
    state: &AppState,
    auth: &AuthCtx,
    id: SessionId,
) -> ApiResult<Session> {
    let session: Option<Session> = sqlx::query_as("SELECT * FROM sessions WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await?;
    let session = session.ok_or(ApiError::NotFound)?;
    auth.require_session_access(state, session.tenant_id)
        .await?;
    Ok(session)
}

use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::events::{self, EventDraft};
use crate::services::core;
use crate::state::AppState;

#[derive(Deserialize, utoipa::IntoParams)]
pub struct SessionsQuery {
    pub workspace_id: Option<WorkspaceId>,
    /// Only sessions that are starting/running/detached.
    pub active: Option<bool>,
}

#[utoipa::path(get, path = "/api/v1/sessions",
    operation_id = "list_sessions",
    params(SessionsQuery),
    responses((status = 200, body = [Session])))]
pub async fn list(
    State(state): State<AppState>,
    auth: AuthCtx,
    Query(q): Query<SessionsQuery>,
) -> ApiResult<Json<Vec<Session>>> {
    Ok(Json(
        core::list_sessions(
            &state.db,
            auth.tenant_id,
            q.workspace_id,
            q.active.unwrap_or(false),
        )
        .await?,
    ))
}

#[utoipa::path(get, path = "/api/v1/sessions/{id}",
    operation_id = "get_session",
    params(("id" = String, Path,)),
    responses((status = 200, body = Session), (status = 404)))]
pub async fn get_one(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<SessionId>,
) -> ApiResult<Json<Session>> {
    let session = session_for_content(&state, &auth, id).await?;
    Ok(Json(session))
}

#[utoipa::path(post, path = "/api/v1/sessions",
    operation_id = "create_session",
    request_body = CreateSessionRequest,
    responses((status = 200, body = Session), (status = 400)))]
pub async fn create(
    State(state): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<CreateSessionRequest>,
) -> ApiResult<Json<Session>> {
    // Starting a session is running a program on that machine.
    auth.require_node_self(req.node_id)?;
    let session = core::create_session(&state, auth.tenant_id, Some(auth.user_id), req).await?;
    Ok(Json(session))
}

/// Open an ad-hoc terminal on a machine — a shell in the node's home directory,
/// no workspace required. The "just give me a prompt on that box" path.
#[utoipa::path(post, path = "/api/v1/nodes/{id}/terminal",
    operation_id = "open_terminal",
    params(("id" = String, Path,)),
    request_body = CreateTerminalRequest,
    responses((status = 200, body = Session), (status = 400)))]
pub async fn open_terminal(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(node_id): Path<NodeId>,
    body: Option<Json<CreateTerminalRequest>>,
) -> ApiResult<Json<Session>> {
    // Same rule as any session: running a shell on a machine is acting on it.
    auth.require_node_self(node_id)?;
    let req = body.map(|Json(r)| r).unwrap_or(CreateTerminalRequest {
        runtime: None,
        name: None,
    });
    let runtime = req.runtime.unwrap_or_else(|| "bash".into());
    let session = core::create_ad_hoc_session(
        &state,
        auth.tenant_id,
        Some(auth.user_id),
        node_id,
        &runtime,
        req.name,
    )
    .await?;
    Ok(Json(session))
}

#[utoipa::path(post, path = "/api/v1/sessions/{id}/kill",
    operation_id = "kill_session",
    params(("id" = String, Path,)),
    responses((status = 204), (status = 404)))]
pub async fn kill(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<SessionId>,
) -> ApiResult<axum::http::StatusCode> {
    let session = session_for_content(&state, &auth, id).await?;
    // A node may only touch sessions running on itself.
    auth.require_node_self(session.node_id)?;

    state.registry.send_to_node(
        session.node_id,
        ControlToNode::KillSession { session_id: id },
    );

    events::record(
        &state,
        auth.tenant_id,
        EventDraft::new("session.kill_requested")
            .actor("user", auth.user_id.0)
            .session(id)
            .node(session.node_id),
    )
    .await;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// Type into a session, as if a human were at the keyboard.
///
/// This is what makes a session drivable from a script: no browser, no SSH,
/// no tmux knowledge — the runtime on the other end (claude, hermes, bash)
/// sees ordinary keystrokes. `enter` is on by default because a prompt left
/// sitting unsubmitted is never what the caller meant.
#[utoipa::path(post, path = "/api/v1/sessions/{id}/input",
    operation_id = "send_session_input",
    params(("id" = String, Path,)),
    request_body = SessionInputRequest,
    responses((status = 204), (status = 400), (status = 404)))]
pub async fn input(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<SessionId>,
    Json(req): Json<SessionInputRequest>,
) -> ApiResult<axum::http::StatusCode> {
    use base64::Engine;

    let session = session_for_content(&state, &auth, id).await?;
    // A node may only touch sessions running on itself.
    auth.require_node_self(session.node_id)?;

    // Ensure the node has a live PTY first: after a node restart its session
    // map is empty and raw input would be silently dropped. AttachSession is
    // idempotent and re-establishes the PTY from tmux.
    state.registry.send_to_node(
        session.node_id,
        ControlToNode::AttachSession {
            session_id: id,
            tmux_session: session.tmux_session.clone(),
        },
    );

    let encode = |s: &str| base64::engine::general_purpose::STANDARD.encode(s.as_bytes());
    let sent = state.registry.send_to_node(
        session.node_id,
        ControlToNode::SessionInput {
            session_id: id,
            data_b64: encode(&req.text),
        },
    );
    if !sent {
        return Err(ApiError::BadRequest("session's node is offline".into()));
    }

    // Enter goes in a SEPARATE write, after a beat.
    //
    // TUI runtimes (Claude Code, codex) read a chunk that ends in \r as pasted
    // text and put the newline *in the box* instead of submitting — the prompt
    // just sits there looking typed but never sent. A shell doesn't care either
    // way, so the delay costs nothing and makes agent runtimes actually answer.
    if req.enter.unwrap_or(true) {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        state.registry.send_to_node(
            session.node_id,
            ControlToNode::SessionInput {
                session_id: id,
                data_b64: encode("\r"),
            },
        );
    }

    events::record(
        &state,
        auth.tenant_id,
        EventDraft::new("session.task_injected")
            .actor("user", auth.user_id.0)
            .session(id)
            .node(session.node_id),
    )
    .await;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// Read back what a session is showing — the visible screen plus optional
/// scrollback. The other half of driving a session from a script: send, then
/// look at what happened.
#[utoipa::path(post, path = "/api/v1/sessions/{id}/output",
    operation_id = "read_session_output",
    params(("id" = String, Path,)),
    request_body = SessionOutputRequest,
    responses((status = 200, body = SessionOutputResponse), (status = 400), (status = 404)))]
pub async fn output(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<SessionId>,
    body: Option<Json<SessionOutputRequest>>,
) -> ApiResult<Json<SessionOutputResponse>> {
    let req = body.map(|Json(r)| r).unwrap_or_default();
    let session = session_for_content(&state, &auth, id).await?;
    // A node may only touch sessions running on itself.
    auth.require_node_self(session.node_id)?;
    let tmux_session = session
        .tmux_session
        .clone()
        .ok_or_else(|| ApiError::BadRequest("session has no terminal yet".into()))?;

    let history_lines = req.history_lines.unwrap_or(0).min(2000);
    let rx = state
        .registry
        .request_op(session.node_id, |request_id| {
            ControlToNode::CaptureSession {
                request_id,
                tmux_session,
                history_lines,
            }
        })
        .ok_or_else(|| ApiError::BadRequest("node is offline".into()))?;
    let payload = tokio::time::timeout(std::time::Duration::from_secs(15), rx)
        .await
        .map_err(|_| ApiError::BadRequest("node did not answer in time".into()))?
        .map_err(|_| ApiError::BadRequest("node disconnected".into()))?;
    if !payload.ok {
        return Err(ApiError::BadRequest(payload.message));
    }

    Ok(Json(SessionOutputResponse {
        runtime: session.runtime,
        status: session.status,
        text: payload.message,
    }))
}

#[utoipa::path(patch, path = "/api/v1/sessions/{id}",
    operation_id = "update_session",
    params(("id" = String, Path,)),
    request_body = UpdateSessionRequest,
    responses((status = 200, body = Session), (status = 404)))]
pub async fn update(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<SessionId>,
    Json(req): Json<UpdateSessionRequest>,
) -> ApiResult<Json<Session>> {
    let name = req.name.trim();
    if name.is_empty() {
        return Err(ApiError::BadRequest("name cannot be empty".into()));
    }
    let session: Option<Session> = sqlx::query_as(
        "UPDATE sessions SET name = $3, updated_at = now()
         WHERE id = $1 AND tenant_id = $2 RETURNING *",
    )
    .bind(id)
    .bind(auth.tenant_id)
    .bind(name)
    .fetch_optional(&state.db)
    .await?;
    let session = session.ok_or(ApiError::NotFound)?;
    // A node may only touch sessions running on itself.
    auth.require_node_self(session.node_id)?;
    state.registry.publish(
        auth.tenant_id,
        UiEvent::SessionStatus {
            session_id: id,
            status: session.status.clone(),
        },
    );
    Ok(Json(session))
}

/// The terminals inside a session — tmux windows. Listing, opening, splitting,
/// focusing, closing and renaming all go through here and always answer with
/// the resulting list.
#[utoipa::path(post, path = "/api/v1/sessions/{id}/windows",
    operation_id = "session_windows",
    params(("id" = String, Path,)),
    request_body = WindowAction,
    responses((status = 200, body = [SessionWindow]), (status = 404)))]
pub async fn windows(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<SessionId>,
    body: Option<Json<WindowAction>>,
) -> ApiResult<Json<Vec<SessionWindow>>> {
    let action = body.map(|Json(a)| a).unwrap_or(WindowAction::List);
    let session = session_for_content(&state, &auth, id).await?;
    // A node may only touch sessions running on itself.
    auth.require_node_self(session.node_id)?;
    let tmux_session = session
        .tmux_session
        .clone()
        .ok_or_else(|| ApiError::BadRequest("session has no terminal yet".into()))?;

    let rx = state
        .registry
        .request_op(session.node_id, |request_id| {
            ControlToNode::SessionWindows {
                request_id,
                tmux_session,
                action,
            }
        })
        .ok_or_else(|| ApiError::BadRequest("node is offline".into()))?;
    let payload = tokio::time::timeout(std::time::Duration::from_secs(15), rx)
        .await
        .map_err(|_| ApiError::BadRequest("node did not answer in time".into()))?
        .map_err(|_| ApiError::BadRequest("node disconnected".into()))?;
    if !payload.ok {
        return Err(ApiError::BadRequest(payload.message));
    }
    // Don't quietly turn a malformed answer into "this session has no
    // terminals" — that reads as a working empty state and hides the fault.
    let windows: Vec<SessionWindow> = serde_json::from_str(&payload.message).map_err(|e| {
        tracing::error!(error = %e, answer = %payload.message, "node sent an unparseable window list");
        ApiError::Internal(anyhow::anyhow!("node sent an unparseable window list"))
    })?;
    Ok(Json(windows))
}

/// Bring a dead session back: same record, same tabs, fresh tmux session.
/// A terminal you closed (or a runtime that exited) shouldn't strand the
/// session — the node's `start` is idempotent, so this just re-issues it.
#[utoipa::path(post, path = "/api/v1/sessions/{id}/restart",
    operation_id = "restart_session",
    params(("id" = String, Path,)),
    responses((status = 200, body = Session), (status = 404)))]
pub async fn restart(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<SessionId>,
) -> ApiResult<Json<Session>> {
    let session = session_for_content(&state, &auth, id).await?;
    // A node may only touch sessions running on itself.
    auth.require_node_self(session.node_id)?;

    if !state.registry.node_online(session.node_id) {
        return Err(ApiError::BadRequest("node is offline".into()));
    }

    // An ad-hoc terminal has no workspace: restart it in the node's home
    // directory, the same empty-path signal it was created with.
    let workspace_path = match session.workspace_id {
        None => String::new(),
        Some(workspace_id) => {
            // Reuse the checkout the session was started in; fall back to any
            // checkout of its workspace on that node (the original may have
            // been pruned).
            let path: Option<(String,)> = sqlx::query_as(
                "SELECT path FROM node_workspaces
                 WHERE workspace_id = $1 AND node_id = $2
                 ORDER BY discovered_at LIMIT 1",
            )
            .bind(workspace_id)
            .bind(session.node_id)
            .fetch_optional(&state.db)
            .await?;
            match path {
                Some((p,)) => p,
                None => {
                    return Err(ApiError::BadRequest(
                        "that workspace has no checkout on this node any more".into(),
                    ))
                }
            }
        }
    };

    let sent = state.registry.send_to_node(
        session.node_id,
        ControlToNode::StartSession {
            session_id: id,
            runtime: session.runtime.clone(),
            workspace_path,
            cols: 120,
            rows: 32,
        },
    );
    if !sent {
        return Err(ApiError::BadRequest("node went offline".into()));
    }

    let session: Session = sqlx::query_as(
        "UPDATE sessions SET status = 'starting', error = NULL, ended_at = NULL,
                updated_at = now()
         WHERE id = $1 RETURNING *",
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;
    state.registry.publish(
        auth.tenant_id,
        UiEvent::SessionStatus {
            session_id: id,
            status: "starting".into(),
        },
    );
    events::record(
        &state,
        auth.tenant_id,
        EventDraft::new("session.restarted")
            .actor("user", auth.user_id.0)
            .session(id)
            .node(session.node_id),
    )
    .await;
    Ok(Json(session))
}

/// Remove a session record. Kills the tmux session first when it's still
/// alive, so "delete" never leaves an orphan running on a node.
#[utoipa::path(delete, path = "/api/v1/sessions/{id}",
    operation_id = "delete_session",
    params(("id" = String, Path,)),
    responses((status = 204), (status = 404)))]
pub async fn delete(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<SessionId>,
) -> ApiResult<axum::http::StatusCode> {
    let session = session_for_content(&state, &auth, id).await?;
    // A node may only touch sessions running on itself.
    auth.require_node_self(session.node_id)?;

    if matches!(session.status.as_str(), "starting" | "running" | "detached") {
        state.registry.send_to_node(
            session.node_id,
            ControlToNode::KillSession { session_id: id },
        );
    }
    sqlx::query("DELETE FROM sessions WHERE id = $1 AND tenant_id = $2")
        .bind(id)
        .bind(auth.tenant_id)
        .execute(&state.db)
        .await?;
    events::record(
        &state,
        auth.tenant_id,
        EventDraft::new("session.deleted")
            .actor("user", auth.user_id.0)
            .node(session.node_id),
    )
    .await;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// `POST /api/v1/sessions/{id}/agent-state` — a hook says what its agent is
/// doing. Guarded by session-content access (only someone who could see the
/// terminal may report on it), it stores the state and, on a real change,
/// fans a `SessionAgentState` event to every browser in the tenant. The report
/// is ephemeral — nothing is written to the database and no notification is
/// raised — so a per-turn `running`/`idle` stream costs the inbox nothing.
#[utoipa::path(post, path = "/api/v1/sessions/{id}/agent-state",
    operation_id = "report_agent_state",
    params(("id" = String, Path,)),
    request_body = ReportAgentStateRequest,
    responses((status = 204), (status = 400), (status = 403), (status = 404)))]
pub async fn report_agent_state(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<SessionId>,
    Json(req): Json<ReportAgentStateRequest>,
) -> ApiResult<axum::http::StatusCode> {
    if !matches!(req.state.as_str(), "running" | "waiting" | "idle") {
        return Err(ApiError::BadRequest(
            "state must be running, waiting, or idle".into(),
        ));
    }
    let session = session_for_content(&state, &auth, id).await?;

    let changed = state
        .registry
        .set_agent_state(session.tenant_id, id, req.window, &req.state);
    if changed {
        state.registry.publish(
            session.tenant_id,
            UiEvent::SessionAgentState {
                session_id: id,
                window: req.window,
                state: req.state,
            },
        );
    }
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// `GET /api/v1/sessions/agent-states` — every live agent state in the tenant,
/// so a browser that just loaded shows the right spinners immediately rather
/// than a blank tab until the next transition. Stale entries are swept here.
#[utoipa::path(get, path = "/api/v1/sessions/agent-states",
    operation_id = "list_agent_states",
    responses((status = 200, body = [AgentStateItem])))]
pub async fn agent_states(
    State(state): State<AppState>,
    auth: AuthCtx,
) -> ApiResult<Json<Vec<AgentStateItem>>> {
    let items = state
        .registry
        .agent_states_for(auth.tenant_id)
        .into_iter()
        .map(|(session_id, window, state)| AgentStateItem {
            session_id,
            window,
            state,
        })
        .collect();
    Ok(Json(items))
}
