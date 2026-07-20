use axum::extract::{Path, Query, State};
use axum::Json;
use nook_proto::{ControlToNode, UiEvent, WindowAction};
use nook_types::*;
use serde::Deserialize;

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
    let session: Option<Session> =
        sqlx::query_as("SELECT * FROM sessions WHERE id = $1 AND tenant_id = $2")
            .bind(id)
            .bind(auth.tenant_id)
            .fetch_optional(&state.db)
            .await?;
    session.map(Json).ok_or(ApiError::NotFound)
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
    let session = core::create_session(&state, auth.tenant_id, Some(auth.user_id), req).await?;
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
    let session: Option<Session> =
        sqlx::query_as("SELECT * FROM sessions WHERE id = $1 AND tenant_id = $2")
            .bind(id)
            .bind(auth.tenant_id)
            .fetch_optional(&state.db)
            .await?;
    let session = session.ok_or(ApiError::NotFound)?;

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
    let session: Option<Session> =
        sqlx::query_as("SELECT * FROM sessions WHERE id = $1 AND tenant_id = $2")
            .bind(id)
            .bind(auth.tenant_id)
            .fetch_optional(&state.db)
            .await?;
    let session = session.ok_or(ApiError::NotFound)?;
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
    let windows: Vec<SessionWindow> =
        serde_json::from_str(&payload.message).unwrap_or_default();
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
    let session: Option<Session> =
        sqlx::query_as("SELECT * FROM sessions WHERE id = $1 AND tenant_id = $2")
            .bind(id)
            .bind(auth.tenant_id)
            .fetch_optional(&state.db)
            .await?;
    let session = session.ok_or(ApiError::NotFound)?;

    if !state.registry.node_online(session.node_id) {
        return Err(ApiError::BadRequest("node is offline".into()));
    }

    // Reuse the checkout the session was started in; fall back to any checkout
    // of its workspace on that node (the original may have been pruned).
    let path: Option<(String,)> = sqlx::query_as(
        "SELECT path FROM node_workspaces
         WHERE workspace_id = $1 AND node_id = $2
         ORDER BY discovered_at LIMIT 1",
    )
    .bind(session.workspace_id)
    .bind(session.node_id)
    .fetch_optional(&state.db)
    .await?;
    let Some((workspace_path,)) = path else {
        return Err(ApiError::BadRequest(
            "that workspace has no checkout on this node any more".into(),
        ));
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
        "UPDATE sessions SET status = 'starting', ended_at = NULL, updated_at = now()
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
