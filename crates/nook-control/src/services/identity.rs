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
/// A person is one `users` row PER tenant, keyed by email — the same email in
/// two tenants is the same human, which is how membership in a shared tenant is
/// represented (a `users` row with their email + a `tenant_members` grant). So
/// "which tenants can this user reach" is: find every `users` row sharing this
/// user's email, keep the ones with a live `tenant_members` grant, and return
/// their tenants. `tenant_members` is the single source of truth (AC-7): a
/// membership row that is gone drops the tenant from this list.
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
         JOIN users u ON lower(u.email) = lower(me.email)
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
pub async fn member_user_in_tenant(
    db: &PgPool,
    user_id: UserId,
    target: TenantId,
) -> ApiResult<Option<UserId>> {
    let row: Option<(UserId,)> = sqlx::query_as(
        "SELECT u.id
         FROM users me
         JOIN users u ON lower(u.email) = lower(me.email)
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
