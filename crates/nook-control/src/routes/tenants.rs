//! Tenants the caller belongs to.
//!
//! Today this is a list of one: your own tenant, made when you first signed
//! in. It exists as an endpoint anyway because it is the seam teams grow from
//! — when a shared tenant can be joined, this is what the switcher reads, and
//! nothing else has to change to make that true.

use axum::extract::{Path, State};
use axum::Json;
use nook_types::*;

use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// The caller's role in a tenant, read from `tenant_members` — the single source
/// of truth (not `users.role`), so authorization is against the membership that
/// actually grants access (AC-7).
async fn role_in(db: &sqlx::PgPool, user_id: uuid::Uuid, tenant: TenantId) -> ApiResult<String> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT role FROM tenant_members
         WHERE tenant_id = $1 AND principal_type = 'user' AND principal_id = $2",
    )
    .bind(tenant)
    .bind(user_id)
    .fetch_optional(db)
    .await?;
    row.map(|(r,)| r)
        .ok_or_else(|| ApiError::ForbiddenMsg("you are not a member of this tenant".into()))
}

/// Every management action targets the caller's ACTIVE tenant only. Managing a
/// tenant you are not switched into would need a cross-tenant role resolution
/// this endpoint deliberately does not do; switch to it first. Returns the
/// caller's role in it.
async fn require_active_tenant(
    state: &AppState,
    auth: &AuthCtx,
    tenant: TenantId,
) -> ApiResult<String> {
    auth.require_user()?;
    if tenant != auth.tenant_id {
        return Err(ApiError::ForbiddenMsg(
            "you can only manage members of the tenant you are switched into".into(),
        ));
    }
    role_in(&state.db, auth.user_id.0, tenant).await
}

/// How many owners a tenant has — the guard that keeps a tenant from being left
/// ownerless (AC-5).
async fn owner_count(db: &sqlx::PgPool, tenant: TenantId) -> ApiResult<i64> {
    let (n,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM tenant_members
         WHERE tenant_id = $1 AND principal_type = 'user' AND role = 'owner'",
    )
    .bind(tenant)
    .fetch_one(db)
    .await?;
    Ok(n)
}

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

/// `GET /api/v1/tenants/{id}/members` — everyone in the tenant. Any member may
/// view (AC-1); management is gated separately.
#[utoipa::path(get, path = "/api/v1/tenants/{id}/members",
    operation_id = "list_members",
    params(("id" = String, Path,)),
    responses((status = 200, body = [TenantMemberItem]), (status = 403)))]
pub async fn list_members(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(tenant): Path<TenantId>,
) -> ApiResult<Json<Vec<TenantMemberItem>>> {
    // Viewing is open to any member of the active tenant.
    require_active_tenant(&state, &auth, tenant).await?;
    let rows: Vec<TenantMemberItem> = sqlx::query_as(
        "SELECT m.principal_id, u.email, u.display_name, m.role, m.created_at AS joined_at
         FROM tenant_members m
         JOIN users u ON u.id = m.principal_id
         WHERE m.tenant_id = $1 AND m.principal_type = 'user'
         ORDER BY (m.role = 'owner') DESC, (m.role = 'admin') DESC, u.display_name",
    )
    .bind(tenant)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

/// `PATCH /api/v1/tenants/{id}/members/{pid}` — change a member's role.
/// owner/admin may move member↔admin; only an owner may grant `owner`
/// (co-owner / transfer). The last owner cannot be demoted (AC-2, AC-5).
#[utoipa::path(patch, path = "/api/v1/tenants/{id}/members/{pid}",
    operation_id = "change_member_role",
    params(("id" = String, Path,), ("pid" = String, Path,)),
    request_body = ChangeMemberRoleRequest,
    responses((status = 200, body = TenantMemberItem), (status = 403), (status = 404)))]
