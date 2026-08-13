//! Files on a ticket and on its comments (MAIN-533).
//!
//! Attaching is deliberately a **second** request, not a multipart route per
//! parent: the bytes already have a home (MAIN-532's store), and that upload is
//! the half that has to stream, report progress and be retried. What is left
//! here is a row — which is why a ticket and a comment can share one endpoint
//! shape and one record type.
//!
//! Every route resolves its parent the way the rest of the task surface does
//! and refuses what the reader cannot see: a private card is a 404 here exactly
//! as it is on the detail route (MAIN-76), or its attachment list becomes a
//! side channel onto a ticket nobody may open.

use axum::extract::{Path, State};
use axum::Json;
use nook_types::*;
use uuid::Uuid;

use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::repo::attachments::{NewAttachment, PARENT_COMMENT, PARENT_TASK};
use crate::services::{attachments, tasks};
use crate::state::AppState;

/// `include=comments` widens the answer to the whole thread — the ticket's own
/// attachments and every comment's, each carrying the parent it belongs to.
///
/// It exists because the ticket page renders both at once: without it the page
/// would ask once per comment, which is an N+1 on the one view that has N
/// comments by definition. The default stays the parent that was named.
#[derive(Debug, Default, serde::Deserialize, utoipa::IntoParams)]
pub struct ListScope {
    #[serde(default)]
    pub include: Option<String>,
}

#[utoipa::path(get, path = "/api/v1/tasks/{id}/attachments",
    operation_id = "list_task_attachments",
    params(("id" = String, Path,), ListScope),
    responses((status = 200, body = [TaskAttachment]), (status = 404)))]
pub async fn list_for_task(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(ident): Path<String>,
    axum::extract::Query(scope): axum::extract::Query<ListScope>,
) -> ApiResult<Json<Vec<TaskAttachment>>> {
    let task = readable_task(&state, &auth, &ident).await?;
    Ok(Json(if scope.include.as_deref() == Some("comments") {
        state.attachments.list_thread(auth.tenant_id, task).await?
    } else {
        state
            .attachments
            .list(auth.tenant_id, PARENT_TASK, task.0)
            .await?
    }))
}

/// One attachment by id — what `nook attachments get` resolves before it
/// writes a byte (MAIN-534).
///
/// The listing routes answer "what is on this parent"; an agent handed an id
/// has neither parent, and re-listing a whole thread to find one row is a
/// question with an answer nobody asked for. Metadata BEFORE bytes is also
/// what lets the CLI refuse to overwrite a file without having downloaded 25
/// MiB first.
///
/// Another tenant's id and an id that never existed are the same 404, for the
/// reason the content route already states: a distinguishable answer is a
/// probe (AC-4).
#[utoipa::path(get, path = "/api/v1/attachments/{id}",
    operation_id = "get_task_attachment", params(("id" = String, Path,)),
    responses((status = 200, body = TaskAttachment), (status = 404)))]
pub async fn get_one(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<TaskAttachment>> {
    Ok(Json(
        crate::services::attachments::readable(&state, auth.tenant_id, auth.user_id, id).await?,
    ))
}

#[utoipa::path(post, path = "/api/v1/tasks/{id}/attachments",
    operation_id = "attach_to_task", params(("id" = String, Path,)),
    request_body = AttachContentRequest,
    responses((status = 200, body = TaskAttachment), (status = 404)))]
pub async fn attach_to_task(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(ident): Path<String>,
    Json(req): Json<AttachContentRequest>,
) -> ApiResult<Json<TaskAttachment>> {
    auth.require_user()?;
    let task = readable_task(&state, &auth, &ident).await?;
    let row = attach(&state, &auth, PARENT_TASK, task.0, req.user_content_id).await?;
    state.registry.publish(
        auth.tenant_id,
        nook_proto::UiEvent::TaskChanged { task_id: task },
    );
    Ok(Json(row))
}

#[utoipa::path(get, path = "/api/v1/comments/{id}/attachments",
    operation_id = "list_comment_attachments", params(("id" = String, Path,)),
    responses((status = 200, body = [TaskAttachment]), (status = 404)))]
