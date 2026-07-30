//! Invite data access (MAIN-250).
//!
//! Everything the invite surface reads or writes about the `invites` table
//! lives behind [`InviteRepository`]. What it does *not* own is the rest of
//! `routes/invites.rs`'s data: accepting an invite reads `users`, writes
//! `tenant_members` and moves a `sessions_auth` row, and all three belong to
//! the identity aggregate. Those went onto [`crate::repo::identity`]'s trait
//! rather than being copied here — a second trait touching `users` would be two
//! places to change when that table does, which is the problem the chain exists
//! to remove.
//!
//! Methods are intent-named and coarse, with no `sqlx` type in any signature.
//! The token's plaintext is never at rest: every lookup is by hash, and the
//! signatures say `token_hash` so a caller cannot pass the wrong one by
//! accident.

use async_trait::async_trait;
use nook_db::dialect::{time_math, type_mapping};
use nook_db::{params, Db, DbPool};
use nook_types::{Invite, TenantId};
use uuid::Uuid;

use crate::error::ApiResult;

/// An invite as the accept path needs it: enough to decide, without loading a
/// shape the caller has to re-interpret.
#[derive(Debug, Clone)]
pub struct InviteRow {
    pub id: Uuid,
    pub tenant: TenantId,
    pub email: String,
    pub role: String,
    pub status: String,
}

/// What the accept-link preview page shows. `valid` is computed in SQL so the
/// row has the same shape whatever the status — a missing, expired, revoked or
/// already-accepted token must not be tellable apart.
#[derive(Debug, Clone)]
pub struct InvitePreview {
    pub tenant: TenantId,
    pub email: String,
    pub invited_by: Option<Uuid>,
    pub valid: bool,
}

/// What invite-gated registration needs: which tenant, which email the account
/// must use, the role acceptance will apply, and whether the link is usable.
#[derive(Debug, Clone)]
pub struct InviteRegistration {
    pub tenant: TenantId,
    pub email: String,
    pub role: String,
    pub valid: bool,
}

#[async_trait]
pub trait InviteRepository: Send + Sync {
    /// Issue an invite, replacing any pending one for the same email so
    /// re-inviting does not stack. Both statements in one method because the
    /// replace exists only to make room for the insert.
    async fn issue(
        &self,
        tenant: TenantId,
        email: &str,
        role: &str,
        token_hash: &str,
        invited_by: Uuid,
    ) -> ApiResult<Invite>;

    async fn list_pending(&self, tenant: TenantId) -> ApiResult<Vec<Invite>>;

    /// Revoke a pending invite; its link stops working. Rows affected, so the
    /// caller can tell "not pending" from "revoked".
    async fn revoke(&self, id: Uuid, tenant: TenantId) -> ApiResult<u64>;

    /// Re-issue the token and re-stamp the expiry. The stored hash is
    /// irreversible, so a resend cannot re-send the old link — it mints a new
    /// one, which invalidates the old.
    async fn reissue(
        &self,
        id: Uuid,
        tenant: TenantId,
        token_hash: &str,
    ) -> ApiResult<Option<Invite>>;

    async fn by_token_hash(&self, token_hash: &str) -> ApiResult<Option<InviteRow>>;

    /// Is this invite still within its expiry? Asked of the database rather
    /// than compared against our clock.
    async fn is_fresh(&self, id: Uuid) -> ApiResult<bool>;

    async fn mark_accepted(&self, id: Uuid) -> ApiResult<()>;

    async fn preview(&self, token_hash: &str) -> ApiResult<Option<InvitePreview>>;

    async fn registration_target(&self, token_hash: &str) -> ApiResult<Option<InviteRegistration>>;
}

/// The real implementation, over the engine-agnostic pool.
pub struct DbInviteRepository {
    db: DbPool,
}

impl DbInviteRepository {
    pub fn new(db: DbPool) -> Self {
        Self { db }
    }
}

#[async_trait]
impl InviteRepository for DbInviteRepository {
    async fn issue(
        &self,
        tenant: TenantId,
        email: &str,
        role: &str,
        token_hash: &str,
        invited_by: Uuid,
    ) -> ApiResult<Invite> {
        // Replace any existing pending invite for this email so re-inviting does
        // not stack; the partial unique index also enforces one pending.
        self.db
            .exec(
                "DELETE FROM invites
         WHERE tenant_id = $1 AND status = 'pending' AND lower(email) = lower($2)",
                params![tenant, email],
            )
            .await?;

        // Only the hash is stored; the plaintext rides in the accept link.
        Ok(self
            .db
            .query_one(
                &format!(
                    "INSERT INTO invites (id, tenant_id, email, role, token_hash, status, invited_by, expires_at)
         VALUES ($1, $2, $3, $4, $5, 'pending', $6, {expiry})
         RETURNING id, email, role, status, created_at, expires_at",
                    expiry = time_math(self.db.engine()).now_plus("14 days")
                ),
                params![Uuid::now_v7(), tenant, email, role, token_hash, invited_by],
            )
            .await?)
    }

