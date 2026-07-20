use axum::extract::{Path, State};
use axum::Json;
use nook_types::*;

use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::services::core;
use crate::state::AppState;

#[utoipa::path(get, path = "/api/v1/workspaces/{id}/notes",
    operation_id = "list_notes",
    params(("id" = String, Path,)),
    responses((status = 200, body = [Note])))]
pub async fn list(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(workspace_id): Path<WorkspaceId>,
) -> ApiResult<Json<Vec<Note>>> {
    Ok(Json(
        core::list_notes(&state.db, auth.tenant_id, workspace_id).await?,
    ))
}

#[utoipa::path(post, path = "/api/v1/workspaces/{id}/notes",
    operation_id = "create_note",
    params(("id" = String, Path,)),
    request_body = CreateNoteRequest,
    responses((status = 200, body = Note)))]
pub async fn create(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(workspace_id): Path<WorkspaceId>,
    Json(req): Json<CreateNoteRequest>,
) -> ApiResult<Json<Note>> {
    let note = core::create_note(&state.db, auth.tenant_id, workspace_id, req).await?;
    crate::events::record(
        &state,
        auth.tenant_id,
        crate::events::EventDraft::new("note.created")
            .actor("user", auth.user_id.0)
            .workspace(workspace_id),
    )
    .await;
    Ok(Json(note))
}

#[utoipa::path(patch, path = "/api/v1/notes/{id}",
    operation_id = "update_note",
    params(("id" = String, Path,)),
    request_body = UpdateNoteRequest,
    responses((status = 200, body = Note), (status = 404)))]
pub async fn update(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<NoteId>,
    Json(req): Json<UpdateNoteRequest>,
) -> ApiResult<Json<Note>> {
    let note: Option<Note> = sqlx::query_as(
        "UPDATE notes SET
            title = COALESCE($3, title),
            content_md = COALESCE($4, content_md),
            updated_at = now()
         WHERE id = $1 AND tenant_id = $2 RETURNING *",
    )
    .bind(id)
    .bind(auth.tenant_id)
    .bind(&req.title)
    .bind(&req.content_md)
    .fetch_optional(&state.db)
    .await?;
    note.map(Json).ok_or(ApiError::NotFound)
}