pub async fn list_for_comment(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Vec<TaskAttachment>>> {
    readable_comment(&state, &auth, id).await?;
    Ok(Json(
        state
            .attachments
            .list(auth.tenant_id, PARENT_COMMENT, id)
            .await?,
    ))
}

#[utoipa::path(post, path = "/api/v1/comments/{id}/attachments",
    operation_id = "attach_to_comment", params(("id" = String, Path,)),
    request_body = AttachContentRequest,
    responses((status = 200, body = TaskAttachment), (status = 403), (status = 404)))]
pub async fn attach_to_comment(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<Uuid>,
    Json(req): Json<AttachContentRequest>,
) -> ApiResult<Json<TaskAttachment>> {
    auth.require_user()?;
    // Hanging a file on somebody else's comment would be putting words in their
    // mouth — the same rule editing one already follows.
    let (author, task) = state
        .tasks
        .comment_author(id, auth.tenant_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if author != Some(auth.user_id.0) {
        return Err(ApiError::ForbiddenMsg(
            "only the author can attach a file to a comment".into(),
        ));
    }
    let row = attach(&state, &auth, PARENT_COMMENT, id, req.user_content_id).await?;
    state.registry.publish(
        auth.tenant_id,
        nook_proto::UiEvent::TaskChanged { task_id: task },
    );
    Ok(Json(row))
}

#[utoipa::path(delete, path = "/api/v1/attachments/{id}",
    operation_id = "detach_content", params(("id" = String, Path,)),
    responses((status = 204), (status = 403), (status = 404)))]
pub async fn detach(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<Uuid>,
) -> ApiResult<axum::http::StatusCode> {
    auth.require_user()?;
    let row = state
        .attachments
        .get(auth.tenant_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;

    if row.attached_by != auth.user_id && !auth.is_tenant_admin(state.identity.as_ref()).await? {
        return Err(ApiError::ForbiddenMsg(
            "only the person who attached this, or a tenant owner or admin, can remove it".into(),
        ));
    }

    // Deleting the CONTENT row is what removes the join row: the foreign key
    // cascades, so there is one delete to get right rather than two to keep in
    // step (AC-6).
    crate::services::attachments::purge_content(&state, auth.tenant_id, &[row.user_content_id])
        .await?;

    if let Some(task) =
        attachments::parent_task(&state, auth.tenant_id, &row.parent_kind, row.parent_id).await?
    {
        state.registry.publish(
            auth.tenant_id,
            nook_proto::UiEvent::TaskChanged { task_id: task },
        );
    }
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// Record the join, having first confirmed the content is this tenant's.
///
/// The check is not redundant with the content repository's own tenant scoping:
/// without it a caller could attach an id they cannot read and learn from the
/// answer whether it exists. `get` returning `None` for another tenant is what
/// turns that probe into a 404.
async fn attach(
    state: &AppState,
    auth: &AuthCtx,
    parent_kind: &str,
    parent_id: Uuid,
    user_content_id: Uuid,
) -> ApiResult<TaskAttachment> {
    state
        .user_content
        .get(user_content_id, auth.tenant_id)
        .await?
        .ok_or(ApiError::NotFound)?;

    state
        .attachments
        .attach(NewAttachment {
            tenant: auth.tenant_id,
            user_content_id,
            parent_kind: parent_kind.to_string(),
            parent_id,
            attached_by: auth.user_id,
        })
        .await
}

/// The task behind an identifier, refused as 404 when this viewer may not see
/// it (MAIN-76). The rule itself lives in the service layer, because MCP
/// resolves the same parents and a second copy of a visibility check is a
/// second chance to get one wrong (MAIN-534).
async fn readable_task(state: &AppState, auth: &AuthCtx, ident: &str) -> ApiResult<TaskId> {
    attachments::readable_task(state, auth.tenant_id, auth.user_id, ident).await
}

/// A comment's task, refused the same way — a comment is exactly as visible as
/// the ticket it hangs on.
async fn readable_comment(state: &AppState, auth: &AuthCtx, id: Uuid) -> ApiResult<TaskId> {
    let task = attachments::parent_task(state, auth.tenant_id, PARENT_COMMENT, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let row = state
        .tasks
        .get_row(auth.tenant_id, task)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !tasks::visible_to(&row, auth.user_id) {
        return Err(ApiError::NotFound);
    }
    Ok(task)
}
