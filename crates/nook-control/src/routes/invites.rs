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
use crate::seed::hash_token;
use crate::services::identity::email_is_verified;
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

    // Only the hash is stored; the plaintext rides in the accept link (AC-9).
    let token = new_token();
    let mut invite: Invite = sqlx::query_as(
        "INSERT INTO invites (id, tenant_id, email, role, token_hash, status, invited_by, expires_at)
         VALUES ($1, $2, $3, $4, $5, 'pending', $6, now() + interval '14 days')
         RETURNING id, email, role, status, created_at, expires_at",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(tenant)
    .bind(email)
    .bind(role)
    .bind(hash_token(&token))
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

    // Look up by the token's hash — the plaintext is never at rest (AC-9).
    let invite: Option<(uuid::Uuid, TenantId, String, String, String)> = sqlx::query_as(
        "SELECT id, tenant_id, email, role, status FROM invites WHERE token_hash = $1",
    )
    .bind(hash_token(token))
    .fetch_optional(db)
    .await?;
    let Some((invite_id, tenant, invite_email, role, status)) = invite else {
        return decline("this invite link is not valid");
    };

    let email_matches = my_email.to_lowercase() == invite_email.to_lowercase();

    // Already a member (by person_id) → no-op success. Consume a still-pending
    // invite only when it was addressed to THIS person's email (AC-10) —
    // otherwise the invite belongs to someone else and must stay pending rather
    // than be burned by whoever happens to click the link.
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
        if status == "pending" && email_matches {
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
    if !email_matches {
        return decline("this invite was sent to a different email address");
    }

    // The email must be VERIFIED, not merely equal — email equality is the
    // MAIN-12 root cause. An unverified accepter is declined and the invite is
    // NOT consumed (AC-8), so it stays valid until they verify.
    if !email_is_verified(db, UserId(user_id)).await? {
        return decline("verify your email address first, then open the invite link again");
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
    use crate::seed::hash_token;
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
        // Stored hashed at rest (AC-9); the helper hands back the plaintext.
        sqlx::query(
            "INSERT INTO invites (id,tenant_id,email,role,token_hash,status,expires_at)
             VALUES ($1,$2,$3,'member',$4,'pending', now() + make_interval(days => $5::int))",
        )
        .bind(Uuid::new_v4())
        .bind(tenant)
        .bind(email)
        .bind(hash_token(&token))
        .bind(days as i32)
        .execute(db)
        .await
        .unwrap();
        token
    }

    /// Mark a user's email verified (a verified identity), so an accept can pass
    /// the AC-8 gate.
    async fn verify(db: &PgPool, user_id: Uuid, email: &str) {
        sqlx::query(
            "INSERT INTO identities (id,user_id,issuer,subject,email,raw_claims,email_verified_at)
             VALUES ($1,$2,'local',$3,$4,'{}'::jsonb, now())",
        )
        .bind(Uuid::now_v7())
        .bind(user_id)
        .bind(user_id.to_string())
        .bind(email)
        .execute(db)
        .await
        .unwrap();
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

        // Good invite, matching email, but the accepter is NOT verified →
        // declined and the invite is NOT consumed (AC-8).
        let good = invite(&db, shared, "invitee@i6.test", 7).await;
        let r_unverified = accept_core(&db, me, TenantId(home), &good).await.unwrap();
        let member_after_unverified = is_member(&db, shared, person).await;
        // The token is stored hashed, never in plaintext (AC-9).
        let (stored_hash,): (String,) = sqlx::query_as(
            "SELECT token_hash FROM invites WHERE tenant_id=$1 AND status='pending' AND lower(email)=lower($2)",
        )
        .bind(shared)
        .bind("invitee@i6.test")
        .fetch_one(&db)
        .await
        .unwrap();

        // Verify the address; the same link now works (AC-8).
        verify(&db, me, "invitee@i6.test").await;
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
            !r_unverified.accepted,
            "an unverified accepter is declined (AC-8)"
        );
        assert!(
            !member_after_unverified,
            "an unverified accept neither joins nor consumes the invite (AC-8)"
        );
        assert_eq!(
            stored_hash,
            hash_token(&good),
            "token stored hashed, not plaintext (AC-9)"
        );
        assert_ne!(
            stored_hash, good,
            "the plaintext token is never at rest (AC-9)"
        );
        assert!(
            r_ok.accepted && r_ok.tenant_id == TenantId(shared),
            "a verified accept lands in shared"
        );
        assert!(member_after, "membership created");
        assert!(
            r_again.accepted,
            "second accept is a no-op success (idempotent)"
        );
        assert_eq!(member_rows, 1, "no duplicate membership from re-accept");
    }

    /// AC-10: a member who presents an invite addressed to a DIFFERENT email
    /// gets the already-member no-op success, but the invite is NOT consumed —
    /// it stays pending for the person it was actually for.
    #[tokio::test]
    async fn already_member_does_not_burn_a_mismatched_email_invite() {
        let Some(db) = pool().await else { return };
        let shared = tenant(&db, "amshare").await;
        let home = tenant(&db, "amhome").await;
        let person = Uuid::new_v4();
        // `me` is already a member of `shared` (a users row + grant there).
        let me = user(&db, home, "me@i10.test", person).await;
        let mine_in_shared = user(&db, shared, "me@i10.test", person).await;
        sqlx::query(
            "INSERT INTO tenant_members (id,tenant_id,principal_type,principal_id,role)
             VALUES ($1,$2,'user',$3,'member')",
        )
        .bind(Uuid::new_v4())
        .bind(shared)
        .bind(mine_in_shared)
        .execute(&db)
        .await
        .unwrap();

        // An invite in `shared` for SOMEONE ELSE's email.
        let others = invite(&db, shared, "colleague@i10.test", 7).await;
        let r = accept_core(&db, me, TenantId(home), &others).await.unwrap();

        let (still_pending,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM invites WHERE tenant_id=$1 AND status='pending' AND lower(email)=lower($2)",
        )
        .bind(shared)
        .bind("colleague@i10.test")
        .fetch_one(&db)
        .await
        .unwrap();

        cleanup(&db, &[shared, home]).await;

        assert!(
            r.accepted,
            "already-a-member is still a no-op success (AC-6)"
        );
        assert_eq!(
            still_pending, 1,
            "a mismatched-email invite is NOT consumed by an existing member (AC-10)"
        );
    }

    /// AC-8: acceptance is gated on a VERIFIED email. An accepter whose email
    /// EQUALS the invite's but is not verified is declined, and the invite is
    /// NOT consumed — the same link works once the address is verified. Pinned
    /// on its own (the omnibus test also asserts it) so the gate is a clear,
    /// hard-to-weaken regression signal, mirroring the AC-10 test above.
    #[tokio::test]
    async fn an_unverified_accepter_is_declined_and_the_invite_survives() {
        let Some(db) = pool().await else { return };
        let shared = tenant(&db, "ac8share").await;
        let home = tenant(&db, "ac8home").await;
        let person = Uuid::new_v4();
        // The accepter's email EQUALS the invite email, but they are NOT
        // verified — no `verify(...)`, so no identity carries `email_verified_at`.
        let me = user(&db, home, "invitee@i8.test", person).await;

        // A pending, unexpired invite in `shared` for that exact email.
        let token = invite(&db, shared, "invitee@i8.test", 7).await;

        // Unverified accept → declined, and nothing joins.
        let declined = accept_core(&db, me, TenantId(home), &token).await.unwrap();
        let member_after_decline = is_member(&db, shared, person).await;
        let (still_pending,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM invites WHERE tenant_id=$1 AND status='pending' AND lower(email)=lower($2)",
        )
        .bind(shared)
        .bind("invitee@i8.test")
        .fetch_one(&db)
        .await
        .unwrap();

        // Verifying the address is the ONLY thing that was missing: the SAME
        // token now accepts — proving the decline was the AC-8 gate, not a
        // mismatch or an expiry.
        verify(&db, me, "invitee@i8.test").await;
        let accepted = accept_core(&db, me, TenantId(home), &token).await.unwrap();
        let member_after_verify = is_member(&db, shared, person).await;

        cleanup(&db, &[shared, home]).await;

        assert!(
            !declined.accepted,
            "an unverified accepter is declined (AC-8)"
        );
        assert_eq!(
            declined.tenant_id,
            TenantId(home),
            "the declined accepter stays in their own tenant"
        );
        assert!(
            declined
                .message
                .to_lowercase()
                .contains("verify your email"),
            "declined for the verification gate specifically, got: {:?}",
            declined.message
        );
        assert!(
            !member_after_decline,
            "no membership is created for an unverified accept (AC-8)"
        );
        assert_eq!(
            still_pending, 1,
            "the invite is NOT consumed — it stays pending until the email is verified (AC-8)"
        );
        assert!(
            accepted.accepted && accepted.tenant_id == TenantId(shared),
            "once verified, the SAME link accepts — the gate was the only blocker"
        );
        assert!(
            member_after_verify,
            "the verified accept creates the membership"
        );
    }
}
