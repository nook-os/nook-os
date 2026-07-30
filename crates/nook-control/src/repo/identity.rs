//! Identity, tenant and person data access (MAIN-246).
//!
//! Everything the identity/auth surface reads or writes about **users,
//! tenants, people, identities, memberships, credentials and auth sessions**
//! lives behind [`IdentityRepository`]. Before this the same fifty-odd queries
//! were inlined across `services/identity.rs`, `services/local_auth.rs`,
//! `auth/mod.rs`, `auth/perm.rs` and `auth/session_guard.rs`, so a change to how
//! membership is stored meant finding every one of them.
//!
//! **Methods are intent-named and coarse.** There is no `query(sql)` escape and
//! no `sqlx` type in any signature — those would hand the abstraction straight
//! back. Where the old code leaned on a driver detail, the trait states the
//! intent instead: [`IdentityRepository::create_tenant`] returns `Ok(None)` for
//! a taken slug rather than a unique-violation for the caller to match on, and
//! [`IdentityRepository::claim_auth_mode`] returns "did we set it" rather than a
//! nullable row.
//!
//! **One impl over the engine-agnostic `DbPool`** ([`DbIdentityRepository`]),
//! with row mapping inside it. There is no per-engine branch here and no
//! dialect dispatch: that layer is underneath us (MAIN-189), and a per-engine
//! impl is a later, hotspot-proven escape hatch (NG-1, NG-3). The `Postgres.now()`
//! and `Postgres.cast()` calls that came in with the moved SQL are unchanged —
//! they were already there, and replacing them is the dialect sweep's job, not
//! this card's.
//!
//! **Multi-table writes of this aggregate stay inside one method.** Creating a
//! local account writes `users` *and* `tenant_members`; both happen in
//! [`IdentityRepository::create_local_user`], so no caller can perform half of
//! it. There are no cross-repository transaction semantics — deliberately, since
//! that would be a distributed-transaction problem dressed as a refactor.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use nook_db::{params, CiMatch, Db, DbPool, Json, Postgres, TypeMapping};
use nook_types::{
    AuthSessionId, IdentityId, Tenant, TenantId, TenantMemberItem, TenantMemberPage, User, UserId,
};
use serde_json::Value;
use uuid::Uuid;

use crate::error::ApiResult;

/// One reachable tenant for a person, as stored. The `current` flag is a view
/// concern and is applied by the caller, so this is the row and nothing more.
#[derive(Debug, Clone)]
pub struct MembershipRow {
    pub tenant_id: TenantId,
    pub name: String,
    pub slug: String,
    pub role: String,
    pub created_at: DateTime<Utc>,
}

/// What a new OIDC-sourced user is made from.
#[derive(Debug, Clone)]
pub struct NewOidcUser {
    pub tenant: TenantId,
    pub display_name: String,
    pub email: String,
    pub avatar_url: Option<String>,
    pub role: String,
}

/// What a new identity row is made from.
#[derive(Debug, Clone)]
pub struct NewIdentity {
    pub user_id: UserId,
    pub issuer: String,
    pub subject: String,
    pub email: Option<String>,
    pub raw_claims: Value,
    /// Stamps `email_verified_at` **only** when true — the invariant that
    /// "verified" means a real timestamp from a real claim, never an email
    /// string that happens to look right (MAIN-29).
    pub email_verified: bool,
}

/// What a new local (password) account is made from.
#[derive(Debug, Clone)]
pub struct NewLocalUser {
    pub tenant: TenantId,
    pub display_name: String,
    pub email: String,
    pub username: String,
    pub password_hash: String,
    pub role: String,
    /// Whether to write the `tenant_members` grant with the row.
    ///
    /// Explicit, because the two callers genuinely differ and collapsing them
    /// would be a silent behaviour change: self-service `create` grants
    /// membership (without it the account is locked out of its own tenant),
    /// while `register_invited` does not — accepting the invite is what grants
    /// it, and granting here too would admit someone whose invite was never
    /// consumed.
    pub grant_membership: bool,
}

/// Why a local account could not be created. Named causes rather than a driver
/// error, so callers map them to HTTP without knowing what a constraint is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateUserError {
    UsernameTaken,
    EmailTaken,
}

#[async_trait]
pub trait IdentityRepository: Send + Sync {
    // ---- people, and the tenants they reach -------------------------------

    /// Every tenant this person can reach, oldest first. Correlated by
    /// `person_id`, never email (MAIN-12).
    async fn memberships_of(&self, user_id: UserId) -> ApiResult<Vec<MembershipRow>>;

    /// Every `users` row belonging to the same person, including this one.
    async fn sibling_user_ids(&self, user_id: UserId) -> ApiResult<Vec<UserId>>;

    /// This person's `users` row in `target`, but only if they belong there.
    async fn user_in_tenant(&self, user_id: UserId, target: TenantId) -> ApiResult<Option<UserId>>;

    /// Does a live `tenant_members` grant exist? Checked per request, so
    /// revoking a grant takes effect immediately rather than at next logout.
    async fn has_active_membership(&self, user_id: UserId, tenant: TenantId) -> ApiResult<bool>;

    /// Idempotent grant. Written even for a personal tenant, so "which tenants
    /// can this user reach" has exactly one answer to read.
    async fn grant_membership(&self, tenant: TenantId, user: UserId, role: &str) -> ApiResult<()>;

    /// One keyset page of a tenant's members.
    async fn members_page(
        &self,
        tenant: TenantId,
        q: Option<String>,
        after: Option<Uuid>,
        limit: i64,
    ) -> ApiResult<TenantMemberPage>;

    /// The person behind a user row — the cross-tenant identity that outlives
    /// any single membership.
    async fn person_id_of(&self, user_id: UserId) -> ApiResult<Option<Uuid>>;

    // ---- users and tenants -------------------------------------------------

