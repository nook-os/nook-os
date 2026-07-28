//! Channel categories (MAIN-178): admin-defined groups that order channels in
//! the sidebar, scoped exactly like channels (`tenant`/`org`). Reads are visible
//! to any member (the sidebar renders the groups for everyone); every MUTATION is
//! admin-only. Deleting a category un-categorizes its channels (FK `ON DELETE SET
//! NULL`) — it never deletes a channel. DMs are never categorized (NG-3).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
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
    let (org,): (Uuid,) = sqlx::query_as("SELECT org_id FROM public.tenants WHERE id = $1")
        .bind(tenant)
        .fetch_one(db)
        .await
        .map_err(|_| ChatError::Internal)?;
    Ok(org)
}

/// A scope's categories in display order — the shared read behind the list
/// endpoint and the reorder response.
async fn scoped(db: &nook_db::DbPool, tenant: Uuid) -> Result<Vec<ChatCategory>, ChatError> {
    let rows = sqlx::query_as::<_, CategoryRow>(&format!(
        "SELECT {CATEGORY_COLS} FROM chat_channel_categories
         WHERE {scope} ORDER BY position, created_at",
        scope = scope_with("$1"),
    ))
    .bind(tenant)
    .fetch_all(db)
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
    let (count,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM chat_channel_categories WHERE owner_type = $1 AND owner_id = $2",
    )
    .bind(owner_type)
    .bind(owner_id)
    .fetch_one(&state.db)
    .await
    .map_err(|_| ChatError::Internal)?;
    let row = sqlx::query_as::<_, CategoryRow>(&format!(
        "INSERT INTO chat_channel_categories (id, owner_type, owner_id, name, position)
         VALUES ($1, $2, $3, $4, $5) RETURNING {CATEGORY_COLS}"
    ))
    .bind(Uuid::now_v7())
    .bind(owner_type)
    .bind(owner_id)
    .bind(name)
    .bind(count as i32)
    .fetch_one(&state.db)
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
    let row = sqlx::query_as::<_, CategoryRow>(&format!(
        "UPDATE chat_channel_categories SET name = $2
         WHERE id = $1 AND {scope} RETURNING {CATEGORY_COLS}",
        scope = scope_with("$3"),
    ))
    .bind(id)
    .bind(name)
    .bind(caller.tenant_id)
    .fetch_optional(&state.db)
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
    let res = sqlx::query(&format!(
        "DELETE FROM chat_channel_categories WHERE id = $1 AND {scope}",
        scope = scope_with("$2"),
    ))
    .bind(id)
    .bind(caller.tenant_id)
    .execute(&state.db)
    .await
    .map_err(|_| ChatError::Internal)?;
    if res.rows_affected() == 0 {
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
        sqlx::query(&format!(
            "UPDATE chat_channel_categories SET position = $2 WHERE id = $1 AND {scope}",
            scope = scope_with("$3"),
        ))
        .bind(id)
        .bind(i as i32)
        .bind(caller.tenant_id)
        .execute(&state.db)
        .await
        .map_err(|_| ChatError::Internal)?;
    }
    Ok(Json(scoped(&state.db, caller.tenant_id).await?))
}
