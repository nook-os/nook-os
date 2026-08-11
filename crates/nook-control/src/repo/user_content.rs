//! User-content data access (MAIN-532).
//!
//! One table, three operations, and nothing that knows what the content is
//! *for*. A consumer that later wants ticket attachments brings its own join
//! table and reads through this trait — it does not add a column here.
//!
//! **Every read and write is tenant-scoped, and that is a security property
//! rather than tidiness.** `get` takes the tenant from the authenticated
//! caller and matches on it, so another tenant's id comes back `None` and the
//! route answers 404. A repository that returned the row and left the check to
//! the caller would make "did we remember to compare the tenant" a question
//! asked at three call sites instead of answered at one (AC-3).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use nook_db::{params, Db, DbPool};
use nook_types::*;
use uuid::Uuid;

use crate::error::ApiResult;

/// The whole row — what the API type omits, plus what it carries.
///
/// `storage_key` and `uploaded_by` live here and never in [`UserContent`]:
/// the key is how the server reaches the bytes, and the uploader is who is
/// allowed to remove them. Both are answers the server gives, not facts it
/// hands out.
#[derive(Debug, Clone, nook_db::FromDbRow)]
pub struct StoredContent {
    pub id: Uuid,
    pub tenant_id: TenantId,
    pub uploaded_by: UserId,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub sha256: String,
    pub storage_key: String,
    pub created_at: DateTime<Utc>,
}

impl StoredContent {
    /// The public shape of this row.
    pub fn record(&self) -> UserContent {
        UserContent {
            id: self.id,
            filename: self.filename.clone(),
            content_type: self.content_type.clone(),
            size_bytes: self.size_bytes,
            sha256: self.sha256.clone(),
            created_at: self.created_at,
        }
    }
}

/// A row to write. The id is chosen by the caller because the storage key is
/// derived from it, and the bytes are written before the row is.
#[derive(Debug, Clone)]
pub struct NewContent {
    pub id: Uuid,
    pub tenant: TenantId,
    pub uploaded_by: UserId,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub sha256: String,
    pub storage_key: String,
}

const CONTENT_COLUMNS: &str = "id, tenant_id, uploaded_by, filename, content_type, \
                               size_bytes, sha256, storage_key, created_at";

#[async_trait]
pub trait UserContentRepository: Send + Sync {
    async fn insert(&self, new: NewContent) -> ApiResult<StoredContent>;

    /// The row, or `None` — including when it belongs to another tenant, which
    /// is what makes an id unprobeable across tenants (AC-3).
    async fn get(&self, id: Uuid, tenant: TenantId) -> ApiResult<Option<StoredContent>>;

    /// Rows removed: `1` normally, `0` if someone else got there first. The
    /// bytes are the route's to remove — this trait owns the table, not the
    /// store.
    async fn delete(&self, id: Uuid, tenant: TenantId) -> ApiResult<u64>;
}

pub struct DbUserContentRepository {
    db: DbPool,
}

impl DbUserContentRepository {
    pub fn new(db: DbPool) -> Self {
        Self { db }
    }
}

#[async_trait]
impl UserContentRepository for DbUserContentRepository {
    async fn insert(&self, new: NewContent) -> ApiResult<StoredContent> {
        Ok(self
            .db
            .query_one(
                &format!(
                    "INSERT INTO user_content
                        (id, tenant_id, uploaded_by, filename, content_type,
                         size_bytes, sha256, storage_key)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                     RETURNING {CONTENT_COLUMNS}"
                ),
                params![
                    new.id,
                    new.tenant,
                    new.uploaded_by.0,
                    new.filename,
                    new.content_type,
                    new.size_bytes,
                    new.sha256,
                    new.storage_key
                ],
            )
            .await?)
    }

    async fn get(&self, id: Uuid, tenant: TenantId) -> ApiResult<Option<StoredContent>> {
        Ok(self
            .db
            .query_opt(
                &format!(
                    "SELECT {CONTENT_COLUMNS} FROM user_content
                     WHERE id = $1 AND tenant_id = $2"
                ),
                params![id, tenant],
            )
            .await?)
    }

    async fn delete(&self, id: Uuid, tenant: TenantId) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                "DELETE FROM user_content WHERE id = $1 AND tenant_id = $2",
                params![id, tenant],
            )
            .await?)
    }
}
