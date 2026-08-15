//! What attachments need from more than one route (MAIN-533).
//!
//! Removal is here because three routes end in the same three steps — drop the
//! join row, drop the content row, drop the bytes — and only the first is a
//! delete a database could have performed. The bytes live in an object store no
//! cascade reaches, which is why AC-7's "deleting a ticket takes its
//! attachments with it" is code rather than a foreign key in the migration.

use uuid::Uuid;

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use nook_types::{human_size, AttachmentContent, TaskAttachment, TaskId, TenantId, UserId};

/// How much text an MCP read will put in a reply, above which the answer is a
/// pointer instead (MAIN-534 AC-5).
///
/// A cap and not just a type check, because "is it text" and "does it belong in
/// a transcript" are different questions: a 25 MiB log is text and inlining it
/// would spend the whole context an agent needs for the work. The number is
/// generous for a spec or a schema and hopeless for a dump, which is the line
/// this is trying to draw.
const MAX_INLINE_BYTES: i64 = 256 * 1024;

/// Delete these content rows and their bytes.
///
/// The row goes first, for MAIN-532's reason: a record pointing at bytes that
/// are gone serves as a 500, while an object nobody records is invisible and
/// harmless. A failure on either half is logged and the rest continue — a
/// ticket that would not delete because one orphaned object refused to is a
/// worse outcome than an orphaned object.
///
/// One round trip per file rather than a batch: an attachment set is a handful
/// of items and this runs when something is being deleted, never on a read
/// path.
pub async fn purge_content(
    state: &AppState,
    tenant: TenantId,
    content_ids: &[Uuid],
) -> ApiResult<()> {
    for id in content_ids {
        let Some(row) = state.user_content.get(*id, tenant).await? else {
            continue;
        };
        if state.user_content.delete(*id, tenant).await? == 0 {
            continue;
        }
        if let Err(e) = state.user_content_store.delete(&row.storage_key).await {
            tracing::warn!(key = %row.storage_key, error = %e, "orphaned attachment: the row went but the object did not");
        }
    }
    Ok(())
}

/// The same, for one comment's files.
pub async fn purge_comment(state: &AppState, tenant: TenantId, comment: Uuid) -> ApiResult<()> {
    let ids: Vec<Uuid> = state
        .attachments
        .list(tenant, crate::repo::attachments::PARENT_COMMENT, comment)
        .await?
        .into_iter()
        .map(|a| a.user_content_id)
        .collect();
    purge_content(state, tenant, &ids).await
}

/// Fill in `attachment_count` for a batch of cards (AC-8).
///
/// Deliberately NOT part of `tasks::enrich`, which every task read goes
/// through: the count is for a board card, and adding a query to the shared
/// enrichment would put it on the agent-facing reads too — which is the surface
/// NG-1 keeps out of this ticket. One query per batch regardless of the number
/// of cards.
pub async fn fill_counts(
    state: &AppState,
    tenant: TenantId,
    tasks: &mut [nook_types::TaskItem],
) -> ApiResult<()> {
    if tasks.is_empty() {
        return Ok(());
    }
    let ids: Vec<Uuid> = tasks.iter().map(|t| t.id.0).collect();
    let counts = state.attachments.counts_for_tasks(tenant, &ids).await?;
    for t in tasks.iter_mut() {
        t.attachment_count = counts.get(&t.id.0).copied().unwrap_or(0);
    }
    Ok(())
}

