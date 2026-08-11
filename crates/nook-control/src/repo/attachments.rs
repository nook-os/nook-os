//! What a ticket or a comment has files hung off it (MAIN-533).
//!
//! The join table MAIN-532 said a consumer would bring. It owns one table and
//! reads a second: `user_content` supplies the filename, type and size a list
//! renders, so a UI gets everything it needs in one query rather than one plus
//! N.
//!
//! **Every method takes the tenant and matches on it**, exactly as the content
//! repository does and for the same reason — a uuid is not an authorisation. A
//! parent id from another tenant finds nothing rather than finding someone
//! else's attachments.

use std::collections::HashMap;

use async_trait::async_trait;
use nook_db::{params, Db, DbPool};
use nook_types::*;
use uuid::Uuid;

use crate::error::ApiResult;

/// The two things a `parent_kind` may be. Spelled once so a typo in a route is
/// a compile error rather than a query that quietly matches nothing.
pub const PARENT_TASK: &str = "task";
pub const PARENT_COMMENT: &str = "task_comment";

/// A row to write.
#[derive(Debug, Clone)]
pub struct NewAttachment {
    pub tenant: TenantId,
    pub user_content_id: Uuid,
    pub parent_kind: String,
    pub parent_id: Uuid,
    pub attached_by: UserId,
}

/// What removing an attachment needs to know: who put it there, and which
/// content row (and therefore which object) it points at.
#[derive(Debug, Clone, nook_db::FromDbRow)]
pub struct AttachmentRow {
    pub id: Uuid,
    pub user_content_id: Uuid,
    pub parent_kind: String,
    pub parent_id: Uuid,
    pub attached_by: UserId,
}

/// The columns the API type is built from — the join spelled once, because
/// three queries select exactly it.
const ATTACHMENT_COLUMNS: &str = "a.id, a.parent_kind, a.parent_id, a.attached_by, \
                                  a.user_content_id, c.filename, c.content_type, \
                                  c.size_bytes, a.created_at";

#[async_trait]
pub trait TaskAttachmentRepository: Send + Sync {
    async fn attach(&self, new: NewAttachment) -> ApiResult<TaskAttachment>;

    /// Everything on one parent, oldest first — a thread is read as a
    /// narrative, and so is the row of files under it.
    async fn list(
        &self,
        tenant: TenantId,
        parent_kind: &str,
        parent_id: Uuid,
    ) -> ApiResult<Vec<TaskAttachment>>;

    /// The whole thread's attachments — the ticket's own and every comment's,
    /// in one query.
    ///
    /// The alternative is the client asking per comment, which is an N+1 on the
    /// one page that has N comments by definition.
    async fn list_thread(&self, tenant: TenantId, task: TaskId) -> ApiResult<Vec<TaskAttachment>>;

    /// One row, or `None` — including when it belongs to another tenant.
    async fn get(&self, tenant: TenantId, id: Uuid) -> ApiResult<Option<AttachmentRow>>;

    /// Every attachment a task carries, its comments' included. What the
    /// cascade removes when the ticket goes (AC-7).
    async fn of_task(&self, tenant: TenantId, task: TaskId) -> ApiResult<Vec<AttachmentRow>>;

    /// How many attachments each of these tasks carries, comments included
    /// (AC-8). One query for a whole board.
    async fn counts_for_tasks(
        &self,
        tenant: TenantId,
        task_ids: &[Uuid],
    ) -> ApiResult<HashMap<Uuid, i64>>;
}

pub struct DbTaskAttachmentRepository {
    db: DbPool,
}

impl DbTaskAttachmentRepository {
    pub fn new(db: DbPool) -> Self {
        Self { db }
    }
}