    async fn get_user(&self, user_id: UserId) -> ApiResult<Option<User>>;
    async fn get_tenant(&self, tenant: TenantId) -> ApiResult<Option<Tenant>>;
    async fn tenant_by_slug(&self, slug: &str) -> ApiResult<Option<Tenant>>;
    async fn user_by_email_in_tenant(
        &self,
        tenant: TenantId,
        email: &str,
    ) -> ApiResult<Option<User>>;

    /// "Is this instance empty?" is a question about **people**, and only
    /// `users` knows: an instance bootstrapped with a local account has zero
    /// identities but is not empty (the bug this counts around).
    async fn count_users(&self) -> ApiResult<i64>;

    /// `Ok(None)` means the slug is taken — the caller retries with another
    /// rather than matching on a driver error.
    async fn create_tenant(&self, name: &str, slug: &str) -> ApiResult<Option<Tenant>>;

    async fn create_oidc_user(&self, new: NewOidcUser) -> ApiResult<User>;

    /// A user's role in one tenant, for the admin check.
    async fn role_in_tenant(&self, user_id: UserId, tenant: TenantId) -> ApiResult<Option<String>>;

    /// The tenant's owner (falling back to its oldest user) — who a machine
    /// credential acts as for attribution.
    async fn tenant_owner_user_id(&self, tenant: Uuid) -> ApiResult<Option<Uuid>>;

    /// The org a tenant belongs to, if any.
    async fn org_of(&self, tenant: TenantId) -> ApiResult<Option<Uuid>>;

    /// The instance's first tenant, and a tenant's owner — what an MCP token
    /// maps to until per-user MCP OAuth exists.
    async fn first_tenant(&self) -> ApiResult<Option<TenantId>>;
    async fn first_user(&self, tenant: TenantId) -> ApiResult<Option<UserId>>;

    // ---- identities (OIDC + local verification) ----------------------------

    async fn user_id_by_identity(&self, issuer: &str, subject: &str) -> ApiResult<Option<UserId>>;

    /// True only when the user holds an identity carrying a real verification
    /// timestamp — never derived from an email string.
    async fn email_is_verified(&self, user_id: UserId) -> ApiResult<bool>;

    /// Record that a returning identity has become verified. Never clears:
    /// verification moves one way, and only from a true claim.
    async fn mark_identity_verified(&self, issuer: &str, subject: &str) -> ApiResult<()>;

    /// A local account has no identity of its own, so a completed local
    /// round-trip writes one. Idempotent — a second confirm keeps the first
    /// verification time.
    async fn mark_local_email_verified(&self, user_id: UserId, email: &str) -> ApiResult<()>;

    async fn create_identity(&self, new: NewIdentity) -> ApiResult<()>;

    // ---- local credentials -------------------------------------------------

    /// A tenant's committed sign-in method. `None` means nobody has signed in.
    async fn auth_mode_of(&self, tenant: TenantId) -> ApiResult<Option<String>>;

    /// Commit a tenant to one method. `true` means **we** set it; `false` means
    /// somebody already had. A conditional UPDATE rather than read-then-write,
    /// so two simultaneous first sign-ins cannot each set their own answer and
    /// silently lock the loser out.
    async fn claim_auth_mode(&self, tenant: TenantId, mode: &str) -> ApiResult<bool>;

    /// `(user_id, password_hash)` for a username-or-email identifier. The hash
    /// is `None` for an OIDC account, which the caller must not distinguish
    /// from "no such user" in its response.
    async fn credentials_for(
        &self,
        tenant: TenantId,
        identifier: &str,
    ) -> ApiResult<Option<(UserId, Option<String>)>>;

    /// Creates the `users` row **and** its `tenant_members` grant. Both, or the
    /// account is one a person cannot reach.
    async fn create_local_user(
        &self,
        new: NewLocalUser,
    ) -> ApiResult<Result<User, CreateUserError>>;

    /// `Some(None)` is a user with no password (OIDC); `None` is no such user.
    async fn password_hash_of(&self, user_id: UserId) -> ApiResult<Option<Option<String>>>;
    async fn set_password_hash(&self, user_id: UserId, hash: &str) -> ApiResult<()>;

    // ---- sessions and authorization ---------------------------------------

    async fn create_auth_session(
        &self,
        id: AuthSessionId,
        user_id: UserId,
        tenant: TenantId,
        ttl_hours: i32,
    ) -> ApiResult<()>;

    /// Does this user hold `permission` at the scope, or any ancestor of it?
    /// Resolved in one query so "covers" has a single definition.
    async fn has_permission(
        &self,
        user_id: UserId,
        permission: &str,
        org_id: Option<Uuid>,
        tenant_id: Option<Uuid>,
    ) -> ApiResult<bool>;

    // ---- node reads that authorization needs -------------------------------
    //
    // These read `nodes`, not identity — they are here because they are
    // authorization decisions made in the owned files, and AC-4 requires those
    // files to hold no queries. MAIN-252's `NodeRepository` is the natural
    // eventual home; grouped and named so moving them later is mechanical.

    /// `None` = no such node; `Some(None)` = a node nobody owns.
    async fn node_owner_person(
        &self,
        node_id: Uuid,
        tenant: TenantId,
    ) -> ApiResult<Option<Option<Uuid>>>;

    async fn node_owner_and_shared(
        &self,
        node_id: Uuid,
        tenant: TenantId,
    ) -> ApiResult<Option<(Option<Uuid>, bool)>>;

    /// `(node_id, tenant_id)` for a presented node token hash.
    async fn node_by_token_hash(&self, hash: &str) -> ApiResult<Option<(Uuid, Uuid)>>;
}

/// The real implementation, over the engine-agnostic pool.
pub struct DbIdentityRepository {
    db: DbPool,
}

impl DbIdentityRepository {
    pub fn new(db: DbPool) -> Self {
        Self { db }
    }
}

