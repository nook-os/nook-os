//! Channel categories (MAIN-178): admin-defined groups that order channels in
//! the sidebar, scoped exactly like channels (`tenant`/`org`). Reads are visible
//! to any member (the sidebar renders the groups for everyone); every MUTATION is
//! admin-only. Deleting a category un-categorizes its channels (FK `ON DELETE SET
//! NULL`) — it never deletes a channel. DMs are never categorized (NG-3).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use nook_db::{params, Db};
use nook_types::{ChatCategory, CreateChatCategory, ReorderChatCategories, UpdateChatCategory};
use uuid::Uuid;

use crate::{AppState, Caller, ChatError};

#[derive(sqlx::FromRow)]
struct CategoryRow {
    id: Uuid,
    name: String,
    owner_type: String,
    position: i32,
    created_at: DateTime<Utc>,
}

impl From<CategoryRow> for ChatCategory {
    fn from(r: CategoryRow) -> Self {
        ChatCategory {
            id: r.id,
            name: r.name,
            owner_type: r.owner_type,
            position: r.position,
            created_at: r.created_at,
        }
    }
}

const CATEGORY_COLS: &str = "id, name, owner_type, position, created_at";

/// The `WHERE` predicate that scopes a category to the caller: their tenant's
/// categories, plus their org's. `$N` is the tenant-id bind index. Mirrors the
/// channel-scope query, so a caller only ever touches categories they can see.
const SCOPE: &str = "((owner_type = 'tenant' AND owner_id = $N)
     OR (owner_type = 'org' AND owner_id = (SELECT org_id FROM public.tenants WHERE id = $N)))";

fn scope_with(bind: &str) -> String {
    SCOPE.replace("$N", bind)
}

/// The org a tenant belongs to (`tenants.org_id`).
async fn org_of(db: &nook_db::DbPool, tenant: Uuid) -> Result<Uuid, ChatError> {
    let org = db
        .query_scalar::<Uuid>(
            "SELECT org_id FROM public.tenants WHERE id = $1",
            params![tenant],
        )
        .await
        .map_err(|_| ChatError::Internal)?;
    Ok(org)
}

/// A scope's categories in display order — the shared read behind the list
/// endpoint and the reorder response.
async fn scoped(db: &nook_db::DbPool, tenant: Uuid) -> Result<Vec<ChatCategory>, ChatError> {
    let rows = db
        .query_all::<CategoryRow>(
            &format!(
                "SELECT {CATEGORY_COLS} FROM chat_channel_categories
         WHERE {scope} ORDER BY position, created_at",
                scope = scope_with("$1"),
            ),
            params![tenant],
        )
        .await
        .map_err(|_| ChatError::Internal)?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// List the caller's categories in `position` order (AC-3). Member-visible.
pub async fn list(
    State(state): State<AppState>,
    caller: Caller,
) -> Result<Json<Vec<ChatCategory>>, ChatError> {
    Ok(Json(scoped(&state.db, caller.tenant_id).await?))
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
        "org" => ("org", org_of(&state.db, caller.tenant_id).await?),
        other => {
            return Err(ChatError::BadRequest(format!(
                "category owner must be \"tenant\" or \"org\" (got {other:?})"
            )))
        }
    };
    // Sort last: position = how many categories the scope already has.
    let count = state
        .db
        .query_scalar::<i64>(
            "SELECT count(*) FROM chat_channel_categories WHERE owner_type = $1 AND owner_id = $2",
            params![owner_type, owner_id],
        )
        .await
        .map_err(|_| ChatError::Internal)?;
    let row = state
        .db
        .query_one::<CategoryRow>(
            &format!(
                "INSERT INTO chat_channel_categories (id, owner_type, owner_id, name, position)
         VALUES ($1, $2, $3, $4, $5) RETURNING {CATEGORY_COLS}"
            ),
            params![Uuid::now_v7(), owner_type, owner_id, name, count as i32],
        )
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
        .db
        .query_opt::<CategoryRow>(
            &format!(
                "UPDATE chat_channel_categories SET name = $2
         WHERE id = $1 AND {scope} RETURNING {CATEGORY_COLS}",
                scope = scope_with("$3"),
            ),
            params![id, name, caller.tenant_id],
        )
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
        .db
        .exec(
            &format!(
                "DELETE FROM chat_channel_categories WHERE id = $1 AND {scope}",
                scope = scope_with("$2"),
            ),
            params![id, caller.tenant_id],
        )
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
    for (i, id) in req.ordered_ids.iter().enumerate() {
        state
            .db
            .exec(
                &format!(
                    "UPDATE chat_channel_categories SET position = $2 WHERE id = $1 AND {scope}",
                    scope = scope_with("$3"),
                ),
                params![id, i as i32, caller.tenant_id],
            )
            .await
            .map_err(|_| ChatError::Internal)?;
    }
    Ok(Json(scoped(&state.db, caller.tenant_id).await?))
}
