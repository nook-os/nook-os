//! Board automation (MAIN-73): rules that fire when a task enters a column.
//!
//! A board carries a map from column **type** to an ordered action list. When
//! anything moves a task into a column — a board drag, a `/move`, dispatch,
//! start-work, submit-pr, a claim — [`on_column_change`] runs that type's
//! actions. It keys on the destination's *type*, not its name (so renaming "In
//! Review" changes nothing), and fires only when the column actually changed.
//!
//! Every mover calls it regardless of principal (NG-3): a human drag and an
//! agent's `/move` are the same event to the board. Actions are best-effort
//! (NG-2) — they run inline once, and a failure never fails or delays the move.
//! A failed action records a `task.automation_failed` event (which notifies at
//! error level) and a system-authored comment naming the action and error.

use nook_types::*;
use serde::Deserialize;
use serde_json::Value;

use crate::error::ApiError;
use crate::state::AppState;

/// The column types a rule may key on — the same set the pick query validates.
const COLUMN_TYPES: [&str; 6] = [
    "backlog",
    "unstarted",
    "started",
    "review",
    "completed",
    "canceled",
];

/// One automation action. `deny_unknown_fields` + the tagged enum are what make
/// [`validate`] reject an unknown kind or a stray field on write (AC-1).
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    /// Add a board label to the task, creating it if missing (lowercased).
    AddBoardLabel { label: String },
    /// Remove a board label from the task; a no-op if it is not present.
    RemoveBoardLabel { label: String },
    /// Raise a notification with a `/board?task=…` deep link. `title`/`body`
    /// accept `{key}` / `{title}` / `{url}` tokens.
    Notify {
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        body: Option<String>,
    },
}

impl Action {
    fn kind(&self) -> &'static str {
        match self {
            Action::AddBoardLabel { .. } => "add_board_label",
            Action::RemoveBoardLabel { .. } => "remove_board_label",
            Action::Notify { .. } => "notify",
        }
    }
}

/// Validate an `automation` config for storage (AC-1): a map from a valid column
/// type to a list of well-formed actions. Rejects unknown kinds, malformed
/// action config, unknown column types, and empty label names.
pub fn validate(value: &Value) -> Result<(), ApiError> {
    let map = value.as_object().ok_or_else(|| {
        ApiError::BadRequest("automation must be an object keyed by column type".into())
    })?;
    for (col_type, actions) in map {
        if !COLUMN_TYPES.contains(&col_type.as_str()) {
            return Err(ApiError::BadRequest(format!(
                "{col_type:?} is not a column type — expected one of {}",
                COLUMN_TYPES.join(", ")
            )));
        }
        let list = actions.as_array().ok_or_else(|| {
            ApiError::BadRequest(format!(
                "automation[{col_type:?}] must be a list of actions"
            ))
        })?;
        for action in list {
            // Typed decode rejects unknown kinds and stray fields.
            let parsed: Action = serde_json::from_value(action.clone()).map_err(|e| {
                ApiError::BadRequest(format!("invalid action in {col_type:?}: {e}"))
            })?;
            // Labels must be valid names now, so a stored rule is always runnable.
            match &parsed {
                Action::AddBoardLabel { label } | Action::RemoveBoardLabel { label } => {
                    crate::routes::labels::validate(label)?;
                }
                Action::Notify { .. } => {}
            }
        }
    }
    Ok(())
}

/// Fire the destination column's actions after a move. Never returns an error:
/// automation is best-effort and must not affect the move it follows (AC-5).
///
/// `old_col == new_col` is the common no-move case (a position-only edit, or a
/// claim that did not set a column) and is silent.
pub async fn on_column_change(
    state: &AppState,
    tenant: TenantId,
    task_id: TaskId,
    board_id: BoardId,
    old_col: ColumnId,
    new_col: ColumnId,
) {
    if old_col == new_col {
        return;
    }
    if let Err(e) = run(state, tenant, task_id, board_id, new_col).await {
        // A failure to even LOAD the rules (bad column, unreadable config) is
        // itself best-effort — log and move on; the move already succeeded.
        tracing::warn!("automation: could not run rules for {task_id:?}: {e}");
    }
}

