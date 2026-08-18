//! Named secret items: `(scope, scope_id, name) -> encrypted value` (MAIN-625).
//!
//! **Nothing in this module encrypts or decrypts.** Values arrive as the two
//! halves [`crate::crypto::Envelope`] produced and leave the same way; the
//! service layer holds the app key. That is what keeps a plaintext secret out
//! of every row struct here, and out of anything that formats one.
//!
//! Deliberately not `workspace_secrets`, which stays exactly as it is (NG-2):
//! that table is one password-sealed blob per repo, and the server cannot read
//! it by design. These are values it can, because delivering one into a session
//! or a job container is the whole point.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use nook_db::{dialect::type_mapping, params, Db, DbPool};
use nook_types::*;
use uuid::Uuid;

use crate::error::ApiResult;

/// An item as it is stored. `Debug` by hand for the reason
/// [`crate::repo::email_pollers::EmailPoller`] gives: a derived one would print
/// ciphertext into any log that formatted a row.
#[derive(Clone, nook_db::FromDbRow)]
pub struct SecretItemRow {
    pub tenant_id: TenantId,
    pub scope: String,
    pub scope_id: Uuid,
    pub name: String,
    pub value_enc: Vec<u8>,
    pub dek_wrapped: Vec<u8>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl std::fmt::Debug for SecretItemRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretItemRow")
            .field("tenant_id", &self.tenant_id)
            .field("scope", &self.scope)
            .field("scope_id", &self.scope_id)
            .field("name", &self.name)
            .field("value_enc", &"<sealed>")
            .field("dek_wrapped", &"<sealed>")
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

impl SecretItemRow {
    /// The read shape, which has nowhere to put a value (AC-4).
    pub fn summary(&self) -> SecretItem {
        SecretItem {
            // A row written by a newer build with a scope this one has never
            // heard of lists as a tenant item rather than making the whole
            // listing fail — the same conservative fallback `SessionInterface`
            // takes. Nothing is DELIVERED on it: `env_for` matches on the
            // stored string, not on this.
            scope: SecretScope::parse(&self.scope).unwrap_or(SecretScope::Tenant),
            scope_id: self.scope_id,
            name: self.name.clone(),
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

/// What a caller is asking to store. The value arrives already sealed — the
/// route seals it before it reaches this layer, so no path exists that writes a
/// plaintext one by forgetting to.
pub struct NewSecretItem {
    pub tenant: TenantId,
    pub scope: SecretScope,
    pub scope_id: Uuid,
    pub name: String,
    pub value_enc: Vec<u8>,
    pub dek_wrapped: Vec<u8>,
    pub updated_by: Option<UserId>,
}

const ITEM_COLUMNS: &str =
    "tenant_id, scope, scope_id, name, value_enc, dek_wrapped, created_at, updated_at";

#[async_trait]
pub trait SecretItemRepository: Send + Sync {
    /// Create the item, or replace the value of one that exists.
    ///
    /// A replace keeps `created_at`, so "when was this first set" survives a
    /// rotation — which is the question an audit actually asks.
    async fn put(&self, new: NewSecretItem) -> ApiResult<SecretItemRow>;

    /// Every item in the tenant, in scope-then-name order.
    ///
    /// The whole tenant rather than one scope, because the scope RULES live in
    /// one pure function (`services::secret_items::env_for`) instead of being
    /// spelled again as a WHERE clause here — two places to say who gets what
    /// is how they come to disagree.
    async fn list(&self, tenant: TenantId) -> ApiResult<Vec<SecretItemRow>>;

    async fn get(
        &self,
        tenant: TenantId,
        scope: SecretScope,
        scope_id: Uuid,
        name: &str,
    ) -> ApiResult<Option<SecretItemRow>>;

    /// Remove it. `false` when there was none, which the route turns into a 404
    /// rather than a silent success.
    async fn delete(
        &self,
        tenant: TenantId,
        scope: SecretScope,
        scope_id: Uuid,
        name: &str,
    ) -> ApiResult<bool>;
}

pub struct DbSecretItemRepository {
    db: DbPool,
}

impl DbSecretItemRepository {
    pub fn new(db: DbPool) -> Self {
        Self { db }
    }
}

#[async_trait]
impl SecretItemRepository for DbSecretItemRepository {
    async fn put(&self, new: NewSecretItem) -> ApiResult<SecretItemRow> {
        let now = type_mapping(self.db.engine()).now();
        self.db
            .exec(
                &format!(
                    "INSERT INTO secret_items
                        (id, tenant_id, scope, scope_id, name, value_enc, dek_wrapped,
                         updated_by, created_at, updated_at)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, {now}, {now})
                     ON CONFLICT (scope, scope_id, name) DO UPDATE SET
                        value_enc = $6, dek_wrapped = $7, updated_by = $8,
                        updated_at = {now}"
                ),
                params![
                    Uuid::now_v7(),
                    new.tenant,
                    new.scope.as_str().to_string(),
                    new.scope_id,
                    new.name.clone(),
                    new.value_enc,
                    new.dek_wrapped,
                    new.updated_by.map(|u| u.0)
                ],
            )
            .await?;
        // Read back rather than `RETURNING`, as `email_pollers` does: a
        // data-modifying CTE is Postgres only and this runs on both engines.
        self.get(new.tenant, new.scope, new.scope_id, &new.name)
            .await?
            .ok_or_else(|| {
                crate::error::ApiError::Internal(anyhow::anyhow!("secret item vanished"))
            })
    }

    async fn list(&self, tenant: TenantId) -> ApiResult<Vec<SecretItemRow>> {
        self.db
            .query_all(
                &format!(
                    "SELECT {ITEM_COLUMNS} FROM secret_items
                      WHERE tenant_id = $1
                      ORDER BY scope, scope_id, name"
                ),
                params![tenant],
            )
            .await
            .map_err(Into::into)
    }

    async fn get(
        &self,
        tenant: TenantId,
        scope: SecretScope,
        scope_id: Uuid,
        name: &str,
    ) -> ApiResult<Option<SecretItemRow>> {
        self.db
            .query_opt(
                &format!(
                    "SELECT {ITEM_COLUMNS} FROM secret_items
                      WHERE tenant_id = $1 AND scope = $2 AND scope_id = $3 AND name = $4"
                ),
                params![
                    tenant,
                    scope.as_str().to_string(),
                    scope_id,
                    name.to_string()
                ],
            )
            .await
            .map_err(Into::into)
    }

    async fn delete(
        &self,
        tenant: TenantId,
        scope: SecretScope,
        scope_id: Uuid,
        name: &str,
    ) -> ApiResult<bool> {
        Ok(self
            .db
            .exec(
                "DELETE FROM secret_items
                  WHERE tenant_id = $1 AND scope = $2 AND scope_id = $3 AND name = $4",
                params![
                    tenant,
                    scope.as_str().to_string(),
                    scope_id,
                    name.to_string()
                ],
            )
            .await?
            > 0)
    }
}