#[async_trait]
impl IdentityRepository for DbIdentityRepository {
    async fn memberships_of(&self, user_id: UserId) -> ApiResult<Vec<MembershipRow>> {
        let rows: Vec<(TenantId, String, String, String, DateTime<Utc>)> = self
            .db
            .query_all(
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
                params![user_id],
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|(tenant_id, name, slug, role, created_at)| MembershipRow {
                tenant_id,
                name,
                slug,
                role,
                created_at,
            })
            .collect())
    }

    async fn sibling_user_ids(&self, user_id: UserId) -> ApiResult<Vec<UserId>> {
        Ok(self
            .db
            .query_scalar_all(
                "SELECT u.id FROM users me JOIN users u ON u.person_id = me.person_id WHERE me.id = $1",
                params![user_id],
            )
            .await
            .unwrap_or_default())
    }

    async fn user_in_tenant(&self, user_id: UserId, target: TenantId) -> ApiResult<Option<UserId>> {
        Ok(self
            .db
            .query_scalar_opt(
                "SELECT u.id
         FROM users me
         JOIN users u ON u.person_id = me.person_id
         JOIN tenant_members tm
             ON tm.tenant_id = u.tenant_id
            AND tm.principal_type = 'user'
            AND tm.principal_id = u.id
         WHERE me.id = $1 AND u.tenant_id = $2
         LIMIT 1",
                params![user_id, target],
            )
            .await?)
    }

    async fn has_active_membership(&self, user_id: UserId, tenant: TenantId) -> ApiResult<bool> {
        let row: Option<i32> = self
            .db
            .query_scalar_opt(
                "SELECT 1 FROM tenant_members
         WHERE tenant_id = $1 AND principal_type = 'user' AND principal_id = $2
         LIMIT 1",
                params![tenant, user_id],
            )
            .await?;
        Ok(row.is_some())
    }

    async fn grant_membership(&self, tenant: TenantId, user: UserId, role: &str) -> ApiResult<()> {
        self.db
            .exec(
                "INSERT INTO tenant_members (id, tenant_id, principal_type, principal_id, role)
         VALUES ($1, $2, 'user', $3, $4)
         ON CONFLICT (tenant_id, principal_type, principal_id) DO NOTHING",
                params![Uuid::now_v7(), tenant, user.0, role],
            )
            .await?;
        Ok(())
    }

    async fn members_page(
        &self,
        tenant: TenantId,
        q: Option<String>,
        after: Option<Uuid>,
        limit: i64,
    ) -> ApiResult<TenantMemberPage> {
        let limit = limit.clamp(1, 200);
        let q = crate::services::core::search_filter(q);
        let term = Postgres.cast("$3", "text");
        let rows: Vec<TenantMemberItem> = self
            .db
            .query_all(
                &format!(
                    "SELECT m.principal_id, u.email, u.display_name, m.role, m.created_at AS joined_at
         FROM tenant_members m
         JOIN users u ON u.id = m.principal_id
         WHERE m.tenant_id = $1 AND m.principal_type = 'user'
           AND ({term} IS NULL OR (
                    {m_email}
                 OR {m_name}
                 OR {m_role}))
           AND ({cursor} IS NULL OR m.principal_id < $4)
         ORDER BY m.principal_id DESC
         LIMIT $2",
                    cursor = Postgres.cast("$4", "uuid"),
                    m_email = Postgres.ci_match("u.email", "'%' || $3 || '%'"),
                    m_name = Postgres.ci_match("u.display_name", "'%' || $3 || '%'"),
                    m_role = Postgres.ci_match("m.role", "'%' || $3 || '%'")
                ),
                params![tenant, limit, q, after],
            )
            .await?;
        let next_cursor = if rows.len() as i64 == limit {
            rows.last().map(|r| r.principal_id)
        } else {
            None
        };
        Ok(TenantMemberPage { rows, next_cursor })
    }

    async fn person_id_of(&self, user_id: UserId) -> ApiResult<Option<Uuid>> {
        Ok(self
            .db
            .query_scalar_opt(
                "SELECT person_id FROM users WHERE id = $1",
                params![user_id],
            )
            .await?)
    }

    async fn get_user(&self, user_id: UserId) -> ApiResult<Option<User>> {
        Ok(self
            .db
            .query_opt("SELECT * FROM users WHERE id = $1", params![user_id])
            .await?)
    }

    async fn get_tenant(&self, tenant: TenantId) -> ApiResult<Option<Tenant>> {
        Ok(self
            .db
            .query_opt("SELECT * FROM tenants WHERE id = $1", params![tenant])
            .await?)
    }

    async fn tenant_by_slug(&self, slug: &str) -> ApiResult<Option<Tenant>> {
        Ok(self
            .db
            .query_opt("SELECT * FROM tenants WHERE slug = $1", params![slug])
            .await?)
    }

    async fn user_by_email_in_tenant(
        &self,
        tenant: TenantId,
        email: &str,
    ) -> ApiResult<Option<User>> {
        Ok(self
            .db
            .query_opt(
                "SELECT * FROM users WHERE tenant_id = $1 AND email = $2",
                params![tenant, email],
            )
            .await?)
    }

    async fn count_users(&self) -> ApiResult<i64> {
        Ok(self
            .db
            .query_scalar("SELECT count(*) FROM users", params![])
            .await?)
    }

    async fn create_tenant(&self, name: &str, slug: &str) -> ApiResult<Option<Tenant>> {
        let res: Result<Tenant, sqlx::Error> = self
            .db
            .query_one(
                "INSERT INTO tenants (id, name, slug) VALUES ($1, $2, $3) RETURNING *",
                params![TenantId::new(), name, slug],
            )
            .await;
        match res {
            Ok(t) => Ok(Some(t)),
            // A taken slug is an ordinary outcome the caller retries around, not
            // an error — so the driver detail stops here rather than travelling
            // out through the trait.
            Err(sqlx::Error::Database(d)) if d.is_unique_violation() => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn create_oidc_user(&self, new: NewOidcUser) -> ApiResult<User> {
        Ok(self
            .db
            .query_one(
                "INSERT INTO users (id, tenant_id, display_name, email, avatar_url, role)
                 VALUES ($1, $2, $3, $4, $5, $6) RETURNING *",
                params![
                    UserId::new(),
                    new.tenant,
                    new.display_name,
                    new.email,
                    new.avatar_url,
                    new.role
                ],
            )
            .await?)
    }

    async fn role_in_tenant(&self, user_id: UserId, tenant: TenantId) -> ApiResult<Option<String>> {
        Ok(self
            .db
            .query_scalar_opt(
                "SELECT role FROM users WHERE id = $1 AND tenant_id = $2",
                params![user_id, tenant],
            )
            .await?)
    }

    async fn tenant_owner_user_id(&self, tenant: Uuid) -> ApiResult<Option<Uuid>> {
        Ok(self
            .db
            .query_scalar_opt(
                "SELECT id FROM users WHERE tenant_id = $1
             ORDER BY (role = 'owner') DESC, created_at LIMIT 1",
                params![tenant],
            )
            .await?)
    }

    async fn org_of(&self, tenant: TenantId) -> ApiResult<Option<Uuid>> {
        let row: Option<(Option<Uuid>,)> = self
            .db
            .query_opt("SELECT org_id FROM tenants WHERE id = $1", params![tenant])
            .await?;
        Ok(row.and_then(|(o,)| o))
    }

    async fn first_tenant(&self) -> ApiResult<Option<TenantId>> {
        Ok(self
            .db
            .query_scalar_opt(
                "SELECT id FROM tenants ORDER BY created_at LIMIT 1",
                params![],
            )
            .await?)
    }

    async fn first_user(&self, tenant: TenantId) -> ApiResult<Option<UserId>> {
        Ok(self
            .db
            .query_scalar_opt(
                "SELECT id FROM users WHERE tenant_id = $1 ORDER BY created_at LIMIT 1",
                params![tenant],
            )
            .await?)
    }

    async fn user_id_by_identity(&self, issuer: &str, subject: &str) -> ApiResult<Option<UserId>> {
        Ok(self
            .db
            .query_scalar_opt(
                "SELECT user_id FROM identities WHERE issuer = $1 AND subject = $2",
                params![issuer, subject],
            )
            .await?)
    }

    async fn email_is_verified(&self, user_id: UserId) -> ApiResult<bool> {
        Ok(self
            .db
            .query_scalar(
                "SELECT EXISTS (
             SELECT 1 FROM identities
             WHERE user_id = $1 AND email_verified_at IS NOT NULL
         )",
                params![user_id],
            )
            .await?)
    }

    async fn mark_identity_verified(&self, issuer: &str, subject: &str) -> ApiResult<()> {
        self.db
            .exec(
                &format!(
                    "UPDATE identities SET email_verified_at = {}
                 WHERE issuer = $1 AND subject = $2 AND email_verified_at IS NULL",
                    Postgres.now()
                ),
                params![issuer, subject],
            )
            .await?;
        Ok(())
    }

    async fn mark_local_email_verified(&self, user_id: UserId, email: &str) -> ApiResult<()> {
        // The static raw_claims literal routes through the json seam (MAIN-201).
        let sql = format!(
            "INSERT INTO identities (id, user_id, issuer, subject, email, raw_claims, email_verified_at)
         VALUES ($1, $2, 'local', $3, $4, {}, {now})
         ON CONFLICT (issuer, subject)
           DO UPDATE SET email_verified_at = COALESCE(identities.email_verified_at, {now})",
            Postgres.literal("{\"verified_via\":\"local\"}"),
            now = Postgres.now()
        );
        self.db
            .exec(
                &sql,
                params![Uuid::now_v7(), user_id, user_id.0.to_string(), email],
            )
            .await?;
        Ok(())
    }

    async fn create_identity(&self, new: NewIdentity) -> ApiResult<()> {
        self.db
            .exec(
                &format!(
                    "INSERT INTO identities (id, user_id, issuer, subject, email, raw_claims, email_verified_at)
         VALUES ($1, $2, $3, $4, $5, $6, CASE WHEN $7 THEN {} ELSE NULL END)",
                    Postgres.now()
                ),
                params![
                    IdentityId::new(),
                    new.user_id,
                    new.issuer,
                    new.subject,
                    new.email,
                    &new.raw_claims,
                    new.email_verified
                ],
            )
            .await?;
        Ok(())
    }

    async fn auth_mode_of(&self, tenant: TenantId) -> ApiResult<Option<String>> {
        let row: Option<(Option<String>,)> = self
            .db
            .query_opt(
                "SELECT auth_mode FROM tenants WHERE id = $1",
                params![tenant],
            )
            .await?;
        Ok(row.and_then(|(m,)| m))
    }

    async fn claim_auth_mode(&self, tenant: TenantId, mode: &str) -> ApiResult<bool> {
        let settled: Option<(Option<String>,)> = self
            .db
            .query_opt(
                "UPDATE tenants SET auth_mode = $2 WHERE id = $1 AND auth_mode IS NULL
         RETURNING auth_mode",
                params![tenant, mode],
            )
            .await?;
        Ok(settled.is_some())
    }

    async fn credentials_for(
        &self,
        tenant: TenantId,
        identifier: &str,
    ) -> ApiResult<Option<(UserId, Option<String>)>> {
        Ok(self
            .db
            .query_opt(
                "SELECT id, password_hash FROM users
         WHERE tenant_id = $1
           AND (lower(username) = lower($2) OR lower(email) = lower($2))",
                params![tenant, identifier],
            )
            .await?)
    }

    async fn create_local_user(
        &self,
        new: NewLocalUser,
    ) -> ApiResult<Result<User, CreateUserError>> {
        let role = new.role.clone();
        let tenant = new.tenant;
        let user: User = match self
            .db
            .query_one(
                "INSERT INTO users (id, tenant_id, display_name, email, username, password_hash, role)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         RETURNING *",
                params![
                    UserId::new(),
                    new.tenant,
                    new.display_name,
                    new.email,
                    new.username,
                    new.password_hash,
                    new.role
                ],
            )
            .await
        {
            Ok(u) => u,
            // Which uniqueness failed is the only thing the caller needs, so it
            // is decided here — the constraint name never leaves this file.
            Err(sqlx::Error::Database(d)) if d.is_unique_violation() => {
                let taken = d.constraint().unwrap_or_default();
                return Ok(Err(if taken.contains("username") {
                    CreateUserError::UsernameTaken
                } else {
                    CreateUserError::EmailTaken
                }));
            }
            Err(e) => return Err(e.into()),
        };

        // The grant, in the same method as the row: `tenant_members` is the
        // single source of truth for who can reach a tenant, and a local user
        // missing it is locked out of their own.
        // Only when the caller asked. `create` grants (without it a
        // self-service account is locked out of its own tenant);
        // `register_invited` does not, because accepting the invite is what
        // grants it. Making this unconditional silently admitted every invitee
        // at registration — caught by the existing invite tests.
        if new.grant_membership {
            self.grant_membership(tenant, user.id, &role).await?;
        }
        Ok(Ok(user))
    }

    async fn password_hash_of(&self, user_id: UserId) -> ApiResult<Option<Option<String>>> {
        let row: Option<(Option<String>,)> = self
            .db
            .query_opt(
                "SELECT password_hash FROM users WHERE id = $1",
                params![user_id],
            )
            .await?;
        Ok(row.map(|(h,)| h))
    }

    async fn set_password_hash(&self, user_id: UserId, hash: &str) -> ApiResult<()> {
        self.db
            .exec(
                &format!(
                    "UPDATE users SET password_hash = $2, updated_at = {} WHERE id = $1",
                    Postgres.now()
                ),
                params![user_id, hash],
            )
            .await?;
        Ok(())
    }

    async fn create_auth_session(
        &self,
        id: AuthSessionId,
        user_id: UserId,
        tenant: TenantId,
        ttl_hours: i32,
    ) -> ApiResult<()> {
        self.db
            .exec(
                &format!(
                    "INSERT INTO sessions_auth (id, user_id, tenant_id, expires_at)
         VALUES ($1, $2, $3, {now} + make_interval(hours => $4))",
                    now = Postgres.now()
                ),
                params![id, user_id, tenant, ttl_hours],
            )
            .await?;
        Ok(())
    }

    async fn has_permission(
        &self,
        user_id: UserId,
        permission: &str,
        org_id: Option<Uuid>,
        tenant_id: Option<Uuid>,
    ) -> ApiResult<bool> {
        let hit: Option<(bool,)> = self
            .db
            .query_opt(
                "SELECT true
             FROM role_bindings b
             JOIN role_permissions rp ON rp.role_key = b.role_key
             WHERE b.subject_type = 'user'
               AND b.subject_id = $1
               AND rp.permission_key = $2
               AND (
                     -- Deployment covers everything below it.
                     b.scope_type = 'deployment'
                     -- The org itself, or the org the target tenant lives in.
                  OR (b.scope_type = 'org' AND b.scope_id = $3)
                     -- The exact tenant.
                  OR (b.scope_type = 'tenant' AND b.scope_id = $4)
               )
             LIMIT 1",
                params![user_id.0, permission, org_id, tenant_id],
            )
            .await?;
        Ok(hit.is_some())
    }

    async fn node_owner_person(
        &self,
        node_id: Uuid,
        tenant: TenantId,
    ) -> ApiResult<Option<Option<Uuid>>> {
        Ok(self
            .db
            .query_scalar_opt(
                "SELECT owner_person_id FROM nodes WHERE id = $1 AND tenant_id = $2",
                params![node_id, tenant],
            )
            .await?)
    }

    async fn node_owner_and_shared(
        &self,
        node_id: Uuid,
        tenant: TenantId,
    ) -> ApiResult<Option<(Option<Uuid>, bool)>> {
        Ok(self
            .db
            .query_opt(
                "SELECT owner_person_id, shared FROM nodes WHERE id = $1 AND tenant_id = $2",
                params![node_id, tenant],
            )
            .await?)
    }

    async fn node_by_token_hash(&self, hash: &str) -> ApiResult<Option<(Uuid, Uuid)>> {
        Ok(self
            .db
            .query_opt(
                "SELECT id, tenant_id FROM nodes WHERE node_token_hash = $1",
                params![hash],
            )
            .await?)
    }
}

