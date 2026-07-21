//! Feedback: type what should be better, and it lands in a working session.
//!
//! The point is context. Re-explaining the same project to an agent every time
//! is the expensive part, so feedback is queued against a workspace and typed
//! into one long-lived, named session that keeps accumulating it. The rolling
//! log is the record of what was asked for and what came of it.

use axum::extract::{Path, State};
use axum::Json;
use base64::Engine;
use nook_proto::ControlToNode;
use nook_types::*;

use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::events::{self, EventDraft};
use crate::state::AppState;

/// The session all feedback for a workspace is delivered into.
const SESSION_NAME: &str = "Feedback";
/// Setting that remembers which workspace feedback goes to.
const WORKSPACE_SETTING: &str = "feedback_workspace_id";

#[utoipa::path(get, path = "/api/v1/feedback",
    operation_id = "list_feedback",
    responses((status = 200, body = [FeedbackItem])))]
pub async fn list(
    State(state): State<AppState>,
    auth: AuthCtx,
) -> ApiResult<Json<Vec<FeedbackItem>>> {
    let rows: Vec<FeedbackItem> = sqlx::query_as(
        "SELECT * FROM feedback WHERE tenant_id = $1 ORDER BY created_at DESC LIMIT 200",
    )
    .bind(auth.tenant_id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

/// Where feedback is configured to go, if anywhere yet.
#[utoipa::path(get, path = "/api/v1/feedback/target",
    operation_id = "feedback_target",
    responses((status = 200, body = FeedbackTarget)))]
pub async fn target(
    State(state): State<AppState>,
    auth: AuthCtx,
) -> ApiResult<Json<FeedbackTarget>> {
    let workspace_id = configured_workspace(&state, &auth).await?;
    let (name, remote) = match workspace_id {
        Some(id) => {
            let row: Option<(String, Option<String>)> = sqlx::query_as(
                "SELECT w.name, w.git_remote_normalized FROM workspaces w
                 WHERE w.id = $1 AND w.tenant_id = $2",
            )
            .bind(id)
            .bind(auth.tenant_id)
            .fetch_optional(&state.db)
            .await?;
            match row {
                Some((n, r)) => (Some(n), r),
                None => (None, None),
            }
        }
        None => (None, None),
    };
    Ok(Json(FeedbackTarget {
        // A workspace that was deleted counts as unconfigured.
        configured: name.is_some(),
        workspace_id: name.as_ref().and(workspace_id),
        workspace_name: name,
        git_remote: remote,
        session_name: SESSION_NAME.to_string(),
    }))
}

async fn configured_workspace(
    state: &AppState,
    auth: &AuthCtx,
) -> ApiResult<Option<WorkspaceId>> {
    let row: Option<(serde_json::Value,)> = sqlx::query_as(
        "SELECT value FROM settings
         WHERE tenant_id = $1 AND key = $2
         ORDER BY (user_id = $3) DESC LIMIT 1",
    )
    .bind(auth.tenant_id)
    .bind(WORKSPACE_SETTING)
    .bind(auth.user_id)
    .fetch_optional(&state.db)
    .await?;
    Ok(row
        .and_then(|(v,)| v.as_str().map(str::to_string))
        .and_then(|s| s.parse::<uuid::Uuid>().ok())
        .map(WorkspaceId))
}

/// Queue feedback. Picks up the configured workspace unless one is given, and
/// delivers into the named session — creating it if this is the first time.
#[utoipa::path(post, path = "/api/v1/feedback",
    operation_id = "submit_feedback",
    request_body = SubmitFeedbackRequest,
    responses((status = 200, body = FeedbackItem), (status = 400)))]
