//! Local accounts: username and password, held in this database.
//!
//! For people with no identity provider — which is most people running this on
//! their own hardware. It sits beside OIDC rather than replacing it, but a
//! given tenant uses one or the other, never both. See `auth_mode` below.

use anyhow::Result;
use nook_types::{Tenant, TenantId, User, UserId};
use sqlx::PgPool;

use crate::auth::password;
use crate::error::{ApiError, ApiResult};

/// The sign-in method a tenant has committed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    Oidc,
    Local,
}

impl AuthMode {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthMode::Oidc => "oidc",
            AuthMode::Local => "local",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "oidc" => Some(AuthMode::Oidc),
            "local" => Some(AuthMode::Local),
            _ => None,
        }
    }
}

/// Read a tenant's mode. `None` means nobody has signed in yet.
pub async fn mode_of(db: &PgPool, tenant: TenantId) -> Result<Option<AuthMode>, sqlx::Error> {
    let row: Option<(Option<String>,)> =
        sqlx::query_as("SELECT auth_mode FROM tenants WHERE id = $1")
            .bind(tenant)
            .fetch_optional(db)
            .await?;
    Ok(row.and_then(|(m,)| m).and_then(|m| AuthMode::parse(&m)))
}

/// Commit a tenant to one method, or verify it already agrees.
///
/// The commit is a conditional UPDATE rather than a read-then-write: two
/// people signing in at the same moment on a fresh instance would otherwise
/// both see "undecided" and each set their own answer, and the loser would be
/// silently locked out of an instance they thought they had just claimed.
pub async fn claim_mode(db: &PgPool, tenant: TenantId, want: AuthMode) -> ApiResult<()> {
    let settled: Option<(Option<String>,)> = sqlx::query_as(
        "UPDATE tenants SET auth_mode = $2 WHERE id = $1 AND auth_mode IS NULL
         RETURNING auth_mode",
    )
    .bind(tenant)
    .bind(want.as_str())
    .fetch_optional(db)
    .await?;

    if settled.is_some() {
        return Ok(()); // We set it.
    }

    // Already decided — by an earlier sign-in, or by whoever won the race.
    match mode_of(db, tenant).await? {
        Some(m) if m == want => Ok(()),
        Some(other) => Err(ApiError::ForbiddenMsg(format!(
            "this instance signs in with {}, not {}.\n\nThe choice is made on \
             first sign-in and is deliberately one-way: allowing both would let \
             the same person exist twice, with different ids and different \
             permissions, and no reliable way to say which one a grant was meant \
             for. Changing it is a migration with a human deciding how to merge \
             the two.",
            other.as_str(),
            want.as_str()
        ))),
        // The tenant vanished between the two statements.
        None => Err(ApiError::NotFound),
    }
}

