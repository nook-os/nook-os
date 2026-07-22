//! Tenants the caller belongs to.
//!
//! Today this is a list of one: your own tenant, made when you first signed
//! in. It exists as an endpoint anyway because it is the seam teams grow from
//! — when a shared tenant can be joined, this is what the switcher reads, and
//! nothing else has to change to make that true.

use axum::extract::State;
use axum::Json;
use nook_types::*;

use crate::auth::AuthCtx;
use crate::error::ApiResult;
use crate::state::AppState;

/// Every tenant this user is a member of, with the role they hold in each.
#[utoipa::path(get, path = "/api/v1/tenants",
    operation_id = "list_tenants",
    responses((status = 200, body = [TenantMembership])))]
pub async fn list(
    State(state): State<AppState>,
    auth: AuthCtx,
) -> ApiResult<Json<Vec<TenantMembership>>> {
    // Read through the membership table rather than users.tenant_id: the
    // column is the *current* tenant, the table is everything reachable, and
    // conflating them is what makes adding teams a rewrite instead of a row.
    let rows: Vec<(
        TenantId,
        String,
        String,
        String,
        chrono::DateTime<chrono::Utc>,
    )> = sqlx::query_as(
        "SELECT t.id, t.name, t.slug, m.role, t.created_at
             FROM tenant_members m
             JOIN tenants t ON t.id = m.tenant_id
             WHERE m.principal_type = 'user' AND m.principal_id = $1
             ORDER BY t.created_at",
    )
    .bind(auth.user_id.0)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(
        rows.into_iter()
            .map(|(id, name, slug, role, created_at)| TenantMembership {
                current: id == auth.tenant_id,
                id,
                name,
                slug,
                role,
                created_at,
            })
            .collect(),
    ))
}
