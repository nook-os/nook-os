//! The chain from a support email to the work it caused (MAIN-330).
//!
//! One row per accepted delivery, grown as the work moves: the inbound pipeline
//! writes the ticket and the sealed object, seeding the run fills in the job,
//! and opening a PR fills in the reference. Nothing here rewrites a link's
//! ticket — a chain is about one message, and a second message is a second row.
//!
//! **Every method takes the tenant and matches on it (AC-4).** A message id is
//! a string an outsider chose and a task id is a uuid somebody may have
//! guessed; neither is an authorisation, so the tenant is a parameter of the
//! query rather than something a caller is trusted to have checked. There is
//! deliberately no method here that can be called without one.
//!
//! The reads take the VIEWER too, and join the card: a link names a ticket, so
//! a link list that ignored visibility would say a private card exists to
//! somebody who may not open it — the side channel `readable_task` closes on
//! every other surface that hangs content off a card (MAIN-76).

use async_trait::async_trait;
use nook_db::{params, Db, DbPool};
use nook_types::*;
use uuid::Uuid;

use crate::error::ApiResult;
use crate::services::tasks::visible_sql;

/// A link to write. Everything a delivery knows at the moment it is accepted;
/// the job and the PR arrive later.
#[derive(Debug, Clone)]
pub struct NewEmailLink {
    pub tenant: TenantId,
    pub workspace: Option<WorkspaceId>,
    pub task: TaskId,
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub storage_key: String,
}

/// The columns the API type is built from, spelled once because every read
/// selects exactly it.
const LINK_COLUMNS: &str = "l.id, l.workspace_id, l.task_id, l.loop_job_id, l.pr_ref, \
                            l.message_id, l.in_reply_to, l.storage_key, l.created_at";

/// How many links one listing returns. The inbox pages by nothing yet (C7), and
/// an unbounded list is what turns a busy tenant's first render into a download.
pub const LIST_LIMIT: i64 = 200;

#[async_trait]
pub trait EmailLinkRepository: Send + Sync {
    /// Record the chain a delivery started.
    async fn create(&self, new: NewEmailLink) -> ApiResult<EmailLink>;

    /// Attach the investigate run, once it exists.
    async fn set_job(&self, tenant: TenantId, id: Uuid, job: JobId) -> ApiResult<()>;

    /// Attach the PR to every link on this card, and say how many there were.
    ///
    /// By CARD rather than by link id, because the caller is the outcome path:
    /// it knows a PR was opened against a ticket and has never heard of an
    /// email. A card with no link is the ordinary case and updates nothing.
    async fn set_pr_ref(&self, tenant: TenantId, task: TaskId, pr_ref: &str) -> ApiResult<u64>;

    /// The chain a `Message-Id` belongs to — the reply-threading lookup (C4).
    ///
    /// Newest first, because nothing dedupes deliveries on `message_id`: a
    /// replayed message files a second card, and the live chain is the latest.
    async fn by_message_id(
        &self,
        tenant: TenantId,
        viewer: UserId,
        message_id: &str,
    ) -> ApiResult<Option<EmailLink>>;

    /// The chain a card belongs to, newest first for the same reason.
    async fn by_task(
        &self,
        tenant: TenantId,
        viewer: UserId,
        task: TaskId,
    ) -> ApiResult<Option<EmailLink>>;

    /// The inbox listing (C7): this workspace's chains, newest first. `None`
    /// lists the tenant's, which is what a tenant that scopes its support mail
    /// to no repository has.
    async fn list(
        &self,
        tenant: TenantId,
        viewer: UserId,
        workspace: Option<WorkspaceId>,
    ) -> ApiResult<Vec<EmailLink>>;
}

pub struct DbEmailLinkRepository {
    db: DbPool,
}

impl DbEmailLinkRepository {
    pub fn new(db: DbPool) -> Self {
        Self { db }
    }
}

