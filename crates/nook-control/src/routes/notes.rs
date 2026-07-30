use axum::extract::{Path, State};
use axum::Json;
use nook_types::*;

use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::services::notebook_queries;
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
        notebook_queries::list_notes(&*state.notebook, auth.tenant_id, workspace_id).await?,
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
    let note =
        notebook_queries::create_note(&*state.notebook, auth.tenant_id, workspace_id, req).await?;
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
    let note: Option<Note> = state
        .notebook
        .update_workspace_note(id, auth.tenant_id, req.title, req.content_md)
        .await?;
    note.map(Json).ok_or(ApiError::NotFound)
}
