//! Identity upsert + tenant bootstrap.
//!
//! Policy: **a new person gets their own tenant.** Signing in provisions a
//! tenant named after them, owns it, and everything they then create — nodes,
//! workspaces, sessions, secrets — is scoped to it. Tenant is the unit of
//! isolation, so this is what stops two people on one instance from seeing
//! each other's machines.
//!
//! There is no opt-out. A flag that dropped every new identity into the oldest
//! tenant used to exist; it made "everyone can see everyone's machines" a
//! single environment variable away, and an instance misconfigured that way is
//! indistinguishable from a leak. Sharing belongs to `tenant_members` — an
//! explicit grant per person — not to a global switch.
//!
//! Membership is a table, not a column: `users.tenant_id` is the personal
//! tenant, and `tenant_members` is what will let someone belong to a shared
//! team tenant as well. Both are written here so the two never disagree.

use chrono::{DateTime, Utc};
use nook_types::{IdentityId, Tenant, TenantId, TenantMembership, User, UserId};
use serde_json::Value;
use sqlx::PgPool;

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Every tenant this person belongs to, resolved from `tenant_members`.
///
/// A person is one `users` row PER tenant, and the rows are tied together by
/// `person_id` — a platform-issued value (`0002_add_person_id`), NOT the email
/// string. Email was the join key once, but it is unverified: anyone who could
/// create a `users` row carrying a victim's email string reached the victim's
/// tenants (MAIN-12). So "which tenants can this user reach" is: find every
/// `users` row sharing this user's `person_id`, keep the ones with a live
/// `tenant_members` grant, and return their tenants. `tenant_members` is the
/// single source of truth (AC-7): a membership row that is gone drops the
/// tenant from this list.
///
/// `active` is the tenant the session is scoped to right now, marked `current`.
pub async fn memberships_for(
    db: &PgPool,
    user_id: UserId,
    active: TenantId,
) -> ApiResult<Vec<TenantMembership>> {
    let rows: Vec<(TenantId, String, String, String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT t.id, t.name, t.slug, tm.role, t.created_at
         FROM users me
         JOIN users u ON u.person_id = me.person_id
         JOIN tenant_members tm
             ON tm.tenant_id = u.tenant_id
            AND tm.principal_type = 'user'
            AND tm.principal_id = u.id
         JOIN tenants t ON t.id = u.tenant_id
         WHERE me.id = $1
         ORDER BY t.created_at",
    )
    .bind(user_id)
    .fetch_all(db)
    .await?;

    Ok(to_memberships(rows, active))
}

/// Mark which tenant is active and shape the rows into `TenantMembership`s.
/// Pure, so the "current" flag and the passthrough are testable without a DB.
fn to_memberships(
    rows: Vec<(TenantId, String, String, String, DateTime<Utc>)>,
    active: TenantId,
) -> Vec<TenantMembership> {
    rows.into_iter()
        .map(|(id, name, slug, role, created_at)| TenantMembership {
            current: id == active,
            id,
            name,
            slug,
            role,
            created_at,
        })
        .collect()
}

/// Resolve the per-tenant `users` row for this person in `target`, but only if
/// they actually belong there. Returns `None` when there is no membership — the
/// caller turns that into a 403. This is the guard behind tenant switching: the
/// active tenant can only become one the person is a `tenant_members` of.
/// Correlated by `person_id`, never email (MAIN-12), so a matching email string
/// in another tenant cannot be leveraged into a switch.
pub async fn member_user_in_tenant(
    db: &PgPool,
    user_id: UserId,
    target: TenantId,
) -> ApiResult<Option<UserId>> {
    let row: Option<(UserId,)> = sqlx::query_as(
        "SELECT u.id
         FROM users me
         JOIN users u ON u.person_id = me.person_id
         JOIN tenant_members tm
             ON tm.tenant_id = u.tenant_id
            AND tm.principal_type = 'user'
            AND tm.principal_id = u.id
         WHERE me.id = $1 AND u.tenant_id = $2
         LIMIT 1",
    )
    .bind(user_id)
    .bind(target)
    .fetch_optional(db)
    .await?;
    Ok(row.map(|(id,)| id))
}