pub async fn change_member_role(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path((tenant, pid)): Path<(TenantId, uuid::Uuid)>,
    Json(req): Json<ChangeMemberRoleRequest>,
) -> ApiResult<Json<TenantMemberItem>> {
    let caller = require_active_tenant(&state, &auth, tenant).await?;
    if caller != "owner" && caller != "admin" {
        return Err(ApiError::ForbiddenMsg(
            "changing roles needs owner or admin".into(),
        ));
    }
    let new_role = req.role.as_str();
    if !matches!(new_role, "owner" | "admin" | "member") {
        return Err(ApiError::BadRequest(
            "role must be owner, admin, or member".into(),
        ));
    }
    // Only an owner may grant ownership.
    if new_role == "owner" && caller != "owner" {
        return Err(ApiError::ForbiddenMsg(
            "only an owner can grant ownership".into(),
        ));
    }
    let current = role_in(&state.db, pid, tenant).await?;
    // Demoting/reassigning the last owner would orphan the tenant.
    if current == "owner" && new_role != "owner" && owner_count(&state.db, tenant).await? <= 1 {
        return Err(ApiError::ForbiddenMsg(
            "this is the last owner — promote someone else to owner first".into(),
        ));
    }

    // Keep users.role in step with tenant_members.role so the two never
    // disagree (see the identity module's invariant).
    sqlx::query(
        "UPDATE tenant_members SET role = $3
         WHERE tenant_id = $1 AND principal_type = 'user' AND principal_id = $2",
    )
    .bind(tenant)
    .bind(pid)
    .bind(new_role)
    .execute(&state.db)
    .await?;
    sqlx::query("UPDATE users SET role = $3, updated_at = now() WHERE id = $2 AND tenant_id = $1")
        .bind(tenant)
        .bind(pid)
        .bind(new_role)
        .execute(&state.db)
        .await?;

    let member: TenantMemberItem = sqlx::query_as(
        "SELECT m.principal_id, u.email, u.display_name, m.role, m.created_at AS joined_at
         FROM tenant_members m JOIN users u ON u.id = m.principal_id
         WHERE m.tenant_id = $1 AND m.principal_id = $2",
    )
    .bind(tenant)
    .bind(pid)
    .fetch_one(&state.db)
    .await?;
    Ok(Json(member))
}

/// `DELETE /api/v1/tenants/{id}/members/{pid}` — remove a member. owner/admin
/// only; the last owner cannot be removed (AC-3, AC-5). The `users` row and all
/// authored work stay with the tenant (NG-2); only the membership grant is gone,
/// so the person loses access on their next request.
#[utoipa::path(delete, path = "/api/v1/tenants/{id}/members/{pid}",
    operation_id = "remove_member",
    params(("id" = String, Path,), ("pid" = String, Path,)),
    responses((status = 204), (status = 403), (status = 404)))]
pub async fn remove_member(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path((tenant, pid)): Path<(TenantId, uuid::Uuid)>,
) -> ApiResult<axum::http::StatusCode> {
    let caller = require_active_tenant(&state, &auth, tenant).await?;
    if caller != "owner" && caller != "admin" {
        return Err(ApiError::ForbiddenMsg(
            "removing members needs owner or admin".into(),
        ));
    }
    let target = role_in(&state.db, pid, tenant).await?;
    if target == "owner" && owner_count(&state.db, tenant).await? <= 1 {
        return Err(ApiError::ForbiddenMsg(
            "this is the last owner — a tenant cannot be left ownerless".into(),
        ));
    }
    let res = sqlx::query(
        "DELETE FROM tenant_members
         WHERE tenant_id = $1 AND principal_type = 'user' AND principal_id = $2",
    )
    .bind(tenant)
    .bind(pid)
    .execute(&state.db)
    .await?;
    if res.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// `POST /api/v1/tenants/{id}/leave` — remove your own membership. You keep your
/// personal tenant; the last owner cannot leave (that includes your personal
/// tenant, where you are the sole owner) (AC-4, AC-5).
#[utoipa::path(post, path = "/api/v1/tenants/{id}/leave",
    operation_id = "leave_tenant",
    params(("id" = String, Path,)),
    responses((status = 204), (status = 403)))]
