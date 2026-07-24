//! Tenant invites (MAIN-6). An owner/admin invites an email into their active
//! tenant; the invitee accepts by signing in as that email and POSTing the
//! opaque token, which links them into the tenant via `person_id` (so the MAIN-4
//! switcher immediately offers it). Emailing the link is MAIN-7 — here it is
//! returned by the API and copied in the UI.

use axum::extract::{Path, State};
use axum::Json;
use nook_types::*;

use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Guard: the action targets the caller's ACTIVE tenant, and the caller is an
/// owner/admin of it. Managing another tenant needs switching to it first.
async fn require_admin_of(state: &AppState, auth: &AuthCtx, tenant: TenantId) -> ApiResult<()> {
    if tenant != auth.tenant_id {
        return Err(ApiError::ForbiddenMsg(
            "switch to a tenant before managing its invites".into(),
        ));
    }
    auth.require_tenant_admin(state).await
}

fn new_token() -> String {
    use rand::distr::Alphanumeric;
    use rand::Rng;
    let body: String = rand::rng()
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect();
    format!("inv_{body}")
}

fn validated_role(role: Option<&str>) -> ApiResult<&str> {
    match role.unwrap_or("member") {
        r @ ("member" | "admin") => Ok(r),
        // `owner` is never invitable (NG-3).
        other => Err(ApiError::BadRequest(format!(
            "invite role must be member or admin, not {other:?}"
        ))),
    }
}

/// `POST /api/v1/tenants/{id}/invites` — create a pending invite, returning the
/// accept URL. Re-inviting the same email replaces the existing pending invite
/// (AC-2). owner/admin only.
#[utoipa::path(post, path = "/api/v1/tenants/{id}/invites",
    operation_id = "create_invite",
    params(("id" = String, Path,)),
    request_body = CreateInviteRequest,
    responses((status = 200, body = Invite), (status = 403)))]
pub async fn create(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(tenant): Path<TenantId>,
    Json(req): Json<CreateInviteRequest>,
) -> ApiResult<Json<Invite>> {
    require_admin_of(&state, &auth, tenant).await?;
    let role = validated_role(req.role.as_deref())?;
    let email = req.email.trim();
    if email.is_empty() || !email.contains('@') {
        return Err(ApiError::BadRequest("a valid email is required".into()));
    }

    // Replace any existing pending invite for this email so re-inviting does not
    // stack (AC-2); the partial unique index also enforces one pending.
    sqlx::query(
        "DELETE FROM invites
         WHERE tenant_id = $1 AND status = 'pending' AND lower(email) = lower($2)",
    )
    .bind(tenant)
    .bind(email)
    .execute(&state.db)
    .await?;

    let token = new_token();
    let mut invite: Invite = sqlx::query_as(
        "INSERT INTO invites (id, tenant_id, email, role, token, status, invited_by, expires_at)
         VALUES ($1, $2, $3, $4, $5, 'pending', $6, now() + interval '14 days')
         RETURNING id, email, role, status, created_at, expires_at",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(tenant)
    .bind(email)
    .bind(role)
    .bind(&token)
    .bind(auth.user_id.0)
    .fetch_one(&state.db)
    .await?;

    // The accept link points at the web app, which drives sign-in then calls the
    // accept endpoint (MAIN-7 will also email this).
    invite.accept_url = Some(format!(
        "{}/accept?token={token}",
        state.cfg.web_origin.trim_end_matches('/')
    ));
    Ok(Json(invite))
}

/// `GET /api/v1/tenants/{id}/invites` — pending invites (never the token).
/// owner/admin only.
#[utoipa::path(get, path = "/api/v1/tenants/{id}/invites",
    operation_id = "list_invites",
    params(("id" = String, Path,)),
    responses((status = 200, body = [Invite]), (status = 403)))]