/// Does this user still have a live `tenant_members` grant in `tenant`?
///
/// `tenant_members` is the single source of truth for the LIFE of a session,
/// not only at switch time (AC-7). `AuthCtx` calls this on every cookie-session
/// request so that revoking a grant takes effect immediately — the session
/// loses access on its next request rather than lingering until logout. One
/// indexed lookup; the personal-tenant grant every user has (written in
/// `login_identity`) means a legitimate session always passes.
pub async fn active_membership_exists(
    db: &PgPool,
    user_id: UserId,
    tenant: TenantId,
) -> ApiResult<bool> {
    let row: Option<(i32,)> = sqlx::query_as(
        "SELECT 1 FROM tenant_members
         WHERE tenant_id = $1 AND principal_type = 'user' AND principal_id = $2
         LIMIT 1",
    )
    .bind(tenant)
    .bind(user_id)
    .fetch_optional(db)
    .await?;
    Ok(row.is_some())
}

pub struct IdentityClaims {
    pub issuer: String,
    pub subject: String,
    pub email: Option<String>,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    pub raw_claims: Value,
}

pub async fn login_identity(state: &AppState, claims: IdentityClaims) -> ApiResult<(User, Tenant)> {
    // Existing identity → existing user.
    let existing: Option<(UserId,)> =
        sqlx::query_as("SELECT user_id FROM identities WHERE issuer = $1 AND subject = $2")
            .bind(&claims.issuer)
            .bind(&claims.subject)
            .fetch_optional(&state.db)
            .await?;

    if let Some((user_id,)) = existing {
        let user: User = sqlx::query_as("SELECT * FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_one(&state.db)
            .await?;
        let tenant: Tenant = sqlx::query_as("SELECT * FROM tenants WHERE id = $1")
            .bind(user.tenant_id)
            .fetch_one(&state.db)
            .await?;
        // The lock has to bind both directions, or it is not a lock: a tenant
        // running local accounts must not silently acquire OIDC identities
        // beside them, which is exactly the duplicate-person problem the mode
        // exists to prevent.
        crate::services::local_auth::claim_mode(
            &state.db,
            tenant.id,
            crate::services::local_auth::AuthMode::Oidc,
        )
        .await?;
        return Ok((user, tenant));
    }

    let email = claims
        .email
        .clone()
        .unwrap_or_else(|| format!("{}@unknown.invalid", claims.subject));
    let display_name = claims
        .display_name
        .clone()
        .unwrap_or_else(|| email.split('@').next().unwrap_or("user").to_string());

    // Count USERS, not identities.
    //
    // This asked `SELECT count(*) FROM identities`, which is zero on an
    // instance bootstrapped with a LOCAL account — local sign-in creates a user
    // with no `identities` row. The first person to sign in with OIDC therefore
    // looked like the first person ever, adopted the existing default tenant,
    // and was made its OWNER: full access to somebody else's nodes, workspaces
    // and secrets. "Is this instance empty?" is a question about people, and
    // there is only one table that knows.
    let (user_count,): (i64,) = sqlx::query_as("SELECT count(*) FROM users")
        .fetch_one(&state.db)
        .await?;

    let (tenant, role) = if user_count == 0 {
        // Fresh instance: adopt the seeded default tenant rather than creating
        // a duplicate beside it, and the first person owns it.
        let name = state.cfg.default_tenant_name.clone();
        let slug = slugify(&name);
        let existing: Option<Tenant> = sqlx::query_as("SELECT * FROM tenants WHERE slug = $1")
            .bind(&slug)
            .fetch_optional(&state.db)
            .await?;
        let tenant = match existing {
            Some(t) => t,
            None => {
                sqlx::query_as(
                    "INSERT INTO tenants (id, name, slug) VALUES ($1, $2, $3) RETURNING *",
                )
                .bind(TenantId::new())
                .bind(&name)
                .bind(&slug)
                .fetch_one(&state.db)
                .await?
            }
        };
        (tenant, "owner")
    } else {
        // Everyone else: their own tenant, which they own. Sharing a machine or
        // a repo with someone is a grant in `tenant_members`, made deliberately,
        // rather than a side effect of signing up.
        (
            create_personal_tenant(state, &display_name, &email).await?,
            "owner",
        )
    };

    // Same email already present in the tenant (e.g. relinked IdP): attach the
    // new identity to that user instead of creating a duplicate.
    let user: Option<User> =
        sqlx::query_as("SELECT * FROM users WHERE tenant_id = $1 AND email = $2")
            .bind(tenant.id)
            .bind(&email)
            .fetch_optional(&state.db)
            .await?;

    // Commit the tenant to OIDC before creating anything. A tenant already on
    // local accounts must be refused here, with nothing half-made left behind.
    crate::services::local_auth::claim_mode(
        &state.db,
        tenant.id,
        crate::services::local_auth::AuthMode::Oidc,
    )
    .await?;

    let user = match user {
        Some(u) => u,
        None => {
            sqlx::query_as(
                "INSERT INTO users (id, tenant_id, display_name, email, avatar_url, role)
                 VALUES ($1, $2, $3, $4, $5, $6) RETURNING *",
            )
            .bind(UserId::new())
            .bind(tenant.id)
            .bind(&display_name)
            .bind(&email)
            .bind(&claims.avatar_url)
            .bind(role)
            .fetch_one(&state.db)
            .await?
        }
    };

    // Membership mirrors the personal tenant. Written even in the single-tenant
    // case, so "which tenants can this user reach" has exactly one answer to
    // read — the table — rather than two rules to keep in step.
    sqlx::query(
        "INSERT INTO tenant_members (id, tenant_id, principal_type, principal_id, role)
         VALUES ($1, $2, 'user', $3, $4)
         ON CONFLICT (tenant_id, principal_type, principal_id) DO NOTHING",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(tenant.id)
    .bind(user.id.0)
    .bind(role)
    .execute(&state.db)
    .await?;

    sqlx::query(
        "INSERT INTO identities (id, user_id, issuer, subject, email, raw_claims)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(IdentityId::new())
    .bind(user.id)
    .bind(&claims.issuer)
    .bind(&claims.subject)
    .bind(&claims.email)
    .bind(&claims.raw_claims)
    .execute(&state.db)
    .await?;

    // Somebody has to be able to run this deployment. Seeding cannot do it —
    // it runs before anybody has signed in — and "the next boot will pick it
    // up" is not true of a control plane nobody restarts, so a fresh instance
    // would have had no operator and no way to grow one.
    //
    // Idempotent by "only when NO deployment binding exists", so calling it on
    // every sign-in costs one indexed lookup and a second person can never
    // become an operator by accident.
    crate::seed::bootstrap_operator(&state.db).await;

    Ok((user, tenant))
}

/// A tenant of one, named for the person it belongs to.
///
/// The name is cosmetic; the slug is not — it is unique instance-wide, and two
/// people called "ryan" must not collide. So a taken slug gets a short random
/// suffix rather than failing the login, which is the one moment a new user
/// cannot recover from an error on their own.
async fn create_personal_tenant(
    state: &AppState,
    display_name: &str,
    email: &str,
) -> ApiResult<Tenant> {
    use rand::distr::Alphanumeric;
    use rand::Rng;

    let name = if display_name.trim().is_empty() {
        email.split('@').next().unwrap_or("user").to_string()
    } else {
        display_name.trim().to_string()
    };
    let base = slugify(&name);

    for attempt in 0..5 {
        let slug = if attempt == 0 {
            base.clone()
        } else {
            let suffix: String = rand::rng()
                .sample_iter(&Alphanumeric)
                .take(4)
                .map(char::from)
                .collect();
            format!("{base}-{}", suffix.to_lowercase())
        };
        let res: Result<Tenant, sqlx::Error> =
            sqlx::query_as("INSERT INTO tenants (id, name, slug) VALUES ($1, $2, $3) RETURNING *")
                .bind(TenantId::new())
                .bind(&name)
                .bind(&slug)
                .fetch_one(&state.db)
                .await;
        match res {
            Ok(tenant) => return Ok(tenant),
            Err(sqlx::Error::Database(d)) if d.is_unique_violation() => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(ApiError::Internal(anyhow::anyhow!(
        "could not allocate a tenant slug for {name}"
    )))
}

pub fn slugify(s: &str) -> String {
    let slug: String = s
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        "tenant".into()
    } else {
        slug
    }
}

#[cfg(test)]
mod tests {
    use super::{slugify, to_memberships};
    use chrono::Utc;
    use nook_types::TenantId;

    #[test]
    fn slugs_are_url_safe_and_stable() {
        assert_eq!(slugify("My Team's Space"), "my-team-s-space");
        assert_eq!(slugify("dev"), "dev");
        assert_eq!(slugify("  --  "), "tenant");
        assert_eq!(slugify("Ünïcode Nämé"), "n-code-n-m"); // ascii-only by design
    }

    #[test]
    fn exactly_the_active_tenant_is_marked_current() {
        let a = TenantId::new();
        let b = TenantId::new();
        let now = Utc::now();
        let rows = vec![
            (a, "Personal".into(), "personal".into(), "owner".into(), now),
            (b, "Shared".into(), "shared".into(), "member".into(), now),
        ];
        let out = to_memberships(rows, b);
        assert_eq!(out.len(), 2);
        assert!(!out[0].current, "the non-active tenant is not current");
        assert!(out[1].current, "the active tenant is current");
        // The role and identity pass through untouched.
        assert_eq!(out[1].role, "member");
        assert_eq!(out[1].id, b);
    }

    #[test]
    fn no_tenant_is_current_when_active_is_absent() {
        // A session scoped to a tenant the person is no longer a member of: the
        // list simply contains no `current`, which the UI renders as "none
        // selected" rather than crashing.
        let a = TenantId::new();
        let orphan = TenantId::new();
        let rows = vec![(
            a,
            "Personal".into(),
            "personal".into(),
            "owner".into(),
            Utc::now(),
        )];
        let out = to_memberships(rows, orphan);
        assert!(out.iter().all(|m| !m.current));
    }
}

/// Behavioral tests that hit a real Postgres — the AC-3 regression can only be
/// proven against the database, since it is about what the SQL join returns.
/// They connect to `DATABASE_URL` and no-op when the DB is absent (the same
/// `NOOK_REQUIRE_DB` gate the rest of the suite uses), so `cargo test` on a
/// machine without Postgres still passes.
#[cfg(test)]
mod db_tests {
    use super::{active_membership_exists, member_user_in_tenant, memberships_for};
    use nook_types::{TenantId, UserId};
    use sqlx::postgres::PgPoolOptions;
    use sqlx::PgPool;
    use uuid::Uuid;

    /// A pool, or `None` when there is no database to talk to — in which case
    /// the test returns early rather than failing, matching the suite's
    /// convention that DB-backed tests are skipped without `NOOK_REQUIRE_DB`.
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
        // Self-provision: apply the migration set so these tests pass against a
        // FRESH database, not only the container's already-migrated one. CI
        // points DATABASE_URL at an empty Postgres; without this the first
        // `INSERT INTO tenants` hit "relation does not exist" and the security
        // regression errored out before it could assert anything. `MIGRATOR` is
        // idempotent, so running it here is a no-op on an already-migrated DB.
        crate::MIGRATOR.run(&db).await.ok()?;
        Some(db)
    }

    async fn tenant(db: &PgPool, name: &str) -> TenantId {
        let id = Uuid::new_v4();
        // Slug is unique instance-wide; the uuid keeps parallel/repeat runs from
        // colliding.
        sqlx::query("INSERT INTO tenants (id, name, slug) VALUES ($1, $2, $3)")
            .bind(id)
            .bind(name)
            .bind(format!("main12-{id}"))
            .execute(db)
            .await
            .unwrap();
        TenantId(id)
    }

    /// A `users` row with an EXPLICIT `person_id` and `email`, plus its
    /// `tenant_members` grant — the two knobs AC-3 turns.
    async fn member(db: &PgPool, tenant: TenantId, email: &str, person: Uuid) -> UserId {
        let uid = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO users (id, tenant_id, display_name, email, role, person_id)
             VALUES ($1, $2, 'T', $3, 'member', $4)",
        )
        .bind(uid)
        .bind(tenant.0)
        .bind(email)
        .bind(person)
        .execute(db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO tenant_members (id, tenant_id, principal_type, principal_id, role)
             VALUES ($1, $2, 'user', $3, 'member')",
        )
        .bind(Uuid::new_v4())
        .bind(tenant.0)
        .bind(uid)
        .execute(db)
        .await
        .unwrap();
        UserId(uid)
    }

    async fn cleanup(db: &PgPool, tenants: &[TenantId]) {
        for t in tenants {
            let _ = sqlx::query("DELETE FROM tenant_members WHERE tenant_id = $1")
                .bind(t.0)
                .execute(db)
                .await;
            let _ = sqlx::query("DELETE FROM users WHERE tenant_id = $1")
                .bind(t.0)
                .execute(db)
                .await;
            let _ = sqlx::query("DELETE FROM tenants WHERE id = $1")
                .bind(t.0)
                .execute(db)
                .await;
        }
    }

    /// AC-3, both directions: membership follows `person_id`, never email.
    ///
    /// - `me` (tenant A, email `shared@`, person P1)
    /// - `imposter` (tenant B, SAME email `shared@`, DIFFERENT person P2) — the
    ///   account-takeover row: under the old email join it would have granted
    ///   `me` reach into B. It must not.
    /// - `twin` (tenant C, DIFFERENT email `other@`, SAME person P1) — the
    ///   legitimate shared membership. It must be reachable, proving the join is
    ///   by person and not by email.
    #[tokio::test]
    async fn membership_follows_person_id_not_email() {
        let Some(db) = pool().await else {
            eprintln!("skipping membership_follows_person_id_not_email — no DATABASE_URL");
            return;
        };

        let p1 = Uuid::new_v4();
        let p2 = Uuid::new_v4();
        let a = tenant(&db, "A").await;
        let b = tenant(&db, "B").await;
        let c = tenant(&db, "C").await;

        let me = member(&db, a, "shared@main12.test", p1).await;
        let _imposter = member(&db, b, "shared@main12.test", p2).await;
        let twin = member(&db, c, "other@main12.test", p1).await;

        // Collect every result BEFORE asserting, so cleanup always runs even
        // when an assertion is about to fail.
        let reachable: Vec<TenantId> = memberships_for(&db, me, a)
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.id)
            .collect();
        let into_b = member_user_in_tenant(&db, me, b).await.unwrap();
        let into_c = member_user_in_tenant(&db, me, c).await.unwrap();

        cleanup(&db, &[a, b, c]).await;

        // Same person → tenants A and C are reachable; the same-email imposter
        // tenant B is NOT.
        assert!(reachable.contains(&a), "own tenant A is reachable");
        assert!(
            reachable.contains(&c),
            "tenant C (same person_id, different email) is reachable — resolution is by person"
        );
        assert!(
            !reachable.contains(&b),
            "tenant B (same email, different person_id) must NOT be reachable — this is the account-takeover the email join allowed"
        );

        // The switch guard agrees: refused into B, allowed into C as the twin.
        assert!(
            into_b.is_none(),
            "member_user_in_tenant must refuse B (matching email, different person)"
        );
        assert_eq!(
            into_c,
            Some(twin),
            "member_user_in_tenant must resolve C to the twin row (matching person)"
        );
    }

    /// AC-1/AC-4: the migration ran (`person_id` exists and is NOT NULL), and
    /// the value comes from the platform default `gen_random_uuid()` — so rows
    /// created without specifying it get their OWN distinct value, never one
    /// derived from email. This is the same per-row volatile default that
    /// backfilled the pre-existing rows, so it proves the distinctness AC-4
    /// requires without depending on other rows in a shared dev database.
    #[tokio::test]
    async fn person_id_defaults_to_a_distinct_platform_value() {
        let Some(db) = pool().await else {
            eprintln!("skipping person_id_defaults_to_a_distinct_platform_value — no DATABASE_URL");
            return;
        };

        // Column exists and is NOT NULL (the query erroring would fail the test,
        // and the constraint guarantees no nulls — this also confirms 0002 ran).
        let (nulls,): (i64,) = sqlx::query_as("SELECT count(*) FROM users WHERE person_id IS NULL")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(nulls, 0, "every users row has a person_id");

        // Insert three users that DO NOT set person_id — the same email even —
        // and confirm the default gave each a distinct, non-email value.
        let t = tenant(&db, "defaults").await;
        let mut ids = Vec::new();
        for i in 0..3 {
            let uid = Uuid::new_v4();
            sqlx::query(
                "INSERT INTO users (id, tenant_id, display_name, email, role)
                 VALUES ($1, $2, 'D', $3, 'member')",
            )
            .bind(uid)
            .bind(t.0)
            // Distinct emails only because of the per-tenant unique constraint;
            // the point is that person_id is NOT derived from them.
            .bind(format!("d{i}@main12.test"))
            .execute(&db)
            .await
            .unwrap();
            ids.push(uid);
        }
        let persons: Vec<Uuid> = sqlx::query_as::<_, (Uuid,)>(
            "SELECT person_id FROM users WHERE tenant_id = $1 ORDER BY id",
        )
        .bind(t.0)
        .fetch_all(&db)
        .await
        .unwrap()
        .into_iter()
        .map(|(p,)| p)
        .collect();

        cleanup(&db, &[t]).await;

        assert_eq!(persons.len(), 3);
        let mut sorted = persons.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            3,
            "the default assigns each row a distinct person_id"
        );
    }

    /// AC-7: revoking a `tenant_members` grant takes effect immediately.
    /// `active_membership_exists` flips to false the moment the row is gone (the
    /// per-request `AuthCtx` guard), and `member_user_in_tenant` then refuses a
    /// switch INTO that same tenant — closing the same-tenant shortcut hole.
    #[tokio::test]
    async fn a_revoked_grant_stops_working_immediately() {
        let Some(db) = pool().await else {
            eprintln!("skipping a_revoked_grant_stops_working_immediately — no DATABASE_URL");
            return;
        };

        let person = Uuid::new_v4();
        let t = tenant(&db, "revoke").await;
        let me = member(&db, t, "revoke@main12.test", person).await;

        // While the grant is live: the session guard passes and re-selecting the
        // current tenant resolves to this user.
        let live_ok = active_membership_exists(&db, me, t).await.unwrap();
        let switch_live = member_user_in_tenant(&db, me, t).await.unwrap();

        // Revoke the grant (what member-management / a leave will do).
        sqlx::query("DELETE FROM tenant_members WHERE tenant_id = $1 AND principal_id = $2")
            .bind(t.0)
            .bind(me.0)
            .execute(&db)
            .await
            .unwrap();

        let revoked_ok = active_membership_exists(&db, me, t).await.unwrap();
        let switch_revoked = member_user_in_tenant(&db, me, t).await.unwrap();

        cleanup(&db, &[t]).await;

        assert!(live_ok, "a live grant passes the session guard");
        assert_eq!(switch_live, Some(me), "a live grant resolves the switch");
        assert!(
            !revoked_ok,
            "a revoked grant fails the AuthCtx session guard immediately, not at logout"
        );
        assert!(
            switch_revoked.is_none(),
            "a revoked grant refuses a switch into the same tenant (no same-tenant shortcut)"
        );
    }
}