/// An in-memory [`IdentityRepository`] for tests that should not need a
/// database (MAIN-246 AC-3).
///
/// Faithful where the behaviour under test lives — membership grants, the
/// slug-uniqueness that drives the personal-tenant retry, the auth-mode claim
/// race, verified-email being a timestamp rather than an email string — and
/// deliberately simple elsewhere. It is a test double, not a second
/// implementation of Postgres: `members_page` sorts and pages but does not
/// reproduce `ILIKE` collation, and nothing here enforces foreign keys.
///
/// Everything is behind one `Mutex` because tests are not contended and a lock
/// per table would invite a deadlock that only shows up in CI.
#[derive(Default)]
pub struct FakeIdentityRepository {
    inner: std::sync::Mutex<FakeState>,
}

#[derive(Default)]
struct FakeState {
    users: Vec<User>,
    /// `person_id` and `username` are real columns that the `User` DTO does not
    /// carry, so the fake keeps them beside it rather than inventing a second
    /// user type. Both are load-bearing here: person is what correlates a human
    /// across tenants, and username is half of the login identifier.
    person_of: std::collections::HashMap<UserId, Uuid>,
    username_of: std::collections::HashMap<UserId, String>,
    tenants: Vec<Tenant>,
    /// `(tenant, user, role)`
    members: Vec<(TenantId, UserId, String)>,
    /// `(issuer, subject, user_id, verified)`
    identities: Vec<(String, String, UserId, bool)>,
    /// `(user, permission, org, tenant)` — a binding that already "covers".
    permissions: Vec<(UserId, String, Option<Uuid>, Option<Uuid>)>,
    /// `(node_id, tenant, owner_person, shared, token_hash)`
    nodes: Vec<(Uuid, TenantId, Option<Uuid>, bool, String)>,
    auth_modes: std::collections::HashMap<TenantId, String>,
    passwords: std::collections::HashMap<UserId, Option<String>>,
    auth_sessions: Vec<(AuthSessionId, UserId, TenantId)>,
}

