//! Automation's own metadata on a card (MAIN-603).
//!
//! A producer has useful things to say about a ticket — a coverage table, a
//! benchmark, a deploy summary — and nowhere to put them that is not a comment
//! thread that grows a line per run. A report is the fix: content addressed by
//! a **key** the producer chose, so re-running replaces rather than appends.
//!
//! **Nook never parses `body_md`** (NG-1). Nothing in this file, or under it,
//! reads the content — no extraction, no link detection, no schema, no enum of
//! report kinds. It is stored as given and rendered by the same sanitising
//! Markdown component the comments use. If a future metadata type needs Nook to
//! *understand* it, the abstraction is wrong and the abstraction gets fixed.
//!
//! Every route resolves its card through `services::tasks::readable_task` and
//! refuses what the reader cannot see: a private card is a 404 here exactly as
//! it is on the detail route (MAIN-76), or its report list becomes a side
//! channel onto a ticket nobody may open — the rule `task_attachments.rs`
//! already states.

use axum::extract::{Path, State};
use axum::Json;
use nook_types::*;

use crate::auth::{AuthCtx, Principal};
use crate::error::ApiResult;
use crate::repo::task_reports::NewTaskReport;
use crate::services::identity::display_name;
use crate::services::{task_reports, tasks};
use crate::state::AppState;

#[utoipa::path(get, path = "/api/v1/tasks/{id}/reports",
    operation_id = "list_task_reports", params(("id" = String, Path,)),
    responses((status = 200, body = [TaskReport]), (status = 404)))]
pub async fn list(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(ident): Path<String>,
) -> ApiResult<Json<Vec<TaskReport>>> {
    let task = tasks::readable_task(&state, auth.tenant_id, auth.user_id, &ident).await?;
    Ok(Json(state.task_reports.list(auth.tenant_id, task).await?))
}

/// Create or **replace** the report at this key (AC-1).
///
/// `PUT` and not `POST` because that is exactly what this is: the key is the
/// address, the body is what is there now, and running the same automation
/// twice leaves one report rather than two. Every refusal is settled before the
/// first write, so a rejected report leaves the card untouched.
#[utoipa::path(put, path = "/api/v1/tasks/{id}/reports/{key}",
    operation_id = "put_task_report",
    params(("id" = String, Path,), ("key" = String, Path,)),
    request_body = PutTaskReportRequest,
    responses((status = 200, body = TaskReport), (status = 400), (status = 403),
              (status = 404)))]
pub async fn put(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path((ident, key)): Path<(String, String)>,
    Json(req): Json<PutTaskReportRequest>,
) -> ApiResult<Json<TaskReport>> {
    task_reports::validate_key(&key)?;
    let title = task_reports::validate_title(&req.title)?;
    task_reports::validate_body(&req.body_md)?;

    let task = tasks::readable_task(&state, auth.tenant_id, auth.user_id, &ident).await?;

    // The cap counts KEYS, so replacing one is never refused for being the
    // twenty-first — otherwise the producer that filled the card up could
    // never correct its own output again.
    //
    // Read-then-write rather than one transaction, deliberately: two producers
    // racing at the boundary can leave twenty-one, and twenty-one reports is
    // not a failure worth serialising every write to prevent. What the cap is
    // actually against is a runaway loop, and a loop cannot race its way past
    // a limit it hits on every iteration.
    let existing = state.task_reports.list(auth.tenant_id, task).await?;
    task_reports::check_room(existing.len() as i64, existing.iter().any(|r| r.key == key))?;

    // A node is `system` because a machine reporting on its own work is not a
    // person; a user token is `user` even when a tool is driving it, because
    // that is who authorised it. The same reading `create_comment` uses.
    let (author_type, author_id) = match auth.principal {
        Principal::Node(_) => ("system", None),
        Principal::User => ("user", Some(auth.user_id.0)),
    };

    let row = state
        .task_reports
        .put(NewTaskReport {
            tenant: auth.tenant_id,
            task,
            key,
            title,
            body_md: req.body_md,
            author_type: author_type.to_string(),
            author_id,
            author_name: display_name(&state, auth.user_id).await,
        })
        .await?;

    // The card's own detail read is what the open ticket re-fetches; a report
    // is content on the card, so it moves with it. NG-3 rules out anything
    // beyond that — no notification, no activity row, no column effect.
    state.registry.publish(
        auth.tenant_id,
        nook_proto::UiEvent::TaskChanged { task_id: task },
    );
    Ok(Json(row))
}

#[utoipa::path(delete, path = "/api/v1/tasks/{id}/reports/{key}",
    operation_id = "delete_task_report",
    params(("id" = String, Path,), ("key" = String, Path,)),
    responses((status = 204), (status = 403), (status = 404)))]
pub async fn delete(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path((ident, key)): Path<(String, String)>,
) -> ApiResult<axum::http::StatusCode> {
    let task = tasks::readable_task(&state, auth.tenant_id, auth.user_id, &ident).await?;
    if !state
        .task_reports
        .delete(auth.tenant_id, task, &key)
        .await?
    {
        return Err(crate::error::ApiError::NotFound);
    }
    state.registry.publish(
        auth.tenant_id,
        nook_proto::UiEvent::TaskChanged { task_id: task },
    );
    Ok(axum::http::StatusCode::NO_CONTENT)
}
