//! What attachments need from more than one route (MAIN-533).
//!
//! Removal is here because three routes end in the same three steps — drop the
//! join row, drop the content row, drop the bytes — and only the first is a
//! delete a database could have performed. The bytes live in an object store no
//! cascade reaches, which is why AC-7's "deleting a ticket takes its
//! attachments with it" is code rather than a foreign key in the migration.

use uuid::Uuid;

use crate::error::ApiResult;
use crate::state::AppState;
use nook_types::TenantId;

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
        if let Err(e) = state.artifacts.delete(&row.storage_key).await {
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