    async fn list_pending(&self, tenant: TenantId) -> ApiResult<Vec<Invite>> {
        Ok(self
            .db
            .query_all(
                "SELECT id, email, role, status, created_at, expires_at
         FROM invites WHERE tenant_id = $1 AND status = 'pending'
         ORDER BY created_at DESC",
                params![tenant],
            )
            .await?)
    }

    async fn revoke(&self, id: Uuid, tenant: TenantId) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                "UPDATE invites SET status = 'revoked'
         WHERE id = $1 AND tenant_id = $2 AND status = 'pending'",
                params![id, tenant],
            )
            .await?)
    }

    async fn reissue(
        &self,
        id: Uuid,
        tenant: TenantId,
        token_hash: &str,
    ) -> ApiResult<Option<Invite>> {
        Ok(self
            .db
            .query_opt(
                &format!(
                    "UPDATE invites
            SET token_hash = $1, expires_at = {expiry}
          WHERE id = $2 AND tenant_id = $3 AND status = 'pending'
      RETURNING id, email, role, status, created_at, expires_at",
                    expiry = time_math(self.db.engine()).now_plus("14 days")
                ),
                params![token_hash, id, tenant],
            )
            .await?)
    }

    async fn by_token_hash(&self, token_hash: &str) -> ApiResult<Option<InviteRow>> {
        let row: Option<(Uuid, TenantId, String, String, String)> = self
            .db
            .query_opt(
                "SELECT id, tenant_id, email, role, status FROM invites WHERE token_hash = $1",
                params![token_hash],
            )
            .await?;
        Ok(row.map(|(id, tenant, email, role, status)| InviteRow {
            id,
            tenant,
            email,
            role,
            status,
        }))
    }

    async fn is_fresh(&self, id: Uuid) -> ApiResult<bool> {
        Ok(self
            .db
            .query_scalar(
                &format!(
                    "SELECT expires_at > {} FROM invites WHERE id = $1",
                    type_mapping(self.db.engine()).now()
                ),
                params![id],
            )
            .await?)
    }

    async fn mark_accepted(&self, id: Uuid) -> ApiResult<()> {
        self.db
            .exec(
                "UPDATE invites SET status = 'accepted' WHERE id = $1",
                params![id],
            )
            .await?;
        Ok(())
    }

    async fn preview(&self, token_hash: &str) -> ApiResult<Option<InvitePreview>> {
        let row: Option<(TenantId, String, Option<Uuid>, bool)> = self
            .db
            .query_opt(
                &format!(
                    "SELECT tenant_id, email, invited_by, (status = 'pending' AND expires_at > {})
         FROM invites WHERE token_hash = $1",
                    type_mapping(self.db.engine()).now()
                ),
                params![token_hash],
            )
            .await?;
        Ok(row.map(|(tenant, email, invited_by, valid)| InvitePreview {
            tenant,
            email,
            invited_by,
            valid,
        }))
    }

    async fn registration_target(&self, token_hash: &str) -> ApiResult<Option<InviteRegistration>> {
        let row: Option<(TenantId, String, String, bool)> = self
            .db
            .query_opt(
                &format!(
                    "SELECT tenant_id, email, role, (status = 'pending' AND expires_at > {})
         FROM invites WHERE token_hash = $1",
                    type_mapping(self.db.engine()).now()
                ),
                params![token_hash],
            )
            .await?;
        Ok(row.map(|(tenant, email, role, valid)| InviteRegistration {
            tenant,
            email,
            role,
            valid,
        }))
    }
}

/// An in-memory [`InviteRepository`] for tests that should not need a database
/// (MAIN-250 AC-3).
///
/// Faithful where the behaviour under test lives — one pending invite per
/// email, status transitions, expiry as a flag a test can set rather than a
/// clock it must wait for — and deliberately simple elsewhere.
#[derive(Default)]
pub struct FakeInviteRepository {
    inner: std::sync::Mutex<Vec<Row>>,
}

struct Row {
    id: Uuid,
    tenant: TenantId,
    email: String,
    role: String,
    status: String,
    token_hash: String,
    invited_by: Option<Uuid>,
    /// Expiry as a flag: a test can make an invite stale without waiting.
    expired: bool,
    created_at: chrono::DateTime<chrono::Utc>,
}

impl Row {
    fn to_invite(&self) -> Invite {
        Invite {
            id: self.id,
            email: self.email.clone(),
            role: self.role.clone(),
            status: self.status.clone(),
            created_at: self.created_at,
            expires_at: self.created_at + chrono::Duration::days(14),
            accept_url: None,
        }
    }
}

