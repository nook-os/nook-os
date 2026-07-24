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

/// How long a cached tenants list survives without explicit invalidation
/// (MAIN-27 AC-4). Short by design: it is the ONLY freshness guarantee across
/// processes (the in-memory cache is per-instance, NG-4), so a grant revoked on
/// one replica is reflected on the others within this window even though its
/// explicit invalidation never reaches them.
const TENANTS_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(30);

/// The cache key for a user's reachable-tenant list.
///
/// Keyed by the per-tenant `users` row id. For a browser session `user_id` and
/// the active `tenant_id` move together (`sessions_auth` holds both, and a
/// switch updates the row), so `user_id` alone determines which tenant is
/// `current` — the cached `Vec<TenantMembership>` is correct for that key with
/// no risk of a cross-user or cross-tenant mix-up (AC-3).
fn tenants_cache_key(user_id: UserId) -> String {
    format!("tenants:user:{}", user_id.0)
}

/// `memberships_for`, served through the cache (AC-3).
///
/// A hit returns the stored list and skips the four-table join; a miss runs the
/// join and populates with a TTL backstop. The `current` flag is re-derived
/// from `active` on every read, so a cached list is safe to serve even in the
/// (unused today) case where the same `user_id` is queried against a different
/// active tenant.
///
/// NOTE: only the *display* list flows through here. The access gate
/// (`active_membership_exists`) never does — a stale grant must never grant
/// access (NG-2), so authorization always reads the table directly.
pub async fn cached_memberships_for(
    cache: &dyn crate::cache::Cache,
    db: &PgPool,
    user_id: UserId,
    active: TenantId,
) -> ApiResult<Vec<TenantMembership>> {
    let key = tenants_cache_key(user_id);
    if let Ok(Some(bytes)) = cache.get(&key).await {
        if let Ok(mut list) = serde_json::from_slice::<Vec<TenantMembership>>(&bytes) {
            for m in &mut list {
                m.current = m.id == active;
            }
            return Ok(list);
        }
    }
    let list = memberships_for(db, user_id, active).await?;
    if let Ok(bytes) = serde_json::to_vec(&list) {
        let _ = cache.set(&key, bytes, TENANTS_CACHE_TTL).await;
    }
    Ok(list)
}

/// Drop the cached tenants list for every `users` row of the person behind
/// `user_id` (AC-4).
///
/// A grant change or a tenant switch affects the whole person, and a person is
/// several `users` rows (one per tenant) correlated by `person_id` — the same
/// correlation `memberships_for` joins on. Invalidating only the row that was
/// touched would leave that person's OTHER sessions serving a stale list until
/// the TTL. Best-effort: a delete that fails (or a person we cannot resolve)
/// falls back to the TTL, and must never fail the write path that called it.
pub async fn invalidate_person_tenants(
    cache: &dyn crate::cache::Cache,
    db: &PgPool,
    user_id: UserId,
) {
    let ids: Vec<UserId> = sqlx::query_scalar(
        "SELECT u.id FROM users me JOIN users u ON u.person_id = me.person_id WHERE me.id = $1",
    )
    .bind(user_id)
    .fetch_all(db)
    .await
    .unwrap_or_default();
    // The join includes `me`, so an empty result means the row is gone; delete
    // its own key anyway as a floor.
    if ids.is_empty() {
        let _ = cache.delete(&tenants_cache_key(user_id)).await;
    }
    for id in ids {
        let _ = cache.delete(&tenants_cache_key(id)).await;
    }
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
    /// The IdP's `email_verified` claim. `true` ONLY when the issuer asserts it;
    /// an absent or false claim, and every non-OIDC source (the dev login), is
    /// `false`. This is the only thing that may set `email_verified_at` — never
    /// the mere presence of an email string (MAIN-29).
    pub email_verified: bool,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    pub raw_claims: Value,
}

/// Whether this user's email is verified — for authorization to consume.
///
/// True only when the user holds an identity carrying a real verification
/// timestamp. It is deliberately NOT satisfied by an email string matching
/// anything: a local account (no identity) is unverified, and so is an OIDC
/// login whose IdP did not assert `email_verified`. This is the platform
/// predicate invite acceptance and account-linking will gate on.
pub async fn email_is_verified(db: &PgPool, user_id: UserId) -> ApiResult<bool> {
    let (verified,): (bool,) = sqlx::query_as(
        "SELECT EXISTS (
             SELECT 1 FROM identities
             WHERE user_id = $1 AND email_verified_at IS NOT NULL
         )",
    )
    .bind(user_id)
    .fetch_one(db)
    .await?;
    Ok(verified)
}