#[async_trait]
impl TaskAttachmentRepository for DbTaskAttachmentRepository {
    async fn attach(&self, new: NewAttachment) -> ApiResult<TaskAttachment> {
        // The same file attached twice to one parent is a double-click, so the
        // insert yields to the unique index and the row is read back by its
        // natural key rather than by the id we just tried to give it. Two
        // statements and not one `RETURNING`: a data-modifying CTE is Postgres
        // only, and this has to run on both engines.
        self.db
            .exec(
                "INSERT INTO task_attachments
                    (id, tenant_id, user_content_id, parent_kind, parent_id, attached_by)
                 VALUES ($1, $2, $3, $4, $5, $6)
                 ON CONFLICT (parent_kind, parent_id, user_content_id) DO NOTHING",
                params![
                    Uuid::now_v7(),
                    new.tenant,
                    new.user_content_id,
                    new.parent_kind.clone(),
                    new.parent_id,
                    new.attached_by.0
                ],
            )
            .await?;

        Ok(self
            .db
            .query_one(
                &format!(
                    "SELECT {ATTACHMENT_COLUMNS}
                       FROM task_attachments a
                       JOIN user_content c ON c.id = a.user_content_id
                      WHERE a.tenant_id = $1 AND a.parent_kind = $2
                        AND a.parent_id = $3 AND a.user_content_id = $4"
                ),
                params![
                    new.tenant,
                    new.parent_kind,
                    new.parent_id,
                    new.user_content_id
                ],
            )
            .await?)
    }

    async fn list(
        &self,
        tenant: TenantId,
        parent_kind: &str,
        parent_id: Uuid,
    ) -> ApiResult<Vec<TaskAttachment>> {
        Ok(self
            .db
            .query_all(
                &format!(
                    "SELECT {ATTACHMENT_COLUMNS}
                       FROM task_attachments a
                       JOIN user_content c ON c.id = a.user_content_id
                      WHERE a.tenant_id = $1 AND a.parent_kind = $2 AND a.parent_id = $3
                      ORDER BY a.created_at, a.id"
                ),
                params![tenant, parent_kind, parent_id],
            )
            .await?)
    }

    async fn list_thread(&self, tenant: TenantId, task: TaskId) -> ApiResult<Vec<TaskAttachment>> {
        Ok(self
            .db
            .query_all(
                &format!(
                    "SELECT {ATTACHMENT_COLUMNS}
                       FROM task_attachments a
                       JOIN user_content c ON c.id = a.user_content_id
                      WHERE a.tenant_id = $1
                        AND (
                            (a.parent_kind = 'task' AND a.parent_id = $2)
                            OR (a.parent_kind = 'task_comment'
                                AND a.parent_id IN
                                    (SELECT id FROM task_comments WHERE task_id = $3))
                        )
                      ORDER BY a.created_at, a.id"
                ),
                params![tenant, task, task],
            )
            .await?)
    }

    async fn get(&self, tenant: TenantId, id: Uuid) -> ApiResult<Option<AttachmentRow>> {
        Ok(self
            .db
            .query_opt(
                "SELECT id, user_content_id, parent_kind, parent_id, attached_by
                   FROM task_attachments WHERE id = $1 AND tenant_id = $2",
                params![id, tenant],
            )
            .await?)
    }

    async fn of_task(&self, tenant: TenantId, task: TaskId) -> ApiResult<Vec<AttachmentRow>> {
        Ok(self
            .db
            .query_all(
                "SELECT a.id, a.user_content_id, a.parent_kind, a.parent_id, a.attached_by
                   FROM task_attachments a
                  WHERE a.tenant_id = $1
                    AND (
                        (a.parent_kind = 'task' AND a.parent_id = $2)
                        OR (a.parent_kind = 'task_comment'
                            AND a.parent_id IN (SELECT id FROM task_comments WHERE task_id = $3))
                    )",
                params![tenant, task, task],
            )
            .await?)
    }

    async fn counts_for_tasks(
        &self,
        tenant: TenantId,
        task_ids: &[Uuid],
    ) -> ApiResult<HashMap<Uuid, i64>> {
        if task_ids.is_empty() {
            return Ok(HashMap::new());
        }
        // A comment's attachment counts toward its ticket: a screenshot pasted
        // into the discussion is exactly the context AC-8 wants discoverable
        // from the board, and a reader who opens the card finds it either way.
        Ok(self
            .db
            .query_all::<(Uuid, i64)>(
                "SELECT task_id, COUNT(*) FROM (
                     SELECT a.parent_id AS task_id
                       FROM task_attachments a
                      WHERE a.tenant_id = $1 AND a.parent_kind = 'task'
                        AND a.parent_id = ANY($2)
                     UNION ALL
                     SELECT c.task_id AS task_id
                       FROM task_attachments a
                       JOIN task_comments c ON c.id = a.parent_id
                      WHERE a.tenant_id = $3 AND a.parent_kind = 'task_comment'
                        AND c.task_id = ANY($4)
                 ) counted
                 GROUP BY task_id",
                params![tenant, task_ids, tenant, task_ids],
            )
            .await?
            .into_iter()
            .collect())
    }
}