#[async_trait]
impl EmailLinkRepository for DbEmailLinkRepository {
    async fn create(&self, new: NewEmailLink) -> ApiResult<EmailLink> {
        let id = Uuid::now_v7();
        self.db
            .exec(
                "INSERT INTO email_links
                    (id, tenant_id, workspace_id, task_id, message_id, in_reply_to, storage_key)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
                params![
                    id,
                    new.tenant,
                    // `Option<WorkspaceId>` has no `IntoDbValue` of its own —
                    // the orphan rule — so it reaches the typed `Option<Uuid>`
                    // arm, as every other nullable id binding here does.
                    new.workspace.map(|w| w.0),
                    new.task,
                    new.message_id,
                    new.in_reply_to,
                    &new.storage_key
                ],
            )
            .await?;

        // Read back rather than RETURNING: a data-modifying CTE is Postgres
        // only and this runs on both engines, and `created_at` is the
        // database's to decide.
        Ok(self
            .db
            .query_one(
                &format!(
                    "SELECT {LINK_COLUMNS} FROM email_links l WHERE l.tenant_id = $1 AND l.id = $2"
                ),
                params![new.tenant, id],
            )
            .await?)
    }

    async fn set_job(&self, tenant: TenantId, id: Uuid, job: JobId) -> ApiResult<()> {
        self.db
            .exec(
                "UPDATE email_links SET loop_job_id = $1 WHERE tenant_id = $2 AND id = $3",
                params![job, tenant, id],
            )
            .await?;
        Ok(())
    }

    async fn set_pr_ref(&self, tenant: TenantId, task: TaskId, pr_ref: &str) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                "UPDATE email_links SET pr_ref = $1 WHERE tenant_id = $2 AND task_id = $3",
                params![pr_ref, tenant, task],
            )
            .await?)
    }

    async fn by_message_id(
        &self,
        tenant: TenantId,
        viewer: UserId,
        message_id: &str,
    ) -> ApiResult<Option<EmailLink>> {
        Ok(self
            .db
            .query_opt(
                &format!(
                    "SELECT {LINK_COLUMNS} FROM email_links l
                       JOIN tasks t ON t.id = l.task_id
                      WHERE l.tenant_id = $1 AND l.message_id = $3 AND {visible}
                      ORDER BY l.created_at DESC, l.id DESC
                      LIMIT 1",
                    visible = visible_sql("t", "$2"),
                ),
                params![tenant, viewer, message_id],
            )
            .await?)
    }

    async fn by_task(
        &self,
        tenant: TenantId,
        viewer: UserId,
        task: TaskId,
    ) -> ApiResult<Option<EmailLink>> {
        Ok(self
            .db
            .query_opt(
                &format!(
                    "SELECT {LINK_COLUMNS} FROM email_links l
                       JOIN tasks t ON t.id = l.task_id
                      WHERE l.tenant_id = $1 AND l.task_id = $3 AND {visible}
                      ORDER BY l.created_at DESC, l.id DESC
                      LIMIT 1",
                    visible = visible_sql("t", "$2"),
                ),
                params![tenant, viewer, task],
            )
            .await?)
    }

    async fn list(
        &self,
        tenant: TenantId,
        viewer: UserId,
        workspace: Option<WorkspaceId>,
    ) -> ApiResult<Vec<EmailLink>> {
        // `id` breaks the tie so the order is total: two deliveries recorded in
        // the same millisecond — SQLite's whole clock resolution — must not swap
        // places between two reads of the same inbox.
        let (scope, params) = match workspace {
            Some(ws) => ("AND l.workspace_id = $3", params![tenant, viewer, ws]),
            None => ("", params![tenant, viewer]),
        };
        // The cap is a constant of this file, never a caller's number, so it is
        // interpolated rather than bound — which keeps the two branches to one
        // bind order instead of two.
        Ok(self
            .db
            .query_all(
                &format!(
                    "SELECT {LINK_COLUMNS} FROM email_links l
                       JOIN tasks t ON t.id = l.task_id
                      WHERE l.tenant_id = $1 AND {visible} {scope}
                      ORDER BY l.created_at DESC, l.id DESC
                      LIMIT {LIST_LIMIT}",
                    visible = visible_sql("t", "$2"),
                ),
                params,
            )
            .await?)
    }
}
