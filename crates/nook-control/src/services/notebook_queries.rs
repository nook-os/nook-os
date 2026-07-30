//! Notebook reads and writes (MAIN-245). Split verbatim out of
//! `services/core.rs`; see that file's header for why.

use nook_db::{params, Db, DbPool, Postgres, TypeMapping};
use nook_types::*;

use crate::error::ApiResult;

pub async fn list_notes(
    db: &DbPool,
    tenant: TenantId,
    workspace: WorkspaceId,
) -> ApiResult<Vec<Note>> {
    Ok(db
        .query_all(
            "SELECT * FROM notes WHERE tenant_id = $1 AND workspace_id = $2 ORDER BY updated_at DESC",
            params![tenant, workspace],
        )
        .await?)
}

pub async fn create_note(
    db: &DbPool,
    tenant: TenantId,
    workspace: WorkspaceId,
    req: CreateNoteRequest,
) -> ApiResult<Note> {
    Ok(db
        .query_one(
            "INSERT INTO notes (id, tenant_id, workspace_id, title, content_md, kind)
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING *",
            params![
                NoteId::new(),
                tenant,
                workspace,
                req.title.unwrap_or_else(|| "Rolling notes".into()),
                &req.content_md,
                req.kind.unwrap_or_else(|| "rolling".into())
            ],
        )
        .await?)
}

/// The workspace's most recent rolling note, if it has one. Moved verbatim out
/// of `mcp_backend` (MAIN-245).
pub async fn latest_rolling_note(
    db: &DbPool,
    tenant: TenantId,
    workspace_id: WorkspaceId,
) -> ApiResult<Option<Note>> {
    Ok(db
        .query_opt(
            "SELECT * FROM notes WHERE tenant_id = $1 AND workspace_id = $2 AND kind = 'rolling'
             ORDER BY updated_at DESC LIMIT 1",
            params![tenant, workspace_id],
        )
        .await?)
}

/// Append to a note's body. Moved verbatim out of `mcp_backend` (MAIN-245).
pub async fn append_to_note(db: &DbPool, note_id: NoteId, addition: String) -> ApiResult<Note> {
    Ok(db
        .query_one(
            &format!(
                "UPDATE notes SET content_md = content_md || $2, updated_at = {now}
                     WHERE id = $1 RETURNING *",
                now = Postgres.now()
            ),
            params![note_id, addition],
        )
        .await?)
}
