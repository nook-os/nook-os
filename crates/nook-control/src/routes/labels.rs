//! Labels — the human approval gate.
//!
//! `agent-ready` is the one signal that says an agent may pick a task up, and
//! the reason it is a label rather than a column or a flag is that a human has
//! to be able to apply and remove it casually, on any task, without moving the
//! work or changing its state.
//!
//! Every mutation here is idempotent. Agents call `PUT`/`DELETE` without
//! reading first, and installers re-run; making "already true" an error would
//! turn ordinary retries into failures.

use axum::extract::{Path, State};
use axum::Json;
use nook_types::*;

use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::services::tasks;
use crate::state::AppState;

pub(crate) fn validate(name: &str) -> Result<String, ApiError> {
    let n = name.trim().to_lowercase();
    if n.is_empty() || n.len() > 48 {
        return Err(ApiError::BadRequest(
            "a label name must be 1–48 characters".into(),
        ));
    }
    // Lowercased on the way in so `Agent-Ready` and `agent-ready` cannot both
    // exist. Two labels that render identically but compare differently would
    // make the gate look applied when it is not — the worst possible failure
    // for this particular label.
    Ok(n)
}

#[utoipa::path(get, path = "/api/v1/labels",
    operation_id = "list_labels", responses((status = 200, body = [Label])))]
pub async fn list(State(state): State<AppState>, auth: AuthCtx) -> ApiResult<Json<Vec<Label>>> {
    Ok(Json(state.tasks.list_labels(auth.tenant_id).await?))
}

/// Create, or return what is already there.
///
/// `200` on an existing name rather than `409`: callers want the label to
/// exist, not to be told who created it first.
#[utoipa::path(post, path = "/api/v1/labels",
    operation_id = "create_label", request_body = CreateLabelRequest,
    responses((status = 200, body = Label)))]
pub async fn create(
    State(state): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<CreateLabelRequest>,
) -> ApiResult<Json<Label>> {
    let name = validate(&req.name)?;
    let color = req.color.unwrap_or_else(|| "#f0a000".into());
    Ok(Json(
        state
            .tasks
            .upsert_label(auth.tenant_id, &name, &color)
            .await?,
    ))
}

#[utoipa::path(delete, path = "/api/v1/labels/{id}",
    operation_id = "delete_label", params(("id" = String, Path,)),
    responses((status = 204), (status = 404)))]
pub async fn delete(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<uuid::Uuid>,
) -> ApiResult<axum::http::StatusCode> {
    // task_labels cascades, so the tasks themselves are untouched — removing a
    // label from the vocabulary must not delete anybody's work.
    let res = state.tasks.delete_label(id, auth.tenant_id).await?;
    if res == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// Attach a label. Succeeds when it is already attached.
#[utoipa::path(put, path = "/api/v1/tasks/{id}/labels/{label}",
    operation_id = "add_task_label",
    params(("id" = String, Path,), ("label" = String, Path,)),
    responses((status = 200, body = [Label]), (status = 404)))]
pub async fn add(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path((ident, label)): Path<(String, String)>,
) -> ApiResult<Json<Vec<Label>>> {
    let task = tasks::resolve_id(state.tasks.as_ref(), auth.tenant_id, &ident).await?;
    let label_id = resolve_label(&state, auth.tenant_id, &label).await?;

    let res = state.tasks.attach_label_id(task, label_id).await?;

    // Only record an event when something actually changed. An agent that
    // re-applies a label on every poll would otherwise flood the timeline the
    // human reads to see what happened.
    if res > 0 {
        record(&state, &auth, task, &label, "task.label.added").await;
    }
    labels_of(&state, task).await.map(Json)
}

/// Detach a label. Succeeds when it is already absent.
#[utoipa::path(delete, path = "/api/v1/tasks/{id}/labels/{label}",
    operation_id = "remove_task_label",
    params(("id" = String, Path,), ("label" = String, Path,)),
    responses((status = 200, body = [Label]), (status = 404)))]
pub async fn remove(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path((ident, label)): Path<(String, String)>,
) -> ApiResult<Json<Vec<Label>>> {
    let task = tasks::resolve_id(state.tasks.as_ref(), auth.tenant_id, &ident).await?;
    let label_id = resolve_label(&state, auth.tenant_id, &label).await?;
    let res = state.tasks.detach_label_id(task, label_id).await?;
    if res > 0 {
        record(&state, &auth, task, &label, "task.label.removed").await;
    }
    labels_of(&state, task).await.map(Json)
}

/// Accept a label id **or** a name, because an agent knows `agent-ready` and
/// not its uuid.
async fn resolve_label(state: &AppState, tenant: TenantId, ident: &str) -> ApiResult<uuid::Uuid> {
    if let Ok(id) = ident.parse::<uuid::Uuid>() {
        return state
            .tasks
            .label_id_by_uuid(id, tenant)
            .await?
            .ok_or(ApiError::NotFound);
    }
    let name = validate(ident)?;
    state
        .tasks
        .label_id_by_name(tenant, &name)
        .await?
        .ok_or(ApiError::NotFound)
}

pub async fn labels_of(state: &AppState, task: TaskId) -> ApiResult<Vec<Label>> {
    state.tasks.labels_of_task(task).await
}

async fn record(state: &AppState, auth: &AuthCtx, task: TaskId, label: &str, kind: &'static str) {
    // Title feeds the activity timeline (unchanged); the human key lets the
    // escalation-label notification name the task WITHOUT its title, so an
    // escalation on a private card leaks nothing new (MAIN-91 AC-2, NG-4).
    let meta = state.tasks.task_naming(task).await.ok().flatten();
    let (title, key) = match meta {
        Some((title, Some(n), Some(k))) => (title, format!("{k}-{n}")),
        Some((title, _, _)) => (title, String::new()),
        None => (String::new(), String::new()),
    };
    crate::events::record(
        state,
        auth.tenant_id,
        crate::events::EventDraft::new(kind)
            .actor("user", auth.user_id.0)
            .payload(serde_json::json!({
                "task_id": task,
                "task": title,
                "label": label,
                "key": key,
            })),
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Agent-Ready` and `agent-ready` must be the same label. Two that render
    /// identically but compare differently would let the approval gate look
    /// applied when it is not.
    #[test]
    fn label_names_are_case_folded() {
        assert_eq!(validate("Agent-Ready").unwrap(), "agent-ready");
        assert_eq!(validate("  BLOCKED  ").unwrap(), "blocked");
        assert!(validate("").is_err());
        assert!(validate("   ").is_err());
        assert!(validate(&"x".repeat(49)).is_err());
    }
}