async fn run(
    state: &AppState,
    tenant: TenantId,
    task_id: TaskId,
    board_id: BoardId,
    new_col: ColumnId,
) -> Result<(), sqlx::Error> {
    let col_type: Option<(String,)> =
        sqlx::query_as("SELECT type FROM board_columns WHERE id = $1")
            .bind(new_col)
            .fetch_optional(&state.db)
            .await?;
    let Some((col_type,)) = col_type else {
        return Ok(());
    };

    let automation: Option<(Value,)> =
        sqlx::query_as("SELECT automation FROM boards WHERE id = $1 AND tenant_id = $2")
            .bind(board_id)
            .bind(tenant)
            .fetch_optional(&state.db)
            .await?;
    let Some((automation,)) = automation else {
        return Ok(());
    };

    // Parse leniently: a config that somehow no longer decodes should not stop
    // the move path. Only this column type's actions are read.
    let Some(actions) = automation.get(&col_type).and_then(|v| v.as_array()) else {
        return Ok(());
    };
    for raw in actions {
        let action: Action = match serde_json::from_value(raw.clone()) {
            Ok(a) => a,
            Err(e) => {
                // A malformed stored action is a failure like any other.
                record_failure(state, tenant, task_id, "unknown", &e.to_string()).await;
                continue;
            }
        };
        if let Err(err) = run_action(state, tenant, task_id, &action).await {
            record_failure(state, tenant, task_id, action.kind(), &err).await;
        }
    }
    Ok(())
}

/// Run one action. `Err(message)` drives the failure path (AC-5).
async fn run_action(
    state: &AppState,
    tenant: TenantId,
    task_id: TaskId,
    action: &Action,
) -> Result<(), String> {
    match action {
        Action::AddBoardLabel { label } => attach_label(state, tenant, task_id, label).await,
        Action::RemoveBoardLabel { label } => detach_label(state, tenant, task_id, label).await,
        Action::Notify { title, body } => {
            notify_action(state, tenant, task_id, title.as_deref(), body.as_deref()).await
        }
    }
}

