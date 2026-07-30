//! Workspace-note reads and writes (MAIN-245; queries moved behind
//! [`NotebookRepository`] by MAIN-254).
//!
//! What is left is the one piece of policy two callers share: the defaults a
//! `CreateNoteRequest` falls back to. Everything else forwards straight to the
//! repository.

use nook_types::*;

use crate::error::ApiResult;
use crate::repo::notebook::NotebookRepository;

pub async fn list_notes(
    repo: &dyn NotebookRepository,
    tenant: TenantId,
    workspace: WorkspaceId,
) -> ApiResult<Vec<Note>> {
    repo.list_workspace_notes(tenant, workspace).await
}

/// Create a workspace note. An untitled note is "Rolling notes" of kind
/// `rolling` — the shape an agent appends to — and both the REST route and the
/// MCP tool rely on that, so the defaults live here rather than being written
/// out twice.
pub async fn create_note(
    repo: &dyn NotebookRepository,
    tenant: TenantId,
    workspace: WorkspaceId,
    req: CreateNoteRequest,
) -> ApiResult<Note> {
    repo.create_workspace_note(
        tenant,
        workspace,
        &req.title.unwrap_or_else(|| "Rolling notes".into()),
        &req.content_md,
        &req.kind.unwrap_or_else(|| "rolling".into()),
    )
    .await
}

/// The workspace's most recent rolling note, if it has one.
pub async fn latest_rolling_note(
    repo: &dyn NotebookRepository,
    tenant: TenantId,
    workspace_id: WorkspaceId,
) -> ApiResult<Option<Note>> {
    repo.latest_rolling_note(tenant, workspace_id).await
}

/// Append to a note's body.
pub async fn append_to_note(
    repo: &dyn NotebookRepository,
    note_id: NoteId,
    addition: String,
) -> ApiResult<Note> {
    repo.append_to_note(note_id, &addition).await
}
