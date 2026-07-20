//! Identity upsert + tenant bootstrap.
//!
//! Policy (milestone 1):
//! - first identity ever seen on the instance creates the default tenant and
//!   becomes its owner;
//! - afterwards, AUTO_JOIN_DEFAULT_TENANT=true (dev default) attaches new
//!   identities to the oldest tenant as members; false rejects them
//!   (invitations are post-M1).

use nook_types::{IdentityId, Tenant, TenantId, User, UserId};
use serde_json::Value;

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

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

    let (identity_count,): (i64,) = sqlx::query_as("SELECT count(*) FROM identities")
        .fetch_one(&state.db)
        .await?;

    let (tenant, role) = if identity_count == 0 {
        // Fresh instance: bootstrap the default tenant, first user owns it.
        // Seeds may have pre-created the tenant — adopt it rather than
        // creating a duplicate.
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
    } else if state.cfg.auto_join_default_tenant {
        let tenant: Tenant = sqlx::query_as("SELECT * FROM tenants ORDER BY created_at LIMIT 1")
            .fetch_one(&state.db)
            .await?;
        (tenant, "member")
    } else {
        return Err(ApiError::Forbidden);
    };

    // Same email already present in the tenant (e.g. relinked IdP): attach the
    // new identity to that user instead of creating a duplicate.
    let user: Option<User> =
        sqlx::query_as("SELECT * FROM users WHERE tenant_id = $1 AND email = $2")
            .bind(tenant.id)
            .bind(&email)
            .fetch_optional(&state.db)
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

    Ok((user, tenant))
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
    use super::slugify;

    #[test]
    fn slugs_are_url_safe_and_stable() {
        assert_eq!(slugify("My Team's Space"), "my-team-s-space");
        assert_eq!(slugify("dev"), "dev");
        assert_eq!(slugify("  --  "), "tenant");
        assert_eq!(slugify("Ünïcode Nämé"), "n-code-n-m"); // ascii-only by design
    }
}