impl FakeInviteRepository {
    pub fn new() -> Self {
        Self::default()
    }

    /// Make an invite look expired, without a test having to wait 14 days.
    pub fn expire(&self, id: Uuid) {
        for r in self.inner.lock().unwrap().iter_mut() {
            if r.id == id {
                r.expired = true;
            }
        }
    }

    /// The stored status, for asserting a transition happened (or did not).
    pub fn status_of(&self, id: Uuid) -> Option<String> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.id == id)
            .map(|r| r.status.clone())
    }

    /// How many invites exist for an email in a tenant — the one-pending rule.
    pub fn count_for(&self, tenant: TenantId, email: &str) -> usize {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.tenant == tenant && r.email.eq_ignore_ascii_case(email))
            .count()
    }
}

#[async_trait]
impl InviteRepository for FakeInviteRepository {
    async fn issue(
        &self,
        tenant: TenantId,
        email: &str,
        role: &str,
        token_hash: &str,
        invited_by: Uuid,
    ) -> ApiResult<Invite> {
        let mut st = self.inner.lock().unwrap();
        // The DELETE the real impl runs first: re-inviting replaces rather than
        // stacks. A fake that skipped it would let a caller test pass while the
        // partial unique index rejected the insert for real.
        st.retain(|r| {
            !(r.tenant == tenant && r.status == "pending" && r.email.eq_ignore_ascii_case(email))
        });
        let row = Row {
            id: Uuid::now_v7(),
            tenant,
            email: email.into(),
            role: role.into(),
            status: "pending".into(),
            token_hash: token_hash.into(),
            invited_by: Some(invited_by),
            expired: false,
            created_at: chrono::Utc::now(),
        };
        let out = row.to_invite();
        st.push(row);
        Ok(out)
    }

    async fn list_pending(&self, tenant: TenantId) -> ApiResult<Vec<Invite>> {
        let st = self.inner.lock().unwrap();
        let mut v: Vec<&Row> = st
            .iter()
            .filter(|r| r.tenant == tenant && r.status == "pending")
            .collect();
        // Newest first, matching the real query's `ORDER BY created_at DESC`.
        v.sort_by_key(|r| std::cmp::Reverse(r.created_at));
        Ok(v.into_iter().map(Row::to_invite).collect())
    }

    async fn revoke(&self, id: Uuid, tenant: TenantId) -> ApiResult<u64> {
        let mut st = self.inner.lock().unwrap();
        let mut n = 0;
        for r in st.iter_mut() {
            if r.id == id && r.tenant == tenant && r.status == "pending" {
                r.status = "revoked".into();
                n += 1;
            }
        }
        Ok(n)
    }

    async fn reissue(
        &self,
        id: Uuid,
        tenant: TenantId,
        token_hash: &str,
    ) -> ApiResult<Option<Invite>> {
        let mut st = self.inner.lock().unwrap();
        let Some(r) = st
            .iter_mut()
            .find(|r| r.id == id && r.tenant == tenant && r.status == "pending")
        else {
            return Ok(None);
        };
        // A fresh token invalidates the old link, which is the point of a resend.
        r.token_hash = token_hash.into();
        r.expired = false;
        Ok(Some(r.to_invite()))
    }

    async fn by_token_hash(&self, token_hash: &str) -> ApiResult<Option<InviteRow>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .iter()
            .find(|r| r.token_hash == token_hash)
            .map(|r| InviteRow {
                id: r.id,
                tenant: r.tenant,
                email: r.email.clone(),
                role: r.role.clone(),
                status: r.status.clone(),
            }))
    }

    async fn is_fresh(&self, id: Uuid) -> ApiResult<bool> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .iter()
            .find(|r| r.id == id)
            .map(|r| !r.expired)
            .unwrap_or(false))
    }

    async fn mark_accepted(&self, id: Uuid) -> ApiResult<()> {
        let mut st = self.inner.lock().unwrap();
        for r in st.iter_mut() {
            if r.id == id {
                r.status = "accepted".into();
            }
        }
        Ok(())
    }

    async fn preview(&self, token_hash: &str) -> ApiResult<Option<InvitePreview>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .iter()
            .find(|r| r.token_hash == token_hash)
            .map(|r| InvitePreview {
                tenant: r.tenant,
                email: r.email.clone(),
                invited_by: r.invited_by,
                valid: r.status == "pending" && !r.expired,
            }))
    }

    async fn registration_target(&self, token_hash: &str) -> ApiResult<Option<InviteRegistration>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .iter()
            .find(|r| r.token_hash == token_hash)
            .map(|r| InviteRegistration {
                tenant: r.tenant,
                email: r.email.clone(),
                role: r.role.clone(),
                valid: r.status == "pending" && !r.expired,
            }))
    }
}