pub async fn list(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(tenant): Path<TenantId>,
) -> ApiResult<Json<Vec<Invite>>> {
    require_admin_of(&state, &auth, tenant).await?;
    let rows: Vec<Invite> = sqlx::query_as(
        "SELECT id, email, role, status, created_at, expires_at
         FROM invites WHERE tenant_id = $1 AND status = 'pending'
         ORDER BY created_at DESC",
    )
    .bind(tenant)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

/// `DELETE /api/v1/tenants/{id}/invites/{invite}` — revoke a pending invite; its
/// link stops working. owner/admin only.
#[utoipa::path(delete, path = "/api/v1/tenants/{id}/invites/{invite}",
    operation_id = "revoke_invite",
    params(("id" = String, Path,), ("invite" = String, Path,)),
    responses((status = 204), (status = 403), (status = 404)))]
pub async fn revoke(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path((tenant, invite)): Path<(TenantId, uuid::Uuid)>,
) -> ApiResult<axum::http::StatusCode> {
    require_admin_of(&state, &auth, tenant).await?;
    let res = sqlx::query(
        "UPDATE invites SET status = 'revoked'
         WHERE id = $1 AND tenant_id = $2 AND status = 'pending'",
    )
    .bind(invite)
    .bind(tenant)
    .execute(&state.db)
    .await?;
    if res.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// `POST /api/v1/invites/accept` — the signed-in person consumes a token. On a
/// match (pending, unexpired, email equals the signed-in email) they are added
/// to the tenant with the invite's role — linked by `person_id`, keeping their
/// personal tenant — and the invite becomes `accepted`. Any failure leaves the
/// invite untouched and returns the caller to their own tenant with a message
/// (AC-4/5); already-a-member is a no-op success (AC-6).
#[utoipa::path(post, path = "/api/v1/invites/accept",
    operation_id = "accept_invite",
    request_body = AcceptInviteRequest,
    responses((status = 200, body = AcceptInviteResult)))]
pub async fn accept(
    State(state): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<AcceptInviteRequest>,
) -> ApiResult<Json<AcceptInviteResult>> {
    auth.require_user()?;
    Ok(Json(
        accept_core(&state.db, auth.user_id.0, auth.tenant_id, &req.token).await?,
    ))
}

