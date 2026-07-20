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
/// Setting that remembers the branch improvements should land on.
const BRANCH_SETTING: &str = "feedback_branch";
/// Setting that remembers what to do with a change once it works.
const INSTRUCTIONS_SETTING: &str = "feedback_instructions";
/// Repo-local fallback for those instructions, so they can live with the code
/// they describe. A flat name: the node only reads files at a checkout root.
const INSTRUCTIONS_FILE: &str = ".nook-feedback.md";

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
    let (instructions, instructions_from_repo) =
        resolved_instructions(&state, &auth, workspace_id).await?;
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
        branch: setting(&state, &auth, BRANCH_SETTING).await?,
        instructions,
        instructions_from_repo,
    }))
}

/// What the agent is told to do with a finished change.
///
/// The setting wins; failing that, `.nook-feedback.md` in the repo, so a
/// project can carry its own rules ("open a PR, never push to main") without
/// anyone remembering to configure them here. Returns the text and whether it
/// came from the repo, which is the difference between the UI showing an
/// editable value and showing where the file is.
async fn resolved_instructions(
    state: &AppState,
    auth: &AuthCtx,
    workspace_id: Option<WorkspaceId>,
) -> ApiResult<(Option<String>, bool)> {
    if let Some(text) = setting(state, auth, INSTRUCTIONS_SETTING).await? {
        if !text.trim().is_empty() {
            return Ok((Some(text), false));
        }
    }
    let Some(workspace_id) = workspace_id else {
        return Ok((None, false));
    };
    let from_repo = crate::routes::gitops::read_from_any_checkout(
        state,
        auth.tenant_id,
        workspace_id,
        INSTRUCTIONS_FILE,
    )
    .await
    .and_then(|(_, bytes)| String::from_utf8(bytes).ok())
    .filter(|t| !t.trim().is_empty());
    Ok((from_repo, true))
}

/// Point feedback at a repo and branch.
///
/// Separate from submitting because the target has to be changeable: it was
/// previously only recorded on the first send, which meant the first repo you
/// ever picked was the one you were stuck with.
#[utoipa::path(put, path = "/api/v1/feedback/target",
    operation_id = "set_feedback_target",
    request_body = SetFeedbackTargetRequest,
    responses((status = 200, body = FeedbackTarget), (status = 404)))]
pub async fn set_target(
    State(state): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<SetFeedbackTargetRequest>,
) -> ApiResult<Json<FeedbackTarget>> {
    // Repointing feedback aims that typing at a different repo.
    auth.require_user()?;
    let owned: Option<(WorkspaceId,)> =
        sqlx::query_as("SELECT id FROM workspaces WHERE id = $1 AND tenant_id = $2")
            .bind(req.workspace_id)
            .bind(auth.tenant_id)
            .fetch_optional(&state.db)
            .await?;
    if owned.is_none() {
        return Err(ApiError::NotFound);
    }

    put_setting(
        &state,
        &auth,
        WORKSPACE_SETTING,
        serde_json::Value::String(req.workspace_id.to_string()),
    )
    .await?;
    let branch = req
        .branch
        .map(|b| b.trim().to_string())
        .filter(|b| !b.is_empty());
    put_setting(
        &state,
        &auth,
        BRANCH_SETTING,
        match &branch {
            Some(b) => serde_json::Value::String(b.clone()),
            None => serde_json::Value::Null,
        },
    )
    .await?;
    let instructions = req
        .instructions
        .map(|i| i.trim().to_string())
        .filter(|i| !i.is_empty());
    put_setting(
        &state,
        &auth,
        INSTRUCTIONS_SETTING,
        match &instructions {
            Some(i) => serde_json::Value::String(i.clone()),
            None => serde_json::Value::Null,
        },
    )
    .await?;

    target(State(state), auth).await
}

/// Read a per-user setting, falling back to the tenant-wide one.
async fn setting(state: &AppState, auth: &AuthCtx, key: &str) -> ApiResult<Option<String>> {
    let row: Option<(serde_json::Value,)> = sqlx::query_as(
        "SELECT value FROM settings
         WHERE tenant_id = $1 AND key = $2
           AND (user_id = $3 OR user_id IS NULL)
         ORDER BY (user_id = $3) DESC LIMIT 1",
    )
    .bind(auth.tenant_id)
    .bind(key)
    .bind(auth.user_id)
    .fetch_optional(&state.db)
    .await?;
    Ok(row.and_then(|(v,)| v.as_str().map(str::to_string)))
}

async fn put_setting(
    state: &AppState,
    auth: &AuthCtx,
    key: &str,
    value: serde_json::Value,
) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO settings (id, tenant_id, scope, user_id, key, value)
         VALUES ($1, $2, 'user', $3, $4, $5)
         ON CONFLICT (tenant_id, scope, user_id, key)
         DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(SettingId::new().0)
    .bind(auth.tenant_id)
    .bind(auth.user_id)
    .bind(key)
    .bind(value)
    .execute(&state.db)
    .await?;
    Ok(())
}