impl FakeIdentityRepository {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed a tenant. Returns it so a test reads as a story rather than as
    /// fixture bookkeeping.
    pub fn with_tenant(&self, name: &str, slug: &str) -> Tenant {
        let mut st = self.inner.lock().unwrap();
        let t = new_tenant(name, slug);
        st.tenants.push(t.clone());
        t
    }

    /// Seed a user, with the `tenant_members` grant a real sign-in would write.
    pub fn with_user(&self, tenant: TenantId, email: &str, role: &str) -> User {
        let mut st = self.inner.lock().unwrap();
        let u = new_user(
            tenant,
            email.split('@').next().unwrap_or("user"),
            email,
            None,
            role,
        );
        st.person_of.insert(u.id, Uuid::now_v7());
        st.users.push(u.clone());
        st.members.push((tenant, u.id, role.into()));
        u
    }

    /// Make two user rows the same person — how one human signing into two
    /// tenants looks in the schema. `person_id` is a column the `User` DTO does
    /// not carry, so this is the seam a test uses instead of reaching into the
    /// struct.
    pub fn link_person(&self, a: UserId, b: UserId) {
        let mut st = self.inner.lock().unwrap();
        if let Some(p) = st.person_of.get(&a).copied() {
            st.person_of.insert(b, p);
        }
    }

