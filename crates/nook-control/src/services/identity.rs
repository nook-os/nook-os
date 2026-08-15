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

use nook_types::{
    Page, PageQuery, Tenant, TenantId, TenantMemberItem, TenantMembership, User, UserId,
};
use serde_json::Value;

use crate::error::{ApiError, ApiResult};
use crate::repo::identity::{IdentityRepository, MembershipRow};
use crate::state::AppState;

/// A user's name as an authored record spells it — a comment's author, a
/// report's (MAIN-603). Identity data, so it comes from that aggregate's
/// repository rather than a second copy of the query beside each caller
/// (MAIN-246/249).
///
/// Missing resolves to `"unknown"` rather than failing: a node token has no
/// user row behind it, and a record of the work is worth more than a refusal
/// over the name on it.
pub async fn display_name(state: &AppState, user: UserId) -> String {
    state
        .identity
        .get_user(user)
        .await
        .ok()
        .flatten()
        .map(|u| u.display_name)
        .unwrap_or_else(|| "unknown".into())
}

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
    repo: &dyn IdentityRepository,
    user_id: UserId,
    active: TenantId,
) -> ApiResult<Vec<TenantMembership>> {
    Ok(to_memberships(repo.memberships_of(user_id).await?, active))
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
    repo: &dyn IdentityRepository,
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
    let list = memberships_for(repo, user_id, active).await?;
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
    repo: &dyn IdentityRepository,
    user_id: UserId,
) {
    let ids: Vec<UserId> = repo.sibling_user_ids(user_id).await.unwrap_or_default();
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
fn to_memberships(rows: Vec<MembershipRow>, active: TenantId) -> Vec<TenantMembership> {
    rows.into_iter()
        .map(|r| TenantMembership {
            current: r.tenant_id == active,
            id: r.tenant_id,
            name: r.name,
            slug: r.slug,
            role: r.role,
            created_at: r.created_at,
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
    repo: &dyn IdentityRepository,
    user_id: UserId,
    target: TenantId,
) -> ApiResult<Option<UserId>> {
    repo.user_in_tenant(user_id, target).await
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
    repo: &dyn IdentityRepository,
    user_id: UserId,
    tenant: TenantId,
) -> ApiResult<bool> {
    repo.has_active_membership(user_id, tenant).await
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
pub async fn email_is_verified(repo: &dyn IdentityRepository, user_id: UserId) -> ApiResult<bool> {
    repo.email_is_verified(user_id).await
}

/// Record that a local account's email was verified (MAIN-30), through the same
/// verified-email model OIDC uses. A local account has no identity of its own,
/// so a completed local round-trip writes one: issuer `local`, keyed to the
/// user, carrying `email_verified_at`. `email_is_verified` then reports true
/// with no change to the predicate. Idempotent — a second confirm keeps the
/// first verification time.
pub async fn mark_local_email_verified(
    repo: &dyn IdentityRepository,
    user_id: UserId,
    email: &str,
) -> ApiResult<()> {
    repo.mark_local_email_verified(user_id, email).await
}

/// The pseudo-issuer the dev-login hatch stamps on its identities. It is never a
/// real IdP, and it is only ever produced behind `dev_login`'s
/// `AUTH_DEV_MODE && !production` gate — so recognising it here is exactly as
/// narrow as that gate (MAIN-221 AC-1). Its purpose: the dev hatch bypasses the
/// auth-mode lock, which would otherwise refuse a dev sign-in on a tenant that
/// mode-locked to `local`.
pub const DEV_ISSUER: &str = "nookos-dev";

pub async fn login_identity(state: &AppState, claims: IdentityClaims) -> ApiResult<(User, Tenant)> {
    // The dev hatch is allowed to sign in without claiming (or being refused by)
    // the tenant's auth mode — it is a testing tool, not a real IdP, and it must
    // work on a `local`-locked instance. Every real issuer still claims the mode
    // below, so the one-way lock is untouched for genuine sign-ins (AC-2, NG-1).
    let claims_the_mode = claims.issuer != DEV_ISSUER;
    // Existing identity → existing user.
    let repo = state.identity.as_ref();
    let existing = repo
        .user_id_by_identity(&claims.issuer, &claims.subject)
        .await?;

    if let Some(user_id) = existing {
        let user = repo.get_user(user_id).await?.ok_or(ApiError::NotFound)?;
        let tenant = repo
            .get_tenant(user.tenant_id)
            .await?
            .ok_or(ApiError::NotFound)?;
        // A returning identity may have become verified since last time (the IdP
        // confirmed the address). Record it the first time we see the claim, and
        // never clear it — verification only moves one way, and only from a true
        // claim.
        if claims.email_verified {
            repo.mark_identity_verified(&claims.issuer, &claims.subject)
                .await?;
        }
        // The lock has to bind both directions, or it is not a lock: a tenant
        // running local accounts must not silently acquire OIDC identities
        // beside them, which is exactly the duplicate-person problem the mode
        // exists to prevent. (Skipped for the dev hatch — AC-1.)
        if claims_the_mode {
            crate::services::local_auth::claim_mode(
                state.identity.as_ref(),
                tenant.id,
                crate::services::local_auth::AuthMode::Oidc,
            )
            .await?;
        }
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
    let user_count = repo.count_users().await?;

    let (tenant, role) = if user_count == 0 {
        // Fresh instance: adopt the seeded default tenant rather than creating
        // a duplicate beside it, and the first person owns it.
        let name = state.cfg.default_tenant_name.clone();
        let slug = slugify(&name);
        let tenant = match repo.tenant_by_slug(&slug).await? {
            Some(t) => t,
            // Nothing else can be racing the seeded default tenant on a fresh
            // instance, so a taken slug here would be a genuine surprise rather
            // than the ordinary contention `create_personal_tenant` retries on.
            None => repo.create_tenant(&name, &slug).await?.ok_or_else(|| {
                ApiError::Internal(anyhow::anyhow!(
                    "default tenant slug {slug} was taken between read and write"
                ))
            })?,
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
    let user = repo.user_by_email_in_tenant(tenant.id, &email).await?;

    // Commit the tenant to OIDC before creating anything. A tenant already on
    // local accounts must be refused here, with nothing half-made left behind.
    // (Skipped for the dev hatch — AC-1; the dev issuer never locks the mode.)
    if claims_the_mode {
        crate::services::local_auth::claim_mode(
            state.identity.as_ref(),
            tenant.id,
            crate::services::local_auth::AuthMode::Oidc,
        )
        .await?;
    }

    let user = match user {
        Some(u) => u,
        None => {
            repo.create_oidc_user(crate::repo::identity::NewOidcUser {
                tenant: tenant.id,
                display_name: display_name.clone(),
                email: email.clone(),
                avatar_url: claims.avatar_url.clone(),
                role: role.to_string(),
            })
            .await?
        }
    };

    // Membership mirrors the personal tenant. Written even in the single-tenant
    // case, so "which tenants can this user reach" has exactly one answer to
    // read — the table — rather than two rules to keep in step.
    repo.grant_membership(tenant.id, user.id, role).await?;

    // `email_verified_at` is stamped now ONLY when the IdP asserted the address;
    // otherwise it stays null. A CASE on the bound flag keeps "verified means a
    // real timestamp" true — nothing here derives it from the email string.
    repo.create_identity(crate::repo::identity::NewIdentity {
        user_id: user.id,
        issuer: claims.issuer.clone(),
        subject: claims.subject.clone(),
        email: claims.email.clone(),
        raw_claims: claims.raw_claims.clone(),
        email_verified: claims.email_verified,
    })
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
        // `None` is "slug taken" — the retry the loop exists for. The driver
        // detail that used to be matched on here now stops inside the repo.
        if let Some(tenant) = state.identity.create_tenant(&name, &slug).await? {
            return Ok(tenant);
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
    use super::{slugify, to_memberships, MembershipRow};
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
            MembershipRow {
                tenant_id: a,
                name: "Personal".into(),
                slug: "personal".into(),
                role: "owner".into(),
                created_at: now,
            },
            MembershipRow {
                tenant_id: b,
                name: "Shared".into(),
                slug: "shared".into(),
                role: "member".into(),
                created_at: now,
            },
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
        let rows = vec![MembershipRow {
            tenant_id: a,
            name: "Personal".into(),
            slug: "personal".into(),
            role: "owner".into(),
            created_at: Utc::now(),
        }];
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

    /// The real repository over this test's pool — these stay DB-backed (NG-4).
    fn repo_of(db: &DbPool) -> crate::repo::identity::DbIdentityRepository {
        crate::repo::identity::DbIdentityRepository::new(db.clone())
    }
    use super::{
        active_membership_exists, cached_memberships_for, email_is_verified,
        invalidate_person_tenants, member_user_in_tenant, memberships_for,
    };
    use crate::cache::memory::MemoryCache;
    use nook_db::dialect::type_mapping;
    use nook_db::{params, Db, DbPool};
    use nook_types::{TenantId, UserId};
    use sqlx::postgres::PgPoolOptions;
    use uuid::Uuid;

    /// A pool, or `None` when there is no database to talk to — in which case
    /// the test returns early rather than failing, matching the suite's
    /// convention that DB-backed tests are skipped without `NOOK_REQUIRE_DB`.
    async fn pool() -> Option<DbPool> {
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
        Some(nook_db::EnginePool::from_pg(db))
    }

    async fn tenant(db: &DbPool, name: &str) -> TenantId {
        let id = Uuid::new_v4();
        // Slug is unique instance-wide; the uuid keeps parallel/repeat runs from
        // colliding.
        db.exec(
            "INSERT INTO tenants (id, name, slug) VALUES ($1, $2, $3)",
            params![id, name, format!("main12-{id}")],
        )
        .await
        .unwrap();
        TenantId(id)
    }

    /// A `users` row with an EXPLICIT `person_id` and `email`, plus its
    /// `tenant_members` grant — the two knobs AC-3 turns.
    async fn member(db: &DbPool, tenant: TenantId, email: &str, person: Uuid) -> UserId {
        let uid = Uuid::new_v4();
        db.exec(
            "INSERT INTO users (id, tenant_id, display_name, email, role, person_id)
             VALUES ($1, $2, 'T', $3, 'member', $4)",
            params![uid, tenant.0, email, person],
        )
        .await
        .unwrap();
        db.exec(
            "INSERT INTO tenant_members (id, tenant_id, principal_type, principal_id, role)
             VALUES ($1, $2, 'user', $3, 'member')",
            params![Uuid::new_v4(), tenant.0, uid],
        )
        .await
        .unwrap();
        UserId(uid)
    }

    async fn cleanup(db: &DbPool, tenants: &[TenantId]) {
        for t in tenants {
            let _ = db
                .exec(
                    "DELETE FROM tenant_members WHERE tenant_id = $1",
                    params![t.0],
                )
                .await;
            let _ = db
                .exec("DELETE FROM users WHERE tenant_id = $1", params![t.0])
                .await;
            let _ = db
                .exec("DELETE FROM tenants WHERE id = $1", params![t.0])
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
        let reachable: Vec<TenantId> = memberships_for(&repo_of(&db), me, a)
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.id)
            .collect();
        let into_b = member_user_in_tenant(&repo_of(&db), me, b).await.unwrap();
        let into_c = member_user_in_tenant(&repo_of(&db), me, c).await.unwrap();

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
        let nulls: i64 = db
            .query_scalar(
                "SELECT count(*) FROM users WHERE person_id IS NULL",
                params![],
            )
            .await
            .unwrap();
        assert_eq!(nulls, 0, "every users row has a person_id");

        // Insert three users that DO NOT set person_id — the same email even —
        // and confirm the default gave each a distinct, non-email value.
        let t = tenant(&db, "defaults").await;
        let mut ids = Vec::new();
        for i in 0..3 {
            let uid = Uuid::new_v4();
            db.exec(
                "INSERT INTO users (id, tenant_id, display_name, email, role)
                 VALUES ($1, $2, 'D', $3, 'member')",
                // Distinct emails only because of the per-tenant unique constraint;
                // the point is that person_id is NOT derived from them.
                params![uid, t.0, format!("d{i}@main12.test")],
            )
            .await
            .unwrap();
            ids.push(uid);
        }
        let persons: Vec<Uuid> = db
            .query_scalar_all(
                "SELECT person_id FROM users WHERE tenant_id = $1 ORDER BY id",
                params![t.0],
            )
            .await
            .unwrap();

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
        let live_ok = active_membership_exists(&repo_of(&db), me, t)
            .await
            .unwrap();
        let switch_live = member_user_in_tenant(&repo_of(&db), me, t).await.unwrap();

        // Revoke the grant (what member-management / a leave will do).
        db.exec(
            "DELETE FROM tenant_members WHERE tenant_id = $1 AND principal_id = $2",
            params![t.0, me.0],
        )
        .await
        .unwrap();

        let revoked_ok = active_membership_exists(&repo_of(&db), me, t)
            .await
            .unwrap();
        let switch_revoked = member_user_in_tenant(&repo_of(&db), me, t).await.unwrap();

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
        db.exec(
            "INSERT INTO identities (id, user_id, issuer, subject, email, raw_claims)
             VALUES ($1, $2, 'idp', $3, 'shared@main29.test', '{}')",
            params![
                Uuid::new_v4(),
                oidc.0,
                format!("sub-{}", Uuid::new_v4().simple())
            ],
        )
        .await
        .unwrap();

        let local_before = email_is_verified(&repo_of(&db), local).await.unwrap();
        let oidc_unverified = email_is_verified(&repo_of(&db), oidc).await.unwrap();

        // Now the IdP verifies the OIDC identity's address.
        db.exec(
            &format!(
                "UPDATE identities SET email_verified_at = {} WHERE user_id = $1",
                type_mapping(db.engine()).now()
            ),
            params![oidc.0],
        )
        .await
        .unwrap();
        let oidc_verified = email_is_verified(&repo_of(&db), oidc).await.unwrap();
        // The local user shares the email but is still unverified — no string join.
        let local_after = email_is_verified(&repo_of(&db), local).await.unwrap();

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
        let first = cached_memberships_for(&cache, &repo_of(&db), uid, a)
            .await
            .unwrap();
        assert_eq!(first.len(), 1, "the live membership is returned and cached");

        // Revoke the grant in the DB WITHOUT invalidating the cache.
        db.exec(
            "DELETE FROM tenant_members WHERE principal_id = $1",
            params![uid.0],
        )
        .await
        .unwrap();

        // A hit: the stale list is served, proving the join was skipped.
        let hit = cached_memberships_for(&cache, &repo_of(&db), uid, a)
            .await
            .unwrap();
        assert_eq!(hit.len(), 1, "served the cached list — the read was a hit");

        // Explicit invalidation → the next read re-queries and sees the revoke.
        invalidate_person_tenants(&cache, &repo_of(&db), uid).await;
        let fresh = cached_memberships_for(&cache, &repo_of(&db), uid, a)
            .await
            .unwrap();
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
            cached_memberships_for(&cache, &repo_of(&db), uid_a, a)
                .await
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            cached_memberships_for(&cache, &repo_of(&db), uid_b, b)
                .await
                .unwrap()
                .len(),
            2
        );

        // A change on row B, invalidated via row B, must refresh row A too.
        db.exec(
            "DELETE FROM tenant_members WHERE principal_id = $1",
            params![uid_b.0],
        )
        .await
        .unwrap();
        invalidate_person_tenants(&cache, &repo_of(&db), uid_b).await;

        let a_fresh = cached_memberships_for(&cache, &repo_of(&db), uid_a, a)
            .await
            .unwrap();
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
            cached_memberships_for(&cache, &repo_of(&db), uid, a)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(active_membership_exists(&repo_of(&db), uid, a)
            .await
            .unwrap());

        // Revoke, but do NOT invalidate: the display cache stays stale on purpose.
        db.exec(
            "DELETE FROM tenant_members WHERE principal_id = $1",
            params![uid.0],
        )
        .await
        .unwrap();
        assert_eq!(
            cached_memberships_for(&cache, &repo_of(&db), uid, a)
                .await
                .unwrap()
                .len(),
            1,
            "the display list is still a stale hit"
        );

        // The gate reads the table directly, so access is refused the instant
        // the grant is gone — the cache never gates.
        assert!(
            !active_membership_exists(&repo_of(&db), uid, a)
                .await
                .unwrap(),
            "a stale display cache must never keep access alive"
        );

        cleanup(&db, &[a]).await;
    }
}

/// Moved verbatim from `services/core.rs` (MAIN-245): a tenant-members page
/// is identity data, and the repository chain needs it to live with the rest
/// of the aggregate. Its tests travelled to `operator_queries`, where the
/// shared keyset behaviour they exercise is covered as one piece.
/// The tenant's sort allowlist — the members list's half of the pagination
/// contract, beside the service that speaks it.
pub const MEMBER_SORTS: &[(&str, &str)] = &[
    ("email", "email"),
    ("name", "display_name"),
    ("role", "role"),
    ("joined", "joined_at"),
];

/// Tenant members, paged + searched (email/name/role) + sorted per the
/// pagination contract (MAIN-45 AC-2, reshaped by the QOL sweep). Keyed on the
/// member's UUID v7 `principal_id`; reaches only members of `tenant`.
pub async fn tenant_members_page(
    repo: &dyn IdentityRepository,
    tenant: TenantId,
    wire: &PageQuery,
) -> ApiResult<Page<TenantMemberItem>> {
    let args = wire
        .args(MEMBER_SORTS)
        .map_err(crate::services::operator_queries::bad_page)?;
    Ok(repo.members_page(tenant, &args).await?.into())
}

/// The instance's first tenant — what an MCP token maps to until per-user MCP
/// OAuth exists.
pub async fn first_tenant(repo: &dyn IdentityRepository) -> anyhow::Result<TenantId> {
    repo.first_tenant()
        .await?
        .ok_or_else(|| anyhow::anyhow!("this instance has no tenants"))
}

/// A tenant's owner — who an MCP call acts as.
pub async fn first_user(repo: &dyn IdentityRepository, tenant: TenantId) -> anyhow::Result<UserId> {
    repo.first_user(tenant)
        .await?
        .ok_or_else(|| anyhow::anyhow!("tenant has no users"))
}