async fn configured_workspace(state: &AppState, auth: &AuthCtx) -> ApiResult<Option<WorkspaceId>> {
    Ok(setting(state, auth, WORKSPACE_SETTING)
        .await?
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
    // Submitting feedback types into a live agent session on a node. That is
    // action at a distance, and a machine credential should not have it.
    auth.require_user()?;
    let body = req.body.trim().to_string();
    if body.is_empty() {
        return Err(ApiError::BadRequest("feedback cannot be empty".into()));
    }

    let workspace_id = match req.workspace_id {
        Some(id) => {
            // Remember the choice so the next one doesn't ask.
            put_setting(
                &state,
                &auth,
                WORKSPACE_SETTING,
                serde_json::Value::String(id.to_string()),
            )
            .await?;
            id
        }
        None => configured_workspace(&state, &auth)
            .await?
            .ok_or_else(|| ApiError::BadRequest("no feedback workspace configured".into()))?,
    };

    // Reuse the standing feedback session; start one when there isn't a live
    // one, so the agent keeps its accumulated context between submissions.
    //
    // Matched by name, but not only the exact one this code creates: someone
    // who opens a session and calls it "Nook@OS: Feedback Session" has told us
    // plainly where feedback should go, and spawning a second agent beside it
    // — which is what an exact match did — is both wasteful and invisible.
    let existing: Option<(SessionId, NodeId)> = sqlx::query_as(
        "SELECT id, node_id FROM sessions
         WHERE tenant_id = $1 AND workspace_id = $2
           AND (name = $3 OR name ILIKE '%feedback%')
           AND status IN ('starting', 'running', 'detached')
         ORDER BY (name = $3) DESC, created_at DESC LIMIT 1",
    )
    .bind(auth.tenant_id)
    .bind(workspace_id)
    .bind(SESSION_NAME)
    .fetch_optional(&state.db)
    .await?;

    let (session_id, node_id, freshly_started) = match existing {
        Some((s, n)) => (s, n, false),
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
            (session.id, session.node_id, true)
        }
    };

    // A session that was just created has no PTY on the node yet, and input
    // sent before it exists is dropped on the floor. Wait for the node to
    // report the terminal before typing into it.
    if freshly_started {
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            let ready: Option<(Option<String>,)> =
                sqlx::query_as("SELECT tmux_session FROM sessions WHERE id = $1")
                    .bind(session_id)
                    .fetch_optional(&state.db)
                    .await?;
            if ready.and_then(|(t,)| t).is_some() {
                // The runtime still needs a moment to start reading stdin —
                // an agent draws its UI first.
                tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                break;
            }
        }
    }

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

    // Type it in, then press Enter as a separate keystroke.
    //
    // Two things this gets right that one `"{body}\n"` write did not. The
    // terminal is in raw mode, where Enter is CR (0x0D) — LF is a newline
    // *inside* the composer, so the whole prompt sat there looking submitted
    // and never was. And an agent TUI reads the burst and the key as one
    // paste when they arrive together, so the CR has to land after the
    // composer has settled.
    let branch = setting(&state, &auth, BRANCH_SETTING).await?;
    let (instructions, _) = resolved_instructions(&state, &auth, Some(workspace_id)).await?;
    let type_into_session = |text: &str| {
        state.registry.send_to_node(
            node_id,
            ControlToNode::SessionInput {
                session_id,
                data_b64: base64::engine::general_purpose::STANDARD.encode(text.as_bytes()),
            },
        )
    };
    let delivered = type_into_session(&prompt_for(
        &body,
        branch.as_deref(),
        instructions.as_deref(),
    ));
    if delivered {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        type_into_session("\r");
    }
    let item: FeedbackItem = sqlx::query_as(
        "UPDATE feedback SET status = $2, updated_at = now() WHERE id = $1 RETURNING *",
    )
    .bind(item.id)
    // 'queued' is not a holding pattern — nothing retries it. Record what
    // actually happened so the log doesn't imply work is under way.
    .bind(if delivered { "delivered" } else { "dropped" })
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
fn prompt_for(body: &str, branch: Option<&str>, instructions: Option<&str>) -> String {
    // Configured instructions replace the built-in wording entirely. Someone
    // who has written down what to do with a finished change has said it
    // better than a generic template can, and appending ours after theirs
    // would just contradict them.
    if let Some(instructions) = instructions {
        return format!("[NookOS feedback] {body}\n\n{}", instructions.trim());
    }
    // A configured branch is the whole point of the feedback loop: the work
    // lands somewhere isolated that CI already deploys, instead of on whatever
    // happened to be checked out.
    let where_to = match branch {
        Some(b) => format!(
            "Work on the `{b}` branch (check it out or create it from the current \
             HEAD if it does not exist), and push there when done."
        ),
        None => "Work in a branch named nookos-feedback/<short-slug>, and tell me \
                 the branch name when done."
            .to_string(),
    };
    format!(
        "[NookOS feedback] {body}\n\n\
         {where_to} Keep the change focused, and commit with a message describing \
         the improvement once it builds and the tests pass."
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