pub async fn submit(
    State(state): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<SubmitFeedbackRequest>,
) -> ApiResult<Json<FeedbackItem>> {
    let body = req.body.trim().to_string();
    if body.is_empty() {
        return Err(ApiError::BadRequest("feedback cannot be empty".into()));
    }

    let workspace_id = match req.workspace_id {
        Some(id) => {
            // Remember the choice so the next one doesn't ask.
            sqlx::query(
                "INSERT INTO settings (id, tenant_id, user_id, key, value)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (tenant_id, user_id, key)
                 DO UPDATE SET value = EXCLUDED.value, updated_at = now()",
            )
            .bind(SettingId::new().0)
            .bind(auth.tenant_id)
            .bind(auth.user_id)
            .bind(WORKSPACE_SETTING)
            .bind(serde_json::Value::String(id.to_string()))
            .execute(&state.db)
            .await?;
            id
        }
        None => configured_workspace(&state, &auth).await?.ok_or_else(|| {
            ApiError::BadRequest("no feedback workspace configured".into())
        })?,
    };

    // Reuse the standing feedback session; start one when there isn't a live
    // one, so the agent keeps its accumulated context between submissions.
    let existing: Option<(SessionId, NodeId)> = sqlx::query_as(
        "SELECT id, node_id FROM sessions
         WHERE tenant_id = $1 AND workspace_id = $2 AND name = $3
           AND status IN ('starting', 'running', 'detached')
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(auth.tenant_id)
    .bind(workspace_id)
    .bind(SESSION_NAME)
    .fetch_optional(&state.db)
    .await?;

    let (session_id, node_id) = match existing {
        Some(pair) => pair,
        None => {
            let node: Option<(NodeId,)> = sqlx::query_as(
                "SELECT node_id FROM node_workspaces WHERE workspace_id = $1 LIMIT 1",
            )
            .bind(workspace_id)
            .fetch_optional(&state.db)
            .await?;
            let (node_id,) = node.ok_or_else(|| {
                ApiError::BadRequest("that workspace has no checkout on any node".into())
            })?;
            let runtime = req.runtime.unwrap_or_else(|| "claude".to_string());
            let session = crate::services::core::create_session(
                &state,
                auth.tenant_id,
                Some(auth.user_id),
                CreateSessionRequest {
                    workspace_id,
                    node_id,
                    runtime,
                    name: Some(SESSION_NAME.to_string()),
                    path: None,
                },
            )
            .await?;
            (session.id, session.node_id)
        }
    };

    let item: FeedbackItem = sqlx::query_as(
        "INSERT INTO feedback (id, tenant_id, workspace_id, session_id, body, status, created_by)
         VALUES ($1, $2, $3, $4, $5, 'queued', $6) RETURNING *",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(auth.tenant_id)
    .bind(workspace_id)
    .bind(session_id)
    .bind(&body)
    .bind(auth.user_id)
    .fetch_one(&state.db)
    .await?;

    // Type it in. A newline submits, so the agent starts on it immediately.
    let typed = format!("{}\n", prompt_for(&body));
    let delivered = state.registry.send_to_node(
        node_id,
        ControlToNode::SessionInput {
            session_id,
            data_b64: base64::engine::general_purpose::STANDARD.encode(typed.as_bytes()),
        },
    );
    let item: FeedbackItem = sqlx::query_as(
        "UPDATE feedback SET status = $2, updated_at = now() WHERE id = $1 RETURNING *",
    )
    .bind(item.id)
    .bind(if delivered { "delivered" } else { "queued" })
    .fetch_one(&state.db)
    .await?;

    events::record(
        &state,
        auth.tenant_id,
        EventDraft::new("feedback.submitted")
            .actor("user", auth.user_id.0)
            .workspace(workspace_id)
            .session(session_id)
            .payload(serde_json::json!({ "delivered": delivered })),
    )
    .await;

    Ok(Json(item))
}

/// The template every piece of feedback is delivered with. Consistent shape
/// so a later automated pass can recognize and act on these changes.
fn prompt_for(body: &str) -> String {
    format!(
        "[NookOS feedback] {body}\n\n\
         Please implement this in a branch named nookos-feedback/<short-slug>, \
         keep the change focused, and when it builds and tests pass, commit with \
         a message describing the improvement. Then tell me the branch name so I \
         can open a pull request."
    )
}

/// Record the pull request a piece of feedback turned into.
#[utoipa::path(patch, path = "/api/v1/feedback/{id}",
    operation_id = "update_feedback",
    params(("id" = String, Path,)),
    request_body = UpdateFeedbackRequest,
    responses((status = 200, body = FeedbackItem), (status = 404)))]
pub async fn update(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<uuid::Uuid>,
    Json(req): Json<UpdateFeedbackRequest>,
) -> ApiResult<Json<FeedbackItem>> {
    let item: Option<FeedbackItem> = sqlx::query_as(
        "UPDATE feedback SET
            status = COALESCE($3, status),
            pr_url = COALESCE($4, pr_url),
            updated_at = now()
         WHERE id = $1 AND tenant_id = $2 RETURNING *",
    )
    .bind(id)
    .bind(auth.tenant_id)
    .bind(&req.status)
    .bind(&req.pr_url)
    .fetch_optional(&state.db)
    .await?;
    item.map(Json).ok_or(ApiError::NotFound)
}
