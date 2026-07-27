//! Bulk task actions for the backlog (MAIN-154).
//!
//! One endpoint, `POST /api/v1/tasks/bulk`, applies a single action to a batch
//! of tasks. It is deliberately **a loop, not a privilege** (AC-3): every task
//! runs the same authorization the equivalent single-task route would — the id
//! is resolved tenant-scoped, the task must be `visible_to` the caller, and the
//! change goes through the same kanban provider / `set_archived` path. The
//! endpoint adds no power a caller did not already have one task at a time; it
//! only saves them the clicks.
//!
//! Honesty per id (AC-2): a task the action cannot apply to is **skipped with a
//! reason**, never failed — an epic can't take a type change or `agent-ready`,
//! and an id outside the caller's visibility is skipped exactly as the
//! single-task route would refuse it (a 404). A malformed action or value is a
//! request-level `400` (it is wrong for every id), not a batch of skips.

use axum::extract::State;
use axum::Json;
use nook_types::*;

use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::routes::boards::{provider_for_board, set_archived};
use crate::state::AppState;

/// A parsed, validated action — bad input is rejected once, up front.
enum Action {
    AgentReady(bool),
    MoveColumn(String),
    Priority(i32),
    SetType(String),
    /// `Some` assigns/reassigns; `None` unassigns.
    Assignee(Option<uuid::Uuid>),
    Archive,
}

/// Why one task did not take the action. `Skip` becomes a per-id `skipped`;
/// `Fatal` (a real DB/infra error) aborts the whole request.
enum ApplyErr {
    Skip(String),
    Fatal(ApiError),
}

fn parse_action(action: &str, value: Option<&str>) -> ApiResult<Action> {
    let v = value.unwrap_or("").trim();
    match action {
        "agent_ready" => match v {
            "on" | "true" | "1" => Ok(Action::AgentReady(true)),
            "off" | "false" | "0" => Ok(Action::AgentReady(false)),
            _ => Err(ApiError::BadRequest(
                "agent_ready value must be on or off".into(),
            )),
        },
        "move_column" => {
            if v.is_empty() {
                return Err(ApiError::BadRequest(
                    "move_column needs a column type".into(),
                ));
            }
            Ok(Action::MoveColumn(v.to_string()))
        }
        "priority" => {
            let p: i32 = v
                .parse()
                .map_err(|_| ApiError::BadRequest("priority must be 0–4".into()))?;
            if !(0..=4).contains(&p) {
                return Err(ApiError::BadRequest("priority must be 0–4".into()));
            }
            Ok(Action::Priority(p))
        }
        "type" => {
            if v.is_empty() {
                return Err(ApiError::BadRequest("type needs a value".into()));
            }
            Ok(Action::SetType(v.to_string()))
        }
        "assignee" => {
            if v.is_empty() || v == "none" || v == "unassign" {
                Ok(Action::Assignee(None))
            } else {
                let id = v.parse::<uuid::Uuid>().map_err(|_| {
                    ApiError::BadRequest("assignee must be a user uuid or empty to unassign".into())
                })?;
                Ok(Action::Assignee(Some(id)))
            }
        }
        "archive" => Ok(Action::Archive),
        other => Err(ApiError::BadRequest(format!(
            "unknown bulk action {other:?}"
        ))),
    }
}

#[utoipa::path(post, path = "/api/v1/tasks/bulk",
    operation_id = "bulk_tasks",
    request_body = BulkTaskRequest,
    responses((status = 200, body = BulkTaskResponse), (status = 400)))]
pub async fn bulk_tasks(
    State(state): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<BulkTaskRequest>,
) -> ApiResult<Json<BulkTaskResponse>> {
    auth.require_user()?;
    // Validate the action once: a bad action/value is wrong for every id, so it
    // is a 400, not a page of identical skips.
    let action = parse_action(&req.action, req.value.as_deref())?;

    let mut results = Vec::with_capacity(req.task_ids.len());
    for ident in &req.task_ids {
        let item = match apply_one(&state, &auth, ident, &action).await {
            Ok(()) => BulkTaskItemResult {
                id: ident.clone(),
                status: "ok".into(),
                reason: None,
            },
            Err(ApplyErr::Skip(reason)) => BulkTaskItemResult {
                id: ident.clone(),
                status: "skipped".into(),
                reason: Some(reason),
            },
            // A real error (DB down, etc.) is not a per-id outcome — fail loud.
            Err(ApplyErr::Fatal(e)) => return Err(e),
        };
        results.push(item);
    }
    let updated = results.iter().filter(|r| r.status == "ok").count();
    let skipped = results.len() - updated;
    Ok(Json(BulkTaskResponse {
        results,
        updated,
        skipped,
    }))
}