    /// Revoke a grant without deleting the user — the case
    /// `has_active_membership` exists to catch.
    pub fn revoke_membership(&self, tenant: TenantId, user: UserId) {
        let mut st = self.inner.lock().unwrap();
        st.members.retain(|(t, u, _)| !(*t == tenant && *u == user));
    }

    /// Grant a permission at a scope, as a role binding would.
    pub fn with_permission(
        &self,
        user: UserId,
        permission: &str,
        org: Option<Uuid>,
        tenant: Option<Uuid>,
    ) {
        self.inner
            .lock()
            .unwrap()
            .permissions
            .push((user, permission.into(), org, tenant));
    }

    /// Seed a node for the ownership checks.
    pub fn with_node(
        &self,
        tenant: TenantId,
        owner: Option<Uuid>,
        shared: bool,
        token_hash: &str,
    ) -> Uuid {
        let id = Uuid::now_v7();
        self.inner
            .lock()
            .unwrap()
            .nodes
            .push((id, tenant, owner, shared, token_hash.into()));
        id
    }

    /// How many auth sessions were opened — so a test can assert the login path
    /// actually issued one.
    pub fn auth_session_count(&self) -> usize {
        self.inner.lock().unwrap().auth_sessions.len()
    }
}

#[async_trait]
impl IdentityRepository for FakeIdentityRepository {
    async fn memberships_of(&self, user_id: UserId) -> ApiResult<Vec<MembershipRow>> {
        let st = self.inner.lock().unwrap();
        let Some(me) = st.person_of.get(&user_id).copied() else {
            return Ok(vec![]);
        };
        let mut out: Vec<MembershipRow> = st
            .users
            .iter()
            .filter(|u| st.person_of.get(&u.id).copied() == Some(me))
            .filter_map(|u| {
                let role = st
                    .members
                    .iter()
                    .find(|(t, mu, _)| *t == u.tenant_id && *mu == u.id)
                    .map(|(_, _, r)| r.clone())?;
                let t = st.tenants.iter().find(|t| t.id == u.tenant_id)?;
                Some(MembershipRow {
                    tenant_id: t.id,
                    name: t.name.clone(),
                    slug: t.slug.clone(),
                    role,
                    created_at: t.created_at,
                })
            })
            .collect();
        out.sort_by_key(|r| r.created_at);
        Ok(out)
    }

    async fn sibling_user_ids(&self, user_id: UserId) -> ApiResult<Vec<UserId>> {
        let st = self.inner.lock().unwrap();
        let Some(me) = st.person_of.get(&user_id).copied() else {
            return Ok(vec![]);
        };
        Ok(st
            .users
            .iter()
            .filter(|u| st.person_of.get(&u.id).copied() == Some(me))
            .map(|u| u.id)
            .collect())
    }

    async fn user_in_tenant(&self, user_id: UserId, target: TenantId) -> ApiResult<Option<UserId>> {
        let st = self.inner.lock().unwrap();
        let Some(me) = st.person_of.get(&user_id).copied() else {
            return Ok(None);
        };
        Ok(st
            .users
            .iter()
            .find(|u| {
                st.person_of.get(&u.id).copied() == Some(me)
                    && u.tenant_id == target
                    && st
                        .members
                        .iter()
                        .any(|(t, mu, _)| *t == target && *mu == u.id)
            })
            .map(|u| u.id))
    }