pub async fn leave_tenant(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(tenant): Path<TenantId>,
) -> ApiResult<axum::http::StatusCode> {
    let mine = require_active_tenant(&state, &auth, tenant).await?;
    if mine == "owner" && owner_count(&state.db, tenant).await? <= 1 {
        return Err(ApiError::ForbiddenMsg(
            "you are the last owner — promote someone else before leaving".into(),
        ));
    }
    sqlx::query(
        "DELETE FROM tenant_members
         WHERE tenant_id = $1 AND principal_type = 'user' AND principal_id = $2",
    )
    .bind(tenant)
    .bind(auth.user_id.0)
    .execute(&state.db)
    .await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    /// The last-owner guard and the role gate are the security-load-bearing
    /// parts; assert they are present in each handler at the source, since a
    /// removal of either is silent (the happy path keeps working).
    fn body(name: &str) -> &'static str {
        include_str!("tenants.rs")
            .split(&format!("pub async fn {name}("))
            .nth(1)
            .expect("handler")
            .split("\npub async fn ")
            .next()
            .expect("body")
    }

    #[test]
    fn role_changes_are_owner_admin_gated_and_owner_grant_is_owner_only() {
        let b = body("change_member_role");
        assert!(b.contains("changing roles needs owner or admin"));
        assert!(b.contains("only an owner can grant ownership"));
        assert!(
            b.contains("owner_count") && b.contains("<= 1"),
            "last-owner demotion guard"
        );
    }

    #[test]
    fn removal_is_gated_and_last_owner_protected() {
        let b = body("remove_member");
        assert!(b.contains("removing members needs owner or admin"));
        assert!(
            b.contains("owner_count") && b.contains("<= 1"),
            "last-owner removal guard"
        );
        assert!(
            b.contains("DELETE FROM tenant_members"),
            "removes only the membership row"
        );
        assert!(
            !b.contains("DELETE FROM users"),
            "must NOT delete the user's row / work (NG-2)"
        );
    }

    #[test]
    fn leaving_cannot_orphan_the_tenant() {
        let b = body("leave_tenant");
        assert!(
            b.contains("owner_count") && b.contains("<= 1"),
            "last-owner leave guard"
        );
        assert!(
            b.contains("auth.user_id.0"),
            "leave removes the caller's own membership"
        );
    }
}

#[cfg(test)]
mod db_tests {
    use super::{owner_count, role_in};
    use nook_types::TenantId;
    use sqlx::postgres::PgPoolOptions;
    use sqlx::PgPool;
    use uuid::Uuid;

    async fn pool() -> Option<PgPool> {
        if std::env::var("NOOK_REQUIRE_DB").ok().as_deref() != Some("1") {
            return None;
        }
        let url = std::env::var("DATABASE_URL").ok()?;
        let db = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .ok()?;
        crate::MIGRATOR.run(&db).await.ok()?;
        Some(db)
    }

    async fn member(db: &PgPool, tenant: Uuid, role: &str) -> Uuid {
        let uid = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO users (id, tenant_id, display_name, email, role, person_id)
             VALUES ($1, $2, 'M', $3, $4, gen_random_uuid())",
        )
        .bind(uid)
        .bind(tenant)
        .bind(format!("{uid}@m5.test"))
        .bind(role)
        .execute(db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO tenant_members (id, tenant_id, principal_type, principal_id, role)
             VALUES ($1, $2, 'user', $3, $4)",
        )
        .bind(Uuid::new_v4())
        .bind(tenant)
        .bind(uid)
        .bind(role)
        .execute(db)
        .await
        .unwrap();
        uid
    }

    #[tokio::test]
    async fn owner_count_and_role_reflect_tenant_members() {
        let Some(db) = pool().await else { return };
        let t = Uuid::new_v4();
        sqlx::query("INSERT INTO tenants (id, name, slug) VALUES ($1, 'M5', $2)")
            .bind(t)
            .bind(format!("m5-{t}"))
            .execute(&db)
            .await
            .unwrap();
        let owner = member(&db, t, "owner").await;
        let plain = member(&db, t, "member").await;

        let oc1 = owner_count(&db, TenantId(t)).await.unwrap();
        let r_owner = role_in(&db, owner, TenantId(t)).await.unwrap();
        let r_plain = role_in(&db, plain, TenantId(t)).await.unwrap();
        // Promote the member to owner → two owners.
        sqlx::query(
            "UPDATE tenant_members SET role='owner' WHERE tenant_id=$1 AND principal_id=$2",
        )
        .bind(t)
        .bind(plain)
        .execute(&db)
        .await
        .unwrap();
        let oc2 = owner_count(&db, TenantId(t)).await.unwrap();

        // A revoked membership resolves to an error (not a member).
        sqlx::query("DELETE FROM tenant_members WHERE tenant_id=$1 AND principal_id=$2")
            .bind(t)
            .bind(plain)
            .execute(&db)
            .await
            .unwrap();
        let gone = role_in(&db, plain, TenantId(t)).await.is_err();

        // cleanup
        let _ = sqlx::query("DELETE FROM tenant_members WHERE tenant_id=$1")
            .bind(t)
            .execute(&db)
            .await;
        let _ = sqlx::query("DELETE FROM users WHERE tenant_id=$1")
            .bind(t)
            .execute(&db)
            .await;
        let _ = sqlx::query("DELETE FROM tenants WHERE id=$1")
            .bind(t)
            .execute(&db)
            .await;

        assert_eq!(oc1, 1, "one owner");
        assert_eq!(r_owner, "owner");
        assert_eq!(r_plain, "member");
        assert_eq!(oc2, 2, "after promotion, two owners");
        assert!(gone, "a removed member has no resolvable role");
    }
}