/// One attachment, refused as 404 when this viewer may not see the ticket it
/// hangs on (MAIN-534).
///
/// The visibility check is the whole reason this is not a bare repository read:
/// an attachment id is a uuid somebody could have seen in a comment, and
/// answering with a private card's filename would leak the card's existence
/// through the one route that does not name it.
pub async fn readable(
    state: &AppState,
    tenant: TenantId,
    viewer: UserId,
    id: Uuid,
) -> ApiResult<TaskAttachment> {
    let row = state
        .attachments
        .get_record(tenant, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let task = parent_task(state, tenant, &row.parent_kind, row.parent_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    readable_task_id(state, tenant, viewer, task).await?;
    Ok(row)
}

/// A whole thread's attachments — the ticket's own and every comment's — for a
/// task named by key or uuid.
pub async fn list_thread_readable(
    state: &AppState,
    tenant: TenantId,
    viewer: UserId,
    ident: &str,
) -> ApiResult<Vec<TaskAttachment>> {
    let task = readable_task(state, tenant, viewer, ident).await?;
    state.attachments.list_thread(tenant, task).await
}

/// The task behind an identifier, refused as 404 when this viewer may not see
/// it (MAIN-76). Every attachment route resolves its parent through here, so
/// none of them can become a side channel onto a ticket nobody may open.
pub async fn readable_task(
    state: &AppState,
    tenant: TenantId,
    viewer: UserId,
    ident: &str,
) -> ApiResult<TaskId> {
    let id = crate::services::tasks::resolve_id(state.tasks.as_ref(), tenant, ident).await?;
    readable_task_id(state, tenant, viewer, id).await
}

async fn readable_task_id(
    state: &AppState,
    tenant: TenantId,
    viewer: UserId,
    id: TaskId,
) -> ApiResult<TaskId> {
    let row = state
        .tasks
        .get_row(tenant, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !crate::services::tasks::visible_to(&row, viewer) {
        return Err(ApiError::NotFound);
    }
    Ok(id)
}

/// Which ticket a parent belongs to. `None` when the parent has already gone —
/// which a removal treats as success and a read treats as a 404.
pub async fn parent_task(
    state: &AppState,
    tenant: TenantId,
    parent_kind: &str,
    parent_id: Uuid,
) -> ApiResult<Option<TaskId>> {
    if parent_kind == crate::repo::attachments::PARENT_TASK {
        return Ok(Some(TaskId(parent_id)));
    }
    Ok(state
        .tasks
        .comment_author(parent_id, tenant)
        .await?
        .map(|(_, task)| task))
}

/// One attachment as an agent reads it: the text, or the reason it is not here
/// and the command that gets it (AC-5).
pub async fn read_content(
    state: &AppState,
    tenant: TenantId,
    viewer: UserId,
    id: Uuid,
) -> ApiResult<AttachmentContent> {
    let row = readable(state, tenant, viewer, id).await?;
    let answer = |content: Option<String>, not_inlined: Option<String>| AttachmentContent {
        id: row.id,
        filename: row.filename.clone(),
        content_type: row.content_type.clone(),
        size_bytes: row.size_bytes,
        content,
        not_inlined,
    };

    if !is_inlineable_text(&row.content_type) {
        return Ok(answer(
            None,
            Some(fetch_hint(row.id, "it is not a text file")),
        ));
    }
    if row.size_bytes > MAX_INLINE_BYTES {
        return Ok(answer(
            None,
            Some(fetch_hint(
                row.id,
                &format!(
                    "it is {}, over the {} inline limit",
                    human_size(row.size_bytes),
                    human_size(MAX_INLINE_BYTES)
                ),
            )),
        ));
    }

    let stored = state
        .user_content
        .get(row.user_content_id, tenant)
        .await?
        .ok_or(ApiError::NotFound)?;
    let bytes = state
        .user_content_store
        .get(&stored.storage_key)
        .await
        .map_err(|e| {
            tracing::error!(key = %stored.storage_key, error = %e, "attachment row has no object behind it");
            ApiError::NotFound
        })?;

    // A type that claims to be text and is not is the uploader's mistake, not
    // this reader's: hand back the pointer rather than a string of replacement
    // characters that reads like a corrupted file.
    Ok(match String::from_utf8(bytes) {
        Ok(text) => answer(Some(text), None),
        Err(_) => answer(
            None,
            Some(fetch_hint(row.id, "its bytes are not valid UTF-8")),
        ),
    })
}

fn fetch_hint(id: Uuid, because: &str) -> String {
    format!("not inlined because {because} — fetch it with `nook attachments get {id}`")
}

/// Which content types are worth putting in a reply verbatim.
///
/// Deliberately a list and not a sniff: the question is whether a *reader* can
/// use the string, and the uploader's declared type is the only evidence
/// available before the bytes are fetched — which is the point, since fetching
/// them is what this is deciding whether to do. The structured-text suffixes
/// (`+json`, `+xml`) are what catch the long tail without naming every vendor
/// type.
pub fn is_inlineable_text(content_type: &str) -> bool {
    let base = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if base.starts_with("text/") {
        return true;
    }
    if base.ends_with("+json") || base.ends_with("+xml") || base.ends_with("+yaml") {
        return true;
    }
    matches!(
        base.as_str(),
        "application/json"
            | "application/xml"
            | "application/yaml"
            | "application/x-yaml"
            | "application/toml"
            | "application/x-toml"
            | "application/javascript"
            | "application/typescript"
            | "application/sql"
            | "application/x-sh"
            | "application/x-shellscript"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_readable_text_is_inlined() {
        for yes in [
            "text/markdown",
            "TEXT/PLAIN",
            "text/csv; charset=utf-8",
            "application/json",
            "application/vnd.api+json",
            "image/svg+xml",
            "application/x-yaml",
        ] {
            assert!(is_inlineable_text(yes), "{yes}");
        }
        for no in [
            "image/png",
            "application/pdf",
            "application/zip",
            "application/octet-stream",
            "",
            "video/mp4",
        ] {
            assert!(!is_inlineable_text(no), "{no}");
        }
    }
}