async fn apply_one(
    state: &AppState,
    auth: &AuthCtx,
    ident: &str,
    action: &Action,
) -> Result<(), ApplyErr> {
    // Tenant-scoped resolve; an id in another tenant (or a bad key) is invisible
    // and skipped, exactly as a single-task route returns 404 for it.
    let id = match crate::services::tasks::resolve_id(&state.db, auth.tenant_id, ident).await {
        Ok(id) => id,
        Err(ApiError::NotFound) => return Err(ApplyErr::Skip("not found in your tenant".into())),
        Err(e) => return Err(ApplyErr::Fatal(e)),
    };
    let task: TaskItem = sqlx::query_as("SELECT * FROM tasks WHERE id = $1 AND tenant_id = $2")
        .bind(id)
        .bind(auth.tenant_id)
        .fetch_optional(&state.db)
        .await
        .map_err(|e| ApplyErr::Fatal(e.into()))?
        .ok_or_else(|| ApplyErr::Skip("not found".into()))?;
    // Same visibility predicate as the single-task routes: a private card the
    // caller cannot see is skipped, never touched.
    if !crate::services::tasks::visible_to(&task, auth.user_id) {
        return Err(ApplyErr::Skip("not visible to you".into()));
    }
    let is_epic = task.type_ == "epic";

    match action {
        Action::AgentReady(on) => {
            if is_epic {
                return Err(ApplyErr::Skip("epics are never agent-ready".into()));
            }
            toggle_agent_ready(state, auth, id, *on)
                .await
                .map_err(ApplyErr::Fatal)?;
        }
        Action::SetType(t) => {
            if is_epic {
                return Err(ApplyErr::Skip("epics are containers".into()));
            }
            let mut req = empty_update();
            req.type_ = Some(t.clone());
            apply_update(state, auth, &task, req).await?;
        }
        Action::MoveColumn(ct) => {
            let mut req = empty_update();
            req.column_type = Some(ct.clone());
            apply_update(state, auth, &task, req).await?;
        }
        Action::Priority(p) => {
            let mut req = empty_update();
            req.priority = Some(*p);
            apply_update(state, auth, &task, req).await?;
        }
        Action::Assignee(Some(uid)) => {
            let mut req = empty_update();
            req.assignee_user_id = Some(UserId(*uid));
            apply_update(state, auth, &task, req).await?;
        }
        Action::Assignee(None) => {
            clear_assignee(state, auth, id)
                .await
                .map_err(ApplyErr::Fatal)?;
        }
        Action::Archive => {
            // Reuse the single-task archive path verbatim (event + publish).
            set_archived(state, auth, ident, true)
                .await
                .map_err(ApplyErr::Fatal)?;
        }
    }
    Ok(())
}

/// Apply a field update through the same kanban provider the single-task
/// `update_task` route uses. A provider-level rejection (an invalid column type
/// or task type for this task's board) becomes a per-id skip with its message.
async fn apply_update(
    state: &AppState,
    auth: &AuthCtx,
    task: &TaskItem,
    req: UpdateTaskRequest,
) -> Result<(), ApplyErr> {
    let provider = provider_for_board(state, auth.tenant_id, task.board_id)
        .await
        .map_err(ApplyErr::Fatal)?;
    let kanban = state
        .kanban
        .get(&provider)
        .ok_or_else(|| ApplyErr::Skip("no provider for this board".into()))?;
    match kanban
        .update_task(auth.tenant_id, Some(auth.user_id), task.id, req)
        .await
    {
        Ok(updated) => {
            state.registry.publish(
                auth.tenant_id,
                nook_proto::UiEvent::TaskChanged {
                    task_id: updated.id,
                },
            );
            Ok(())
        }
        Err(e) => Err(ApplyErr::Skip(e.to_string())),
    }
}

/// Clear the assignee — the "unassign" half of the assignee action, matching
/// `task_query::release`'s effect (there is no way to clear via
/// `UpdateTaskRequest`, whose `assignee_user_id` COALESCEs).
async fn clear_assignee(state: &AppState, auth: &AuthCtx, id: TaskId) -> ApiResult<()> {
    sqlx::query(
        "UPDATE tasks SET assignee_user_id = NULL, updated_at = now()
         WHERE id = $1 AND tenant_id = $2",
    )
    .bind(id)
    .bind(auth.tenant_id)
    .execute(&state.db)
    .await?;
    state.registry.publish(
        auth.tenant_id,
        nook_proto::UiEvent::TaskChanged { task_id: id },
    );
    Ok(())
}

/// Attach or detach the `agent-ready` label, creating the label definition on
/// first use (idempotent, like `labels::add`). This is the human's approval
/// gate driven from the UI — never the loop applying it to its own work.
async fn toggle_agent_ready(
    state: &AppState,
    auth: &AuthCtx,
    id: TaskId,
    on: bool,
) -> ApiResult<()> {
    let label_id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO labels (id, tenant_id, name, color)
         VALUES ($1, $2, 'agent-ready', '#f0a000')
         ON CONFLICT (tenant_id, name) DO UPDATE SET name = EXCLUDED.name
         RETURNING id",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(auth.tenant_id)
    .fetch_one(&state.db)
    .await?;
    if on {
        sqlx::query(
            "INSERT INTO task_labels (task_id, label_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        )
        .bind(id)
        .bind(label_id)
        .execute(&state.db)
        .await?;
    } else {
        sqlx::query("DELETE FROM task_labels WHERE task_id = $1 AND label_id = $2")
            .bind(id)
            .bind(label_id)
            .execute(&state.db)
            .await?;
    }
    state.registry.publish(
        auth.tenant_id,
        nook_proto::UiEvent::TaskChanged { task_id: id },
    );
    Ok(())
}

/// An `UpdateTaskRequest` that changes nothing; set one field before use.
fn empty_update() -> UpdateTaskRequest {
    UpdateTaskRequest {
        title: None,
        description: None,
        column_id: None,
        column_type: None,
        position: None,
        assignee_user_id: None,
        priority: None,
        type_: None,
        workspace_id: None,
        visibility: None,
        parent: None,
        expected_updated_at: None,
    }
}