/// Add a board label, creating it (lowercased) if missing (AC-4). Idempotent.
async fn attach_label(
    state: &AppState,
    tenant: TenantId,
    task_id: TaskId,
    label: &str,
) -> Result<(), String> {
    let name = crate::routes::labels::validate(label).map_err(|e| e.to_string())?;
    let label_id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO labels (id, tenant_id, name, color) VALUES ($1, $2, $3, '#f0a000')
         ON CONFLICT (tenant_id, name) DO UPDATE SET name = EXCLUDED.name
         RETURNING id",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(tenant)
    .bind(&name)
    .fetch_one(&state.db)
    .await
    .map_err(|e| e.to_string())?;
    sqlx::query(
        "INSERT INTO task_labels (task_id, label_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(task_id)
    .bind(label_id)
    .execute(&state.db)
    .await
    .map_err(|e| e.to_string())?;
    state
        .registry
        .publish(tenant, nook_proto::UiEvent::TaskChanged { task_id });
    Ok(())
}

/// Remove a board label; a no-op if the label or the link is absent (AC-4).
async fn detach_label(
    state: &AppState,
    tenant: TenantId,
    task_id: TaskId,
    label: &str,
) -> Result<(), String> {
    let name = crate::routes::labels::validate(label).map_err(|e| e.to_string())?;
    let label_id: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT id FROM labels WHERE tenant_id = $1 AND name = $2")
            .bind(tenant)
            .bind(&name)
            .fetch_optional(&state.db)
            .await
            .map_err(|e| e.to_string())?;
    if let Some(label_id) = label_id {
        sqlx::query("DELETE FROM task_labels WHERE task_id = $1 AND label_id = $2")
            .bind(task_id)
            .bind(label_id)
            .execute(&state.db)
            .await
            .map_err(|e| e.to_string())?;
        state
            .registry
            .publish(tenant, nook_proto::UiEvent::TaskChanged { task_id });
    }
    Ok(())
}

/// Raise a notification with the task's deep link, expanding `{key}`/`{title}`/
/// `{url}` in the title and body (AC-4). Reaches the bell, toasts and channels.
async fn notify_action(
    state: &AppState,
    tenant: TenantId,
    task_id: TaskId,
    title: Option<&str>,
    body: Option<&str>,
) -> Result<(), String> {
    let (key, task_title, url) = task_ref(state, task_id).await.map_err(|e| e.to_string())?;
    let expand = |t: &str| {
        t.replace("{key}", &key)
            .replace("{title}", &task_title)
            .replace("{url}", &url)
    };
    let title = expand(title.unwrap_or("{key}"));
    let body = expand(body.unwrap_or("{title}"));

    let draft = crate::services::notify::Draft::new(title)
        .level("info")
        .kind("board.automation")
        .body(body)
        .link(url)
        .payload(serde_json::json!({ "task_id": task_id, "key": key }));
    crate::services::notify::raise(state, tenant, draft).await;
    Ok(())
}

/// The task's display key, title, and `/board?task=…` URL for notify templates.
async fn task_ref(
    state: &AppState,
    task_id: TaskId,
) -> Result<(String, String, String), sqlx::Error> {
    let (key, number, title): (Option<String>, i32, String) = sqlx::query_as(
        "SELECT b.key, t.number, t.title
         FROM tasks t JOIN boards b ON b.id = t.board_id WHERE t.id = $1",
    )
    .bind(task_id)
    .fetch_one(&state.db)
    .await?;
    let key = match key {
        Some(k) => format!("{k}-{number}"),
        None => format!("#{number}"),
    };
    let base = state.cfg.public_base_url.trim_end_matches('/');
    let url = format!("{base}/board?task={task_id}");
    Ok((key, title, url))
}

/// Record a failed action: a `task.automation_failed` event (which `notable`
/// turns into an error-level notification with the board deep link) and a
/// system-authored comment naming the action and error (AC-5).
async fn record_failure(
    state: &AppState,
    tenant: TenantId,
    task_id: TaskId,
    action_kind: &str,
    error: &str,
) {
    crate::events::record(
        state,
        tenant,
        crate::events::EventDraft::new("task.automation_failed").payload(serde_json::json!({
            "task_id": task_id,
            "action": action_kind,
            "error": error,
        })),
    )
    .await;

    // A system comment so the failure is visible on the card itself, not only in
    // the activity feed. Authored by the machine (no user), mirroring how a node
    // comment is stored.
    let _ = sqlx::query(
        "INSERT INTO task_comments (id, tenant_id, task_id, author_type, author_id, author_name, body_md)
         VALUES ($1, $2, $3, 'system', NULL, 'Automation', $4)",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(tenant)
    .bind(task_id)
    .bind(format!(
        "⚠ Automation action `{action_kind}` failed on this move: {error}"
    ))
    .execute(&state.db)
    .await;
    state
        .registry
        .publish(tenant, nook_proto::UiEvent::TaskChanged { task_id });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validate_accepts_a_well_formed_config() {
        let cfg = json!({
            "review": [{ "kind": "notify", "title": "{key} in review" }],
            "completed": [{ "kind": "remove_board_label", "label": "agent-ready" }],
            "started": [{ "kind": "add_board_label", "label": "WIP" }],
        });
        validate(&cfg).expect("valid config");
    }

    #[test]
    fn validate_rejects_an_unknown_kind() {
        let cfg = json!({ "review": [{ "kind": "merge_pr" }] });
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn validate_rejects_an_unknown_column_type() {
        let cfg = json!({ "in-review": [{ "kind": "notify" }] });
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn validate_rejects_a_stray_field() {
        // `deny_unknown_fields` catches a typo'd or extra key.
        let cfg = json!({ "review": [{ "kind": "notify", "titel": "oops" }] });
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn validate_rejects_an_empty_label() {
        let cfg = json!({ "completed": [{ "kind": "add_board_label", "label": "  " }] });
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn validate_rejects_a_non_object() {
        assert!(validate(&json!([])).is_err());
        assert!(validate(&json!({ "review": { "kind": "notify" } })).is_err());
    }
}