    async fn has_active_membership(&self, user_id: UserId, tenant: TenantId) -> ApiResult<bool> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .members
            .iter()
            .any(|(t, u, _)| *t == tenant && *u == user_id))
    }

    async fn grant_membership(&self, tenant: TenantId, user: UserId, role: &str) -> ApiResult<()> {
        let mut st = self.inner.lock().unwrap();
        if !st
            .members
            .iter()
            .any(|(t, u, _)| *t == tenant && *u == user)
        {
            st.members.push((tenant, user, role.into()));
        }
        Ok(())
    }

    async fn members_page(
        &self,
        tenant: TenantId,
        q: Option<String>,
        after: Option<Uuid>,
        limit: i64,
    ) -> ApiResult<TenantMemberPage> {
        let limit = limit.clamp(1, 200);
        let st = self.inner.lock().unwrap();
        let q = q.map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty());
        let mut rows: Vec<TenantMemberItem> = st
            .members
            .iter()
            .filter(|(t, _, _)| *t == tenant)
            .filter_map(|(_, uid, role)| {
                let u = st.users.iter().find(|u| u.id == *uid)?;
                Some(TenantMemberItem {
                    principal_id: u.id.0,
                    email: u.email.clone(),
                    display_name: u.display_name.clone(),
                    role: role.clone(),
                    joined_at: u.created_at,
                })
            })
            .filter(|r| match &q {
                None => true,
                Some(q) => {
                    r.email.to_lowercase().contains(q)
                        || r.display_name.to_lowercase().contains(q)
                        || r.role.to_lowercase().contains(q)
                }
            })
            .collect();
        // Descending, matching the real query's `ORDER BY … DESC`.
        rows.sort_by_key(|r| std::cmp::Reverse(r.principal_id));
        if let Some(after) = after {
            rows.retain(|r| r.principal_id < after);
        }
        rows.truncate(limit as usize);
        let next_cursor = if rows.len() as i64 == limit {
            rows.last().map(|r| r.principal_id)
        } else {
            None
        };
        Ok(TenantMemberPage { rows, next_cursor })
    }

    async fn person_id_of(&self, user_id: UserId) -> ApiResult<Option<Uuid>> {
        let st = self.inner.lock().unwrap();
        Ok(st.person_of.get(&user_id).copied())
    }

    async fn get_user(&self, user_id: UserId) -> ApiResult<Option<User>> {
        let st = self.inner.lock().unwrap();
        Ok(st.users.iter().find(|u| u.id == user_id).cloned())
    }

    async fn get_tenant(&self, tenant: TenantId) -> ApiResult<Option<Tenant>> {
        let st = self.inner.lock().unwrap();
        Ok(st.tenants.iter().find(|t| t.id == tenant).cloned())
    }

    async fn tenant_by_slug(&self, slug: &str) -> ApiResult<Option<Tenant>> {
        let st = self.inner.lock().unwrap();
        Ok(st.tenants.iter().find(|t| t.slug == slug).cloned())
    }

    async fn user_by_email_in_tenant(
        &self,
        tenant: TenantId,
        email: &str,
    ) -> ApiResult<Option<User>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .users
            .iter()
            .find(|u| u.tenant_id == tenant && u.email == email)
            .cloned())
    }

    async fn count_users(&self) -> ApiResult<i64> {
        Ok(self.inner.lock().unwrap().users.len() as i64)
    }

    async fn create_tenant(&self, name: &str, slug: &str) -> ApiResult<Option<Tenant>> {
        let mut st = self.inner.lock().unwrap();
        // The uniqueness that makes the caller's retry loop real.
        if st.tenants.iter().any(|t| t.slug == slug) {
            return Ok(None);
        }
        let t = new_tenant(name, slug);
        st.tenants.push(t.clone());
        Ok(Some(t))
    }

    async fn create_oidc_user(&self, new: NewOidcUser) -> ApiResult<User> {
        let mut st = self.inner.lock().unwrap();
        let mut u = new_user(new.tenant, &new.display_name, &new.email, None, &new.role);
        u.avatar_url = new.avatar_url;
        st.person_of.insert(u.id, Uuid::now_v7());
        st.users.push(u.clone());
        Ok(u)
    }

    async fn role_in_tenant(&self, user_id: UserId, tenant: TenantId) -> ApiResult<Option<String>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .users
            .iter()
            .find(|u| u.id == user_id && u.tenant_id == tenant)
            .map(|u| u.role.clone()))
    }

    async fn tenant_owner_user_id(&self, tenant: Uuid) -> ApiResult<Option<Uuid>> {
        let st = self.inner.lock().unwrap();
        let mut users: Vec<&User> = st
            .users
            .iter()
            .filter(|u| u.tenant_id.0 == tenant)
            .collect();
        // Owners first, then oldest — the production ORDER BY.
        users.sort_by_key(|u| (u.role != "owner", u.created_at));
        Ok(users.first().map(|u| u.id.0))
    }

    async fn org_of(&self, _tenant: TenantId) -> ApiResult<Option<Uuid>> {
        Ok(None)
    }

    async fn first_tenant(&self) -> ApiResult<Option<TenantId>> {
        let st = self.inner.lock().unwrap();
        let mut ts: Vec<&Tenant> = st.tenants.iter().collect();
        ts.sort_by_key(|t| t.created_at);
        Ok(ts.first().map(|t| t.id))
    }

    async fn first_user(&self, tenant: TenantId) -> ApiResult<Option<UserId>> {
        let st = self.inner.lock().unwrap();
        let mut us: Vec<&User> = st.users.iter().filter(|u| u.tenant_id == tenant).collect();
        us.sort_by_key(|u| u.created_at);
        Ok(us.first().map(|u| u.id))
    }

    async fn user_id_by_identity(&self, issuer: &str, subject: &str) -> ApiResult<Option<UserId>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .identities
            .iter()
            .find(|(i, s, _, _)| i == issuer && s == subject)
            .map(|(_, _, u, _)| *u))
    }

    async fn email_is_verified(&self, user_id: UserId) -> ApiResult<bool> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .identities
            .iter()
            .any(|(_, _, u, verified)| *u == user_id && *verified))
    }

    async fn mark_identity_verified(&self, issuer: &str, subject: &str) -> ApiResult<()> {
        let mut st = self.inner.lock().unwrap();
        for (i, s, _, v) in st.identities.iter_mut() {
            if i == issuer && s == subject {
                *v = true; // one-way, exactly as the SQL's `IS NULL` guard is
            }
        }
        Ok(())
    }

    async fn mark_local_email_verified(&self, user_id: UserId, _email: &str) -> ApiResult<()> {
        let mut st = self.inner.lock().unwrap();
        let subject = user_id.0.to_string();
        if let Some(row) = st
            .identities
            .iter_mut()
            .find(|(i, s, _, _)| i == "local" && *s == subject)
        {
            row.3 = true;
        } else {
            st.identities.push(("local".into(), subject, user_id, true));
        }
        Ok(())
    }

    async fn create_identity(&self, new: NewIdentity) -> ApiResult<()> {
        self.inner.lock().unwrap().identities.push((
            new.issuer,
            new.subject,
            new.user_id,
            new.email_verified,
        ));
        Ok(())
    }

    async fn auth_mode_of(&self, tenant: TenantId) -> ApiResult<Option<String>> {
        Ok(self.inner.lock().unwrap().auth_modes.get(&tenant).cloned())
    }

    async fn claim_auth_mode(&self, tenant: TenantId, mode: &str) -> ApiResult<bool> {
        let mut st = self.inner.lock().unwrap();
        // Set-if-absent under one lock, which is the whole point of the
        // conditional UPDATE it stands in for.
        if st.auth_modes.contains_key(&tenant) {
            return Ok(false);
        }
        st.auth_modes.insert(tenant, mode.into());
        Ok(true)
    }

    async fn credentials_for(
        &self,
        tenant: TenantId,
        identifier: &str,
    ) -> ApiResult<Option<(UserId, Option<String>)>> {
        let st = self.inner.lock().unwrap();
        let ident = identifier.to_lowercase();
        Ok(st
            .users
            .iter()
            .find(|u| {
                u.tenant_id == tenant
                    && (u.email.to_lowercase() == ident
                        || st.username_of.get(&u.id).map(|n| n.to_lowercase())
                            == Some(ident.clone()))
            })
            .map(|u| (u.id, st.passwords.get(&u.id).cloned().flatten())))
    }

    async fn create_local_user(
        &self,
        new: NewLocalUser,
    ) -> ApiResult<Result<User, CreateUserError>> {
        let mut st = self.inner.lock().unwrap();
        if st.username_of.values().any(|n| *n == new.username) {
            return Ok(Err(CreateUserError::UsernameTaken));
        }
        if st
            .users
            .iter()
            .any(|u| u.tenant_id == new.tenant && u.email == new.email)
        {
            return Ok(Err(CreateUserError::EmailTaken));
        }
        let u = new_user(
            new.tenant,
            &new.display_name,
            &new.email,
            Some(&new.username),
            &new.role,
        );
        st.person_of.insert(u.id, Uuid::now_v7());
        st.username_of.insert(u.id, new.username.clone());
        st.users.push(u.clone());
        st.passwords.insert(u.id, Some(new.password_hash));
        // Mirrors the real impl, including the flag: a fake that always granted
        // would hide the invite path's deliberate omission.
        if new.grant_membership {
            st.members.push((new.tenant, u.id, new.role));
        }
        Ok(Ok(u))
    }

    async fn password_hash_of(&self, user_id: UserId) -> ApiResult<Option<Option<String>>> {
        let st = self.inner.lock().unwrap();
        if !st.users.iter().any(|u| u.id == user_id) {
            return Ok(None);
        }
        Ok(Some(st.passwords.get(&user_id).cloned().flatten()))
    }

    async fn set_password_hash(&self, user_id: UserId, hash: &str) -> ApiResult<()> {
        self.inner
            .lock()
            .unwrap()
            .passwords
            .insert(user_id, Some(hash.into()));
        Ok(())
    }

    async fn create_auth_session(
        &self,
        id: AuthSessionId,
        user_id: UserId,
        tenant: TenantId,
        _ttl_hours: i32,
    ) -> ApiResult<()> {
        self.inner
            .lock()
            .unwrap()
            .auth_sessions
            .push((id, user_id, tenant));
        Ok(())
    }

    async fn has_permission(
        &self,
        user_id: UserId,
        permission: &str,
        org_id: Option<Uuid>,
        tenant_id: Option<Uuid>,
    ) -> ApiResult<bool> {
        let st = self.inner.lock().unwrap();
        Ok(st.permissions.iter().any(|(u, p, o, t)| {
            *u == user_id
                && p == permission
                // A deployment-scoped binding (both None) covers everything
                // below it, exactly as the SQL's `scope_type = 'deployment'`
                // arm does.
                && ((o.is_none() && t.is_none())
                    || (o.is_some() && *o == org_id)
                    || (t.is_some() && *t == tenant_id))
        }))
    }

    async fn node_owner_person(
        &self,
        node_id: Uuid,
        tenant: TenantId,
    ) -> ApiResult<Option<Option<Uuid>>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .nodes
            .iter()
            .find(|(id, t, _, _, _)| *id == node_id && *t == tenant)
            .map(|(_, _, owner, _, _)| *owner))
    }

    async fn node_owner_and_shared(
        &self,
        node_id: Uuid,
        tenant: TenantId,
    ) -> ApiResult<Option<(Option<Uuid>, bool)>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .nodes
            .iter()
            .find(|(id, t, _, _, _)| *id == node_id && *t == tenant)
            .map(|(_, _, owner, shared, _)| (*owner, *shared)))
    }

    async fn node_by_token_hash(&self, hash: &str) -> ApiResult<Option<(Uuid, Uuid)>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .nodes
            .iter()
            .find(|(_, _, _, _, h)| h == hash)
            .map(|(id, t, _, _, _)| (*id, t.0)))
    }
}

/// The DTO literals the fake builds, in one place — `Tenant`/`User` have no
/// `Default`, and repeating the field list at five call sites is how one of them
/// ends up subtly different from the others.
fn new_tenant(name: &str, slug: &str) -> Tenant {
    let now = Utc::now();
    Tenant {
        id: TenantId::new(),
        name: name.into(),
        slug: slug.into(),
        created_at: now,
        updated_at: now,
    }
}

fn new_user(
    tenant: TenantId,
    display_name: &str,
    email: &str,
    _username: Option<&str>,
    role: &str,
) -> User {
    let now = Utc::now();
    User {
        id: UserId::new(),
        tenant_id: tenant,
        display_name: display_name.into(),
        email: email.into(),
        avatar_url: None,
        role: role.into(),
        created_at: now,
        updated_at: now,
    }
}
