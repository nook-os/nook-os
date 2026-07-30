//! Channel categories (MAIN-178): admin-defined groups that order channels in
//! the sidebar, scoped exactly like channels (`tenant`/`org`). Reads are visible
//! to any member (the sidebar renders the groups for everyone); every MUTATION is
//! admin-only. Deleting a category un-categorizes its channels (FK `ON DELETE SET
//! NULL`) — it never deletes a channel. DMs are never categorized (NG-3).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use nook_types::{ChatCategory, CreateChatCategory, ReorderChatCategories, UpdateChatCategory};
use uuid::Uuid;

use crate::{AppState, Caller, ChatError};

impl From<crate::repo::channels::CategoryRow> for ChatCategory {
    fn from(r: crate::repo::channels::CategoryRow) -> Self {
        ChatCategory {
            id: r.id,
            name: r.name,
            owner_type: r.owner_type,
            position: r.position,
            created_at: r.created_at,
        }
    }
}

/// The org a tenant belongs to (`tenants.org_id`).
async fn org_of(
    repo: &dyn crate::repo::channels::ChannelRepository,
    tenant: Uuid,
) -> Result<Uuid, ChatError> {
    repo.org_of_tenant(tenant)
        .await
        .map_err(|_| ChatError::Internal)?
        .ok_or(ChatError::Internal)
}

/// A scope's categories in display order — the shared read behind the list
/// endpoint and the reorder response.
async fn scoped(
    repo: &dyn crate::repo::channels::ChannelRepository,
    tenant: Uuid,
) -> Result<Vec<ChatCategory>, ChatError> {
    let rows = repo
        .categories(tenant)
        .await
        .map_err(|_| ChatError::Internal)?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// List the caller's categories in `position` order (AC-3). Member-visible.
pub async fn list(
    State(state): State<AppState>,
    caller: Caller,
) -> Result<Json<Vec<ChatCategory>>, ChatError> {
    Ok(Json(scoped(&*state.channels, caller.tenant_id).await?))
}

/// Create a category (admin-only, AC-2). It sorts last in its scope.
pub async fn create(
    State(state): State<AppState>,
    caller: Caller,
    Json(req): Json<CreateChatCategory>,
) -> Result<(StatusCode, Json<ChatCategory>), ChatError> {
    crate::require_admin(&state.db, &caller).await?;
    let name = req.name.trim();
    if name.is_empty() {
        return Err(ChatError::BadRequest("a category needs a name".into()));
    }
    let (owner_type, owner_id) = match req.owner.as_deref().unwrap_or("tenant") {
        "tenant" => ("tenant", caller.tenant_id),
        "org" => ("org", org_of(&*state.channels, caller.tenant_id).await?),
        other => {
            return Err(ChatError::BadRequest(format!(
                "category owner must be \"tenant\" or \"org\" (got {other:?})"
            )))
        }
    };
    // Sort last: position = how many categories the scope already has.
    let owner = crate::repo::channels::OwnerScope {
        owner_type,
        owner_id,
    };
    let count = state
        .channels
        .category_count(owner)
        .await
        .map_err(|_| ChatError::Internal)?;
    let row = state
        .channels
        .create_category(owner, name, count as i32)
        .await
        .map_err(|_| ChatError::Internal)?;
    Ok((StatusCode::CREATED, Json(row.into())))
}

/// Rename a category (admin-only, AC-2). Scoped: a category outside the caller's
/// tenant/org is `NotFound`, never renamed.
pub async fn update(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateChatCategory>,
) -> Result<Json<ChatCategory>, ChatError> {
    crate::require_admin(&state.db, &caller).await?;
    let name = req.name.trim();
    if name.is_empty() {
        return Err(ChatError::BadRequest(
            "a category name cannot be blank".into(),
        ));
    }
    let row = state
        .channels
        .rename_category(id, caller.tenant_id, name)
        .await
        .map_err(|_| ChatError::Internal)?
        .ok_or(ChatError::NotFound)?;
    Ok(Json(row.into()))
}

/// Delete a category (admin-only, AC-2/AC-3). Its channels are un-categorized by
/// the FK (`ON DELETE SET NULL`) — never deleted. Scoped like rename.
pub async fn delete(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ChatError> {
    crate::require_admin(&state.db, &caller).await?;
    let affected = state
        .channels
        .delete_category(id, caller.tenant_id)
        .await
        .map_err(|_| ChatError::Internal)?;
    if affected == 0 {
        return Err(ChatError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Reorder categories (admin-only, AC-2): each id's `position` becomes its index
/// in `ordered_ids`. Ids outside the caller's scope are ignored (the scoped
/// UPDATE simply matches nothing). Returns the reordered list.
pub async fn reorder(
    State(state): State<AppState>,
    caller: Caller,
    Json(req): Json<ReorderChatCategories>,
) -> Result<Json<Vec<ChatCategory>>, ChatError> {
    crate::require_admin(&state.db, &caller).await?;
    state
        .channels
        .reorder_categories(caller.tenant_id, &req.ordered_ids)
        .await
        .map_err(|_| ChatError::Internal)?;
    Ok(Json(scoped(&*state.channels, caller.tenant_id).await?))
}