/// Record that a local account's email was verified (MAIN-30), through the same
/// verified-email model OIDC uses. A local account has no identity of its own,
/// so a completed local round-trip writes one: issuer `local`, keyed to the
/// user, carrying `email_verified_at`. `email_is_verified` then reports true
/// with no change to the predicate. Idempotent — a second confirm keeps the
/// first verification time.
pub async fn mark_local_email_verified(db: &PgPool, user_id: UserId, email: &str) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO identities (id, user_id, issuer, subject, email, raw_claims, email_verified_at)
         VALUES ($1, $2, 'local', $3, $4, '{\"verified_via\":\"local\"}'::jsonb, now())
         ON CONFLICT (issuer, subject)
           DO UPDATE SET email_verified_at = COALESCE(identities.email_verified_at, now())",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(user_id)
    .bind(user_id.0.to_string())
    .bind(email)
    .execute(db)
    .await?;
    Ok(())
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
        // A returning identity may have become verified since last time (the IdP
        // confirmed the address). Record it the first time we see the claim, and
        // never clear it — verification only moves one way, and only from a true
        // claim.
        if claims.email_verified {
            sqlx::query(
                "UPDATE identities SET email_verified_at = now()
                 WHERE issuer = $1 AND subject = $2 AND email_verified_at IS NULL",
            )
            .bind(&claims.issuer)
            .bind(&claims.subject)
            .execute(&state.db)
            .await?;
        }
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

    // `email_verified_at` is stamped now ONLY when the IdP asserted the address;
    // otherwise it stays null. A CASE on the bound flag keeps "verified means a
    // real timestamp" true — nothing here derives it from the email string.
    sqlx::query(
        "INSERT INTO identities (id, user_id, issuer, subject, email, raw_claims, email_verified_at)
         VALUES ($1, $2, $3, $4, $5, $6, CASE WHEN $7 THEN now() ELSE NULL END)",
    )
    .bind(IdentityId::new())
    .bind(user.id)
    .bind(&claims.issuer)
    .bind(&claims.subject)
    .bind(&claims.email)
    .bind(&claims.raw_claims)
    .bind(claims.email_verified)
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
    use super::{
        active_membership_exists, cached_memberships_for, email_is_verified,
        invalidate_person_tenants, member_user_in_tenant, memberships_for,
    };
    use crate::cache::memory::MemoryCache;
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

    /// MAIN-29 AC-3/AC-4: `email_is_verified` is satisfied only by a real
    /// verification timestamp — a local account (no identity) is false, an
    /// unverified identity is false, and a matching email string never makes it
    /// true.
    #[tokio::test]
    async fn email_is_verified_only_from_a_timestamp_never_email() {
        let Some(db) = pool().await else {
            eprintln!("skipping email_is_verified_only_from_a_timestamp — no DATABASE_URL");
            return;
        };
        // Two tenants so both users can hold the SAME email (users are unique on
        // (tenant_id, email)) — the point is that a shared email string never
        // crosses between them.
        let ta = tenant(&db, "verify-a").await;
        let tb = tenant(&db, "verify-b").await;
        // A local-account-style user: a users row with no identity at all.
        let local = member(&db, ta, "shared@main29.test", Uuid::new_v4()).await;
        // Another user, same email, with an (initially unverified) OIDC identity.
        let oidc = member(&db, tb, "shared@main29.test", Uuid::new_v4()).await;
        sqlx::query(
            "INSERT INTO identities (id, user_id, issuer, subject, email, raw_claims)
             VALUES ($1, $2, 'idp', $3, 'shared@main29.test', '{}')",
        )
        .bind(Uuid::new_v4())
        .bind(oidc.0)
        .bind(format!("sub-{}", Uuid::new_v4().simple()))
        .execute(&db)
        .await
        .unwrap();

        let local_before = email_is_verified(&db, local).await.unwrap();
        let oidc_unverified = email_is_verified(&db, oidc).await.unwrap();

        // Now the IdP verifies the OIDC identity's address.
        sqlx::query("UPDATE identities SET email_verified_at = now() WHERE user_id = $1")
            .bind(oidc.0)
            .execute(&db)
            .await
            .unwrap();
        let oidc_verified = email_is_verified(&db, oidc).await.unwrap();
        // The local user shares the email but is still unverified — no string join.
        let local_after = email_is_verified(&db, local).await.unwrap();

        cleanup(&db, &[ta, tb]).await;

        assert!(!local_before, "a local account (no identity) is unverified");
        assert!(
            !oidc_unverified,
            "an identity with a null timestamp is unverified"
        );
        assert!(oidc_verified, "a real timestamp verifies");
        assert!(
            !local_after,
            "sharing an email with a verified user does NOT verify you (never an email join)"
        );
    }

    /// AC-3/AC-4: a second read is a cache hit (skips the join), and an explicit
    /// invalidation drops the entry so the next read reflects the DB.
    #[tokio::test]
    async fn tenants_list_is_cached_then_dropped_on_invalidation() {
        let Some(db) = pool().await else {
            eprintln!(
                "skipping tenants_list_is_cached_then_dropped_on_invalidation — no DATABASE_URL"
            );
            return;
        };
        let person = Uuid::new_v4();
        let a = tenant(&db, "cache-hit").await;
        let uid = member(&db, a, "cache-me@main27.test", person).await;
        let cache = MemoryCache::new();

        // Miss → populates from the join.
        let first = cached_memberships_for(&cache, &db, uid, a).await.unwrap();
        assert_eq!(first.len(), 1, "the live membership is returned and cached");

        // Revoke the grant in the DB WITHOUT invalidating the cache.
        sqlx::query("DELETE FROM tenant_members WHERE principal_id = $1")
            .bind(uid.0)
            .execute(&db)
            .await
            .unwrap();

        // A hit: the stale list is served, proving the join was skipped.
        let hit = cached_memberships_for(&cache, &db, uid, a).await.unwrap();
        assert_eq!(hit.len(), 1, "served the cached list — the read was a hit");

        // Explicit invalidation → the next read re-queries and sees the revoke.
        invalidate_person_tenants(&cache, &db, uid).await;
        let fresh = cached_memberships_for(&cache, &db, uid, a).await.unwrap();
        assert!(
            fresh.is_empty(),
            "after invalidation the revoked grant is gone"
        );

        cleanup(&db, &[a]).await;
    }

    /// AC-4: a grant change / switch touching ONE of a person's tenant rows
    /// invalidates the whole person, so their other sessions refresh too.
    #[tokio::test]
    async fn invalidation_spans_every_tenant_row_of_the_person() {
        let Some(db) = pool().await else {
            eprintln!(
                "skipping invalidation_spans_every_tenant_row_of_the_person — no DATABASE_URL"
            );
            return;
        };
        let person = Uuid::new_v4();
        let a = tenant(&db, "multi-a").await;
        let b = tenant(&db, "multi-b").await;
        let uid_a = member(&db, a, "multi-a@main27.test", person).await;
        let uid_b = member(&db, b, "multi-b@main27.test", person).await;
        let cache = MemoryCache::new();

        // Both per-tenant rows see the same two-tenant set, and both get cached.
        assert_eq!(
            cached_memberships_for(&cache, &db, uid_a, a)
                .await
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            cached_memberships_for(&cache, &db, uid_b, b)
                .await
                .unwrap()
                .len(),
            2
        );

        // A change on row B, invalidated via row B, must refresh row A too.
        sqlx::query("DELETE FROM tenant_members WHERE principal_id = $1")
            .bind(uid_b.0)
            .execute(&db)
            .await
            .unwrap();
        invalidate_person_tenants(&cache, &db, uid_b).await;

        let a_fresh = cached_memberships_for(&cache, &db, uid_a, a).await.unwrap();
        assert_eq!(a_fresh.len(), 1, "invalidating via row B refreshed row A");

        cleanup(&db, &[a, b]).await;
    }

    /// NG-2, the load-bearing boundary: the access gate never reads the cache,
    /// so a revoked grant is refused immediately even while the DISPLAY list is
    /// still cached stale.
    #[tokio::test]
    async fn the_access_gate_is_never_served_from_cache() {
        let Some(db) = pool().await else {
            eprintln!("skipping the_access_gate_is_never_served_from_cache — no DATABASE_URL");
            return;
        };
        let person = Uuid::new_v4();
        let a = tenant(&db, "gate").await;
        let uid = member(&db, a, "gate-me@main27.test", person).await;
        let cache = MemoryCache::new();

        // Warm the display cache and confirm the gate agrees while the grant lives.
        assert_eq!(
            cached_memberships_for(&cache, &db, uid, a)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(active_membership_exists(&db, uid, a).await.unwrap());

        // Revoke, but do NOT invalidate: the display cache stays stale on purpose.
        sqlx::query("DELETE FROM tenant_members WHERE principal_id = $1")
            .bind(uid.0)
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(
            cached_memberships_for(&cache, &db, uid, a)
                .await
                .unwrap()
                .len(),
            1,
            "the display list is still a stale hit"
        );

        // The gate reads the table directly, so access is refused the instant
        // the grant is gone — the cache never gates.
        assert!(
            !active_membership_exists(&db, uid, a).await.unwrap(),
            "a stale display cache must never keep access alive"
        );

        cleanup(&db, &[a]).await;
    }
}