/// Sign in with a username and password.
pub async fn login(
    db: &PgPool,
    tenant: TenantId,
    username: &str,
    supplied: &str,
) -> ApiResult<(User, Tenant)> {
    let row: Option<(UserId, Option<String>)> = sqlx::query_as(
        "SELECT id, password_hash FROM users
         WHERE tenant_id = $1 AND lower(username) = lower($2)",
    )
    .bind(tenant)
    .bind(username)
    .fetch_optional(db)
    .await?;

    // One failure for every reason: no such user, no password set (an OIDC
    // account), or the wrong password. Distinguishing them turns this endpoint
    // into a way to find out who has an account here.
    let Some((user_id, Some(hash))) = row else {
        password::waste_time();
        return Err(ApiError::Unauthorized);
    };
    if !password::verify(supplied, &hash) {
        return Err(ApiError::Unauthorized);
    }

    // Only now, once a real credential has been proven, does the tenant get
    // committed to local sign-in. Doing it before would let an anonymous
    // caller with a wrong password lock an instance out of OIDC forever.
    claim_mode(db, tenant, AuthMode::Local).await?;

    let user: User = sqlx::query_as("SELECT * FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_one(db)
        .await?;
    let tenant: Tenant = sqlx::query_as("SELECT * FROM tenants WHERE id = $1")
        .bind(user.tenant_id)
        .fetch_one(db)
        .await?;
    Ok((user, tenant))
}

/// Create a local account.
///
/// `is_first` decides the role: the person who claims an empty instance owns
/// it. Everyone after is created by an admin and starts as a member.
pub async fn create(
    db: &PgPool,
    tenant: TenantId,
    username: &str,
    email: &str,
    display_name: &str,
    plaintext: &str,
    is_first: bool,
) -> ApiResult<User> {
    let username = username.trim();
    validate_username(username).map_err(ApiError::BadRequest)?;
    let hash = password::hash(plaintext).map_err(|e| ApiError::BadRequest(e.to_string()))?;

    let user: User = sqlx::query_as(
        "INSERT INTO users (id, tenant_id, display_name, email, username, password_hash, role)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         RETURNING *",
    )
    .bind(UserId::new())
    .bind(tenant)
    .bind(display_name)
    .bind(email)
    .bind(username)
    .bind(&hash)
    .bind(if is_first { "owner" } else { "member" })
    .fetch_one(db)
    .await
    .map_err(|e| match &e {
        // The unique index on (tenant, lower(username)).
        sqlx::Error::Database(d) if d.is_unique_violation() => {
            ApiError::BadRequest(format!("the username '{username}' is already taken"))
        }
        _ => ApiError::from(e),
    })?;

    // Grant tenant membership, exactly as the OIDC path does in
    // `login_identity`. `tenant_members` is the single source of truth for who
    // can reach a tenant (AC-7); a local user missing this row would be listed
    // in no tenant by `/me/tenants` and — now that `AuthCtx` enforces membership
    // per request — locked out of their own tenant. Idempotent on conflict.
    sqlx::query(
        "INSERT INTO tenant_members (id, tenant_id, principal_type, principal_id, role)
         VALUES ($1, $2, 'user', $3, $4)
         ON CONFLICT (tenant_id, principal_type, principal_id) DO NOTHING",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(tenant)
    .bind(user.id.0)
    .bind(if is_first { "owner" } else { "member" })
    .execute(db)
    .await?;

    Ok(user)
}

/// Change a password, proving the old one first.
pub async fn change_password(
    db: &PgPool,
    user_id: UserId,
    current: &str,
    next: &str,
) -> ApiResult<()> {
    let row: Option<(Option<String>,)> =
        sqlx::query_as("SELECT password_hash FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_optional(db)
            .await?;
    let Some((Some(hash),)) = row else {
        // An OIDC account has no password here, and must not gain one: two
        // ways to become someone, only one of them revocable by the provider.
        return Err(ApiError::ForbiddenMsg(
            "this account signs in through an identity provider and has no password here".into(),
        ));
    };
    if !password::verify(current, &hash) {
        return Err(ApiError::Unauthorized);
    }
    let next_hash = password::hash(next).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    sqlx::query("UPDATE users SET password_hash = $2, updated_at = now() WHERE id = $1")
        .bind(user_id)
        .bind(next_hash)
        .execute(db)
        .await?;
    Ok(())
}

/// Usernames are an identifier, not free text: they appear in URLs, logs and
/// audit trails, and `lower()` has to be enough to make two of them equal.
pub fn validate_username(u: &str) -> Result<(), String> {
    if u.len() < 2 || u.len() > 39 {
        return Err("username must be between 2 and 39 characters".into());
    }
    if !u
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err("username may contain only letters, numbers, dot, dash and underscore".into());
    }
    if !u.chars().next().is_some_and(|c| c.is_ascii_alphanumeric()) {
        return Err("username must start with a letter or number".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usernames_are_identifier_shaped() {
        assert!(validate_username("ryan").is_ok());
        assert!(validate_username("ryan.hein").is_ok());
        assert!(validate_username("a1_-.").is_ok());

        assert!(validate_username("r").is_err(), "too short");
        assert!(validate_username(&"a".repeat(40)).is_err(), "too long");
        assert!(validate_username("-leading").is_err());
        assert!(validate_username(".leading").is_err());
        // The ones that matter: anything that could be read as another kind of
        // thing further down — a path, an email, a shell word.
        assert!(validate_username("has space").is_err());
        assert!(validate_username("a/b").is_err());
        assert!(validate_username("a@b").is_err());
        assert!(validate_username("../etc").is_err());
    }

    #[test]
    fn auth_mode_round_trips() {
        for m in [AuthMode::Oidc, AuthMode::Local] {
            assert_eq!(AuthMode::parse(m.as_str()), Some(m));
        }
        assert_eq!(AuthMode::parse("saml"), None);
        assert_eq!(AuthMode::parse(""), None);
    }
}