/// The accept logic, split from the handler so it can be tested against a real
/// database without an `AuthCtx`. `fallback_tenant` is where a declined/no-op
/// caller stays (their own active tenant).
pub async fn accept_core(
    db: &sqlx::PgPool,
    user_id: uuid::Uuid,
    fallback_tenant: TenantId,
    token: &str,
) -> ApiResult<AcceptInviteResult> {
    let decline = |msg: &str| {
        Ok(AcceptInviteResult {
            accepted: false,
            tenant_id: fallback_tenant,
            message: msg.to_string(),
        })
    };

    // Who is accepting — email, name, and the cross-tenant person key.
    let (my_email, my_name, my_person): (String, String, uuid::Uuid) =
        sqlx::query_as("SELECT email, display_name, person_id FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_one(db)
            .await?;

    let invite: Option<(uuid::Uuid, TenantId, String, String, String)> =
        sqlx::query_as("SELECT id, tenant_id, email, role, status FROM invites WHERE token = $1")
            .bind(token)
            .fetch_optional(db)
            .await?;
    let Some((invite_id, tenant, invite_email, role, status)) = invite else {
        return decline("this invite link is not valid");
    };

    // Already a member (by person_id) → no-op success, and consume a still-
    // pending invite so the link cannot be handed on (AC-6).
    let existing_member: Option<(UserId,)> = sqlx::query_as(
        "SELECT u.id FROM users u
         JOIN tenant_members m
           ON m.tenant_id = u.tenant_id AND m.principal_type = 'user' AND m.principal_id = u.id
         WHERE u.tenant_id = $1 AND u.person_id = $2
         LIMIT 1",
    )
    .bind(tenant)
    .bind(my_person)
    .fetch_optional(db)
    .await?;
    if existing_member.is_some() {
        if status == "pending" {
            let _ = sqlx::query("UPDATE invites SET status = 'accepted' WHERE id = $1")
                .bind(invite_id)
                .execute(db)
                .await;
        }
        return Ok(AcceptInviteResult {
            accepted: true,
            tenant_id: tenant,
            message: "you are already a member of this tenant".into(),
        });
    }

    // Consumable only when pending, unexpired, and to THIS person's email.
    if status != "pending" {
        return decline("this invite has already been used or revoked");
    }
    let (fresh,): (bool,) = sqlx::query_as("SELECT expires_at > now() FROM invites WHERE id = $1")
        .bind(invite_id)
        .fetch_one(db)
        .await?;
    if !fresh {
        return decline("this invite has expired");
    }
    if my_email.to_lowercase() != invite_email.to_lowercase() {
        return decline("this invite was sent to a different email address");
    }

    // Add the per-tenant user row carrying this person_id (or reuse one that
    // exists by email), then the membership grant, then consume the invite.
    let user_id: uuid::Uuid = match sqlx::query_as::<_, (uuid::Uuid,)>(
        "SELECT id FROM users WHERE tenant_id = $1 AND lower(email) = lower($2) LIMIT 1",
    )
    .bind(tenant)
    .bind(&invite_email)
    .fetch_optional(db)
    .await?
    {
        Some((id,)) => id,
        None => {
            let (id,): (uuid::Uuid,) = sqlx::query_as(
                "INSERT INTO users (id, tenant_id, display_name, email, role, person_id)
                 VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
            )
            .bind(uuid::Uuid::now_v7())
            .bind(tenant)
            .bind(&my_name)
            .bind(&invite_email)
            .bind(&role)
            .bind(my_person)
            .fetch_one(db)
            .await?;
            id
        }
    };
    sqlx::query(
        "INSERT INTO tenant_members (id, tenant_id, principal_type, principal_id, role)
         VALUES ($1, $2, 'user', $3, $4)
         ON CONFLICT (tenant_id, principal_type, principal_id) DO NOTHING",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(tenant)
    .bind(user_id)
    .bind(&role)
    .execute(db)
    .await?;
    sqlx::query("UPDATE invites SET status = 'accepted' WHERE id = $1")
        .bind(invite_id)
        .execute(db)
        .await?;

    Ok(AcceptInviteResult {
        accepted: true,
        tenant_id: tenant,
        message: "welcome — you have joined the tenant".into(),
    })
}

#[cfg(test)]
mod tests {
    /// Source guards: create/list/revoke are admin-gated; owner is not invitable.
    fn body(name: &str) -> &'static str {
        include_str!("invites.rs")
            .split(&format!("pub async fn {name}("))
            .nth(1)
            .expect("fn")
            .split("\npub async fn ")
            .next()
            .expect("body")
    }
    #[test]
    fn management_is_admin_gated_and_owner_is_not_invitable() {
        for f in ["create", "list", "revoke"] {
            assert!(
                body(f).contains("require_admin_of"),
                "{f} must be admin-gated"
            );
        }
        assert!(
            super::validated_role(Some("owner")).is_err(),
            "owner not invitable (NG-3)"
        );
        assert!(super::validated_role(Some("admin")).is_ok());
        assert!(super::validated_role(None).is_ok(), "defaults to member");
    }
}

#[cfg(test)]
mod db_tests {
    use super::accept_core;
    use nook_types::TenantId;
    use sqlx::postgres::PgPoolOptions;
    use sqlx::PgPool;
    use uuid::Uuid;

    async fn pool() -> Option<PgPool> {
        if std::env::var("NOOK_REQUIRE_DB").ok().as_deref() != Some("1") {
            return None;
        }
        let db = PgPoolOptions::new()
            .max_connections(2)
            .connect(&std::env::var("DATABASE_URL").ok()?)
            .await
            .ok()?;
        crate::MIGRATOR.run(&db).await.ok()?;
        Some(db)
    }
    async fn tenant(db: &PgPool, name: &str) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO tenants (id, name, slug) VALUES ($1,$2,$3)")
            .bind(id)
            .bind(name)
            .bind(format!("{name}-{id}"))
            .execute(db)
            .await
            .unwrap();
        id
    }
    /// A users row (a person, by person_id) in a tenant.
    async fn user(db: &PgPool, tenant: Uuid, email: &str, person: Uuid) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO users (id,tenant_id,display_name,email,role,person_id) VALUES ($1,$2,'P',$3,'owner',$4)")
            .bind(id).bind(tenant).bind(email).bind(person).execute(db).await.unwrap();
        id
    }
    async fn invite(db: &PgPool, tenant: Uuid, email: &str, days: i64) -> String {
        let token = format!("inv_{}", Uuid::new_v4().simple());
        sqlx::query(
            "INSERT INTO invites (id,tenant_id,email,role,token,status,expires_at)
             VALUES ($1,$2,$3,'member',$4,'pending', now() + make_interval(days => $5::int))",
        )
        .bind(Uuid::new_v4())
        .bind(tenant)
        .bind(email)
        .bind(&token)
        .bind(days as i32)
        .execute(db)
        .await
        .unwrap();
        token
    }
    async fn is_member(db: &PgPool, tenant: Uuid, person: Uuid) -> bool {
        let (n,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM users u JOIN tenant_members m
               ON m.tenant_id=u.tenant_id AND m.principal_type='user' AND m.principal_id=u.id
             WHERE u.tenant_id=$1 AND u.person_id=$2",
        )
        .bind(tenant)
        .bind(person)
        .fetch_one(db)
        .await
        .unwrap();
        n > 0
    }
    async fn cleanup(db: &PgPool, tenants: &[Uuid]) {
        for t in tenants {
            for tbl in ["invites", "tenant_members", "users"] {
                let _ = sqlx::query(&format!("DELETE FROM {tbl} WHERE tenant_id=$1"))
                    .bind(t)
                    .execute(db)
                    .await;
            }
            let _ = sqlx::query("DELETE FROM tenants WHERE id=$1")
                .bind(t)
                .execute(db)
                .await;
        }
    }

    #[tokio::test]
    async fn accept_consumes_only_on_match_and_is_idempotent() {
        let Some(db) = pool().await else { return };
        let shared = tenant(&db, "shared").await;
        let home = tenant(&db, "home").await;
        // Separate tenant for the expired case: the good invite reuses this
        // email in `shared`, and one-pending-per-email forbids two there.
        let stale = tenant(&db, "stale").await;
        let person = Uuid::new_v4();
        let me = user(&db, home, "invitee@i6.test", person).await;

        // Wrong email invite → declined, no membership.
        let wrong = invite(&db, shared, "someone-else@i6.test", 7).await;
        let r_wrong = accept_core(&db, me, TenantId(home), &wrong).await.unwrap();

        // Expired invite (own tenant) → declined.
        let expired = invite(&db, stale, "invitee@i6.test", -1).await;
        let r_expired = accept_core(&db, me, TenantId(home), &expired)
            .await
            .unwrap();

        // Unknown token → declined.
        let r_unknown = accept_core(&db, me, TenantId(home), "inv_nope")
            .await
            .unwrap();
        let member_before = is_member(&db, shared, person).await;

        // Good invite → accepted, membership created, lands in shared.
        let good = invite(&db, shared, "invitee@i6.test", 7).await;
        let r_ok = accept_core(&db, me, TenantId(home), &good).await.unwrap();
        let member_after = is_member(&db, shared, person).await;
        // Idempotent: second accept is a no-op success; token can't be reused for a NEW membership.
        let r_again = accept_core(&db, me, TenantId(home), &good).await.unwrap();
        let (member_rows,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM tenant_members m JOIN users u ON u.id=m.principal_id
             WHERE u.tenant_id=$1 AND u.person_id=$2",
        )
        .bind(shared)
        .bind(person)
        .fetch_one(&db)
        .await
        .unwrap();

        cleanup(&db, &[shared, home, stale]).await;

        assert!(
            !r_wrong.accepted && r_wrong.tenant_id == TenantId(home),
            "email mismatch declined"
        );
        assert!(!r_expired.accepted, "expired declined");
        assert!(!r_unknown.accepted, "unknown token declined");
        assert!(!member_before, "no membership before a valid accept");
        assert!(
            r_ok.accepted && r_ok.tenant_id == TenantId(shared),
            "valid accept lands in shared"
        );
        assert!(member_after, "membership created");
        assert!(
            r_again.accepted,
            "second accept is a no-op success (idempotent)"
        );
        assert_eq!(member_rows, 1, "no duplicate membership from re-accept");
    }
}
