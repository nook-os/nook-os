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
use chrono::{DateTime, Utc};
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
    /// The verified envelope sender — the staffer who forwarded the report, and
    /// the only address a `to_staffer` reply may go to (MAIN-332).
    pub staffer_address: String,
    /// The delivery's `Reply-To:`, when it carried one. See the migration for
    /// why that header and no other.
    pub customer_address: Option<String>,
    pub subject: String,
    pub storage_key: String,
}

/// The columns the API type is built from, spelled once because every read
/// selects exactly it.
///
/// **`draft_reply_enc` is present only as a NULL test**, and the test is
/// ALIASED because mapping is by name: unaliased, Postgres calls the column
/// `?column?` and every read of this table fails at runtime. The draft is customer
/// content and every read here is a listing or a lookup; selecting the
/// ciphertext into the shape those return would put it one careless `Debug` away
/// from a log. Nothing reads it back yet — the inbox that will is C7, and it can
/// add that read with the caller and the test that justify it.
const LINK_COLUMNS: &str = "l.id, l.workspace_id, l.task_id, l.loop_job_id, l.pr_ref, \
                            l.message_id, l.in_reply_to, l.staffer_address, \
                            l.customer_address, l.subject, l.reply_sent_at, \
                            l.reply_recipient, l.storage_key, l.findings, \
                            (l.draft_reply_enc IS NOT NULL) AS has_draft_reply, \
                            l.created_at";

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

    /// The chain a RUN is about (MAIN-331): what the investigate job seeded for
    /// this message is allowed to read and to write onto.
    ///
    /// The job id is the whole address, which is what confines the run: it
    /// cannot name a link, so it cannot reach another message's — and a job
    /// carrying no link (every kind but this one) resolves to `None` rather
    /// than to somebody else's row.
    async fn by_job(
        &self,
        tenant: TenantId,
        viewer: UserId,
        job: JobId,
    ) -> ApiResult<Option<EmailLink>>;

    /// Record what the investigation produced. The draft arrives already
    /// sealed — this layer stores bytes and never holds the vault.
    ///
    /// Returns how many rows moved, so a caller can tell "recorded" from "no
    /// such chain" without a second read.
    async fn set_investigation(
        &self,
        tenant: TenantId,
        id: Uuid,
        findings: &str,
        draft_reply_enc: Vec<u8>,
    ) -> ApiResult<u64>;

    /// One chain by its own id — what the approve action holds (MAIN-332).
    async fn by_id(
        &self,
        tenant: TenantId,
        viewer: UserId,
        id: Uuid,
    ) -> ApiResult<Option<EmailLink>>;

    /// The sealed draft itself, which no other read returns.
    ///
    /// The caller is the send path and nothing else: it needs the words, and
    /// the vault to unseal them lives a layer up. Keeping it off
    /// [`LINK_COLUMNS`] is what stops the ciphertext riding along on every
    /// listing — see that constant.
    async fn draft_reply_enc(&self, tenant: TenantId, id: Uuid) -> ApiResult<Option<Vec<u8>>>;

    /// Replace the sealed draft, for a human who edited it before approving.
    /// The findings are untouched: an edited reply is not a re-investigation.
    async fn set_draft_reply(
        &self,
        tenant: TenantId,
        id: Uuid,
        draft_reply_enc: Vec<u8>,
    ) -> ApiResult<u64>;

    /// Take the ONE reply this chain gets, naming who it goes to (MAIN-332
    /// AC-5). Returns how many rows moved — `0` means somebody already has it.
    ///
    /// Conditional on `reply_sent_at IS NULL`, and taken BEFORE the transport
    /// is asked, because the thing being protected is a customer's inbox: two
    /// approves a second apart would both pass a read-then-send check and the
    /// person would get the same reply twice. A send that then fails calls
    /// [`release_reply`](EmailLinkRepository::release_reply).
    async fn claim_reply(
        &self,
        tenant: TenantId,
        id: Uuid,
        recipient: &str,
        sent_at: DateTime<Utc>,
    ) -> ApiResult<u64>;

    /// Give the claim back: the transport refused, so nothing went and the
    /// chain must be replyable again.
    async fn release_reply(&self, tenant: TenantId, id: Uuid) -> ApiResult<()>;
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
                    (id, tenant_id, workspace_id, task_id, message_id, in_reply_to,
                     staffer_address, customer_address, subject, storage_key)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
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
                    &new.staffer_address,
                    new.customer_address,
                    &new.subject,
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

    async fn by_job(
        &self,
        tenant: TenantId,
        viewer: UserId,
        job: JobId,
    ) -> ApiResult<Option<EmailLink>> {
        Ok(self
            .db
            .query_opt(
                &format!(
                    "SELECT {LINK_COLUMNS} FROM email_links l
                       JOIN tasks t ON t.id = l.task_id
                      WHERE l.tenant_id = $1 AND l.loop_job_id = $3 AND {visible}
                      LIMIT 1",
                    visible = visible_sql("t", "$2"),
                ),
                params![tenant, viewer, job],
            )
            .await?)
    }

    async fn set_investigation(
        &self,
        tenant: TenantId,
        id: Uuid,
        findings: &str,
        draft_reply_enc: Vec<u8>,
    ) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                "UPDATE email_links SET findings = $3, draft_reply_enc = $4
                  WHERE tenant_id = $1 AND id = $2",
                params![tenant, id, findings, draft_reply_enc],
            )
            .await?)
    }

    async fn by_id(
        &self,
        tenant: TenantId,
        viewer: UserId,
        id: Uuid,
    ) -> ApiResult<Option<EmailLink>> {
        Ok(self
            .db
            .query_opt(
                &format!(
                    "SELECT {LINK_COLUMNS} FROM email_links l
                       JOIN tasks t ON t.id = l.task_id
                      WHERE l.tenant_id = $1 AND l.id = $3 AND {visible}",
                    visible = visible_sql("t", "$2"),
                ),
                params![tenant, viewer, id],
            )
            .await?)
    }

    async fn draft_reply_enc(&self, tenant: TenantId, id: Uuid) -> ApiResult<Option<Vec<u8>>> {
        Ok(self
            .db
            .query_scalar_opt::<Option<Vec<u8>>>(
                "SELECT draft_reply_enc FROM email_links WHERE tenant_id = $1 AND id = $2",
                params![tenant, id],
            )
            .await?
            // The outer `Option` is "no such chain", the inner is "no draft
            // yet" — the caller's refusal is the same either way.
            .flatten())
    }

    async fn set_draft_reply(
        &self,
        tenant: TenantId,
        id: Uuid,
        draft_reply_enc: Vec<u8>,
    ) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                "UPDATE email_links SET draft_reply_enc = $3
                  WHERE tenant_id = $1 AND id = $2",
                params![tenant, id, draft_reply_enc],
            )
            .await?)
    }

    async fn claim_reply(
        &self,
        tenant: TenantId,
        id: Uuid,
        recipient: &str,
        sent_at: DateTime<Utc>,
    ) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                "UPDATE email_links SET reply_sent_at = $3, reply_recipient = $4
                  WHERE tenant_id = $1 AND id = $2 AND reply_sent_at IS NULL",
                params![tenant, id, sent_at, recipient],
            )
            .await?)
    }

    async fn release_reply(&self, tenant: TenantId, id: Uuid) -> ApiResult<()> {
        self.db
            .exec(
                "UPDATE email_links SET reply_sent_at = NULL, reply_recipient = NULL
                  WHERE tenant_id = $1 AND id = $2",
                params![tenant, id],
            )
            .await?;
        Ok(())
    }
}
