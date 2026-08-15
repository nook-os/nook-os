//! The IMAP poller's configuration and the ledger of what it has ingested
//! (MAIN-333).
//!
//! Two tables, one aggregate: a mailbox and the record of which of its messages
//! this deployment has already decided about. They share a trait because they
//! share a lifetime — deleting a tenant's poller and forgetting what that
//! poller had seen are the same operation from anywhere but here.
//!
//! **Nothing in this module decrypts.** `password_enc` goes in and comes out as
//! the bytes `crypto::Vault` produced; the service layer holds the key. That is
//! what keeps a plaintext password out of every row struct here, and out of
//! anything that logs one.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use nook_db::{
    dialect::{time_math, type_mapping},
    params, Db, DbPool,
};
use nook_types::*;

use crate::error::ApiResult;

/// A poller as it is stored. Sealed credential included, which is why this type
/// is `Debug` by hand: a derived one would print the ciphertext into any log
/// that formatted a row, and ciphertext in a log is a fact about a secret that
/// nothing here needs.
#[derive(Clone, nook_db::FromDbRow)]
pub struct EmailPoller {
    pub tenant_id: TenantId,
    pub host: String,
    pub port: i32,
    pub username: String,
    pub password_enc: Vec<u8>,
    pub mailbox: String,
    pub poll_interval_secs: i32,
    pub enabled: bool,
    pub uid_validity: Option<i64>,
    pub last_uid: i64,
    pub last_polled_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

impl std::fmt::Debug for EmailPoller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmailPoller")
            .field("tenant_id", &self.tenant_id)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password_enc", &"<sealed>")
            .field("mailbox", &self.mailbox)
            .field("poll_interval_secs", &self.poll_interval_secs)
            .field("enabled", &self.enabled)
            .field("uid_validity", &self.uid_validity)
            .field("last_uid", &self.last_uid)
            .field("last_polled_at", &self.last_polled_at)
            .field("last_error", &self.last_error)
            .finish()
    }
}

/// What a caller is asking to store. The password arrives already sealed — the
/// route seals it before it reaches this layer, so no path exists that writes a
/// plaintext one by forgetting to.
#[derive(Clone)]
pub struct NewEmailPoller {
    pub tenant: TenantId,
    pub host: String,
    pub port: i32,
    pub username: String,
    pub password_enc: Vec<u8>,
    pub mailbox: String,
    pub poll_interval_secs: i32,
    pub enabled: bool,
}

const POLLER_COLUMNS: &str = "tenant_id, host, port, username, password_enc, mailbox, \
                              poll_interval_secs, enabled, uid_validity, last_uid, \
                              last_polled_at, last_error";

#[async_trait]
pub trait EmailPollerRepository: Send + Sync {
    /// Create or replace this tenant's poller.
    ///
    /// A re-put resets `uid_validity`/`last_uid`: the host, the mailbox or the
    /// account may have changed, and a watermark from the previous mailbox
    /// names messages in a mailbox that no longer exists. Starting over is
    /// cheap and correct — the message-id ledger is what stops it re-filing.
    async fn put(&self, new: NewEmailPoller) -> ApiResult<EmailPoller>;

    async fn get(&self, tenant: TenantId) -> ApiResult<Option<EmailPoller>>;

    /// Remove the poller. `false` when there was none, which the route turns
    /// into a 404 rather than a silent success.
    async fn delete(&self, tenant: TenantId) -> ApiResult<bool>;

    /// Claim every poller that is enabled and due, marking each polled in the
    /// same statement.
    ///
    /// Cross-tenant, like `job_dispatch`'s own sweep: this is one loop for the
    /// deployment. The claim is what makes it safe on every replica — the
    /// `last_polled_at` guard is inside the UPDATE, so exactly one replica gets
    /// a given row back and the rest get nothing. A poller whose run then dies
    /// with the process is simply due again one interval later, which is the
    /// right amount of harm for a poll.
    async fn claim_due(&self) -> ApiResult<Vec<EmailPoller>>;

    /// Record where the mailbox got to, and whether the poll worked.
    ///
    /// `uid_validity`/`last_uid` are only advanced on a run that read the
    /// mailbox; a failed poll passes `None` and leaves the watermark alone
    /// rather than stranding messages it never saw.
    async fn record_poll(
        &self,
        tenant: TenantId,
        watermark: Option<(i64, i64)>,
        error: Option<&str>,
    ) -> ApiResult<()>;

    /// Claim one message id for processing.
    ///
    /// `true` means this caller now owns it and must process it; `false` means
    /// somebody already has, and it must be skipped (AC-3). The insert IS the
    /// claim — see the migration for why recording afterwards would not do.
    async fn claim_message(
        &self,
        tenant: TenantId,
        source: &str,
        message_id: &str,
    ) -> ApiResult<bool>;

    /// Attach the card a claimed message became.
    async fn record_filed(
        &self,
        tenant: TenantId,
        source: &str,
        message_id: &str,
        task: TaskId,
    ) -> ApiResult<()>;

    /// Give a claim back, so a later poll may try the message again.
    ///
    /// Only for a failure that is about the moment rather than the message —
    /// the object store being down, the tenant having no board yet. A message
    /// that can never be accepted keeps its claim, or every poll for the rest
    /// of time re-runs the same refusal.
    async fn release_message(
        &self,
        tenant: TenantId,
        source: &str,
        message_id: &str,
    ) -> ApiResult<()>;
}

pub struct DbEmailPollerRepository {
    db: DbPool,
}

impl DbEmailPollerRepository {
    pub fn new(db: DbPool) -> Self {
        Self { db }
    }
}

#[async_trait]
impl EmailPollerRepository for DbEmailPollerRepository {
    async fn put(&self, new: NewEmailPoller) -> ApiResult<EmailPoller> {
        let now = type_mapping(self.db.engine()).now();
        self.db
            .exec(
                &format!(
                    "INSERT INTO email_pollers
                        (tenant_id, host, port, username, password_enc, mailbox,
                         poll_interval_secs, enabled, created_at, updated_at)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, {now}, {now})
                     ON CONFLICT (tenant_id) DO UPDATE SET
                        host = $2, port = $3, username = $4, password_enc = $5,
                        mailbox = $6, poll_interval_secs = $7, enabled = $8,
                        uid_validity = NULL, last_uid = 0, last_error = NULL,
                        updated_at = {now}"
                ),
                params![
                    new.tenant,
                    new.host,
                    new.port,
                    new.username,
                    new.password_enc,
                    new.mailbox,
                    new.poll_interval_secs,
                    new.enabled
                ],
            )
            .await?;
        // Read back rather than `RETURNING`, as `task_reports` does: a
        // data-modifying CTE is Postgres only and this runs on both engines.
        self.get(new.tenant)
            .await?
            .ok_or_else(|| crate::error::ApiError::Internal(anyhow::anyhow!("poller vanished")))
    }

    async fn get(&self, tenant: TenantId) -> ApiResult<Option<EmailPoller>> {
        self.db
            .query_opt(
                &format!("SELECT {POLLER_COLUMNS} FROM email_pollers WHERE tenant_id = $1"),
                params![tenant],
            )
            .await
            .map_err(Into::into)
    }

    async fn delete(&self, tenant: TenantId) -> ApiResult<bool> {
        Ok(self
            .db
            .exec(
                "DELETE FROM email_pollers WHERE tenant_id = $1",
                params![tenant],
            )
            .await?
            > 0)
    }

    async fn claim_due(&self) -> ApiResult<Vec<EmailPoller>> {
        // The interval is a per-row COLUMN, not a constant, which is what makes
        // `now_minus_scaled` the right seam here — it takes the count as a
        // composed expression precisely so a variable amount need not be
        // spliced as a literal.
        let now = type_mapping(self.db.engine()).now();
        let due = time_math(self.db.engine()).now_minus_scaled("poll_interval_secs", "1 second");
        self.db
            .query_all(
                &format!(
                    "UPDATE email_pollers SET last_polled_at = {now}
                      WHERE enabled AND (last_polled_at IS NULL OR last_polled_at <= {due})
                      RETURNING {POLLER_COLUMNS}"
                ),
                params![],
            )
            .await
            .map_err(Into::into)
    }

    async fn record_poll(
        &self,
        tenant: TenantId,
        watermark: Option<(i64, i64)>,
        error: Option<&str>,
    ) -> ApiResult<()> {
        let now = type_mapping(self.db.engine()).now();
        match watermark {
            Some((uid_validity, last_uid)) => {
                self.db
                    .exec(
                        &format!(
                            "UPDATE email_pollers
                                SET uid_validity = $2, last_uid = $3, last_error = $4,
                                    updated_at = {now}
                              WHERE tenant_id = $1"
                        ),
                        params![tenant, uid_validity, last_uid, error.map(str::to_string)],
                    )
                    .await?;
            }
            None => {
                self.db
                    .exec(
                        &format!(
                            "UPDATE email_pollers SET last_error = $2, updated_at = {now}
                              WHERE tenant_id = $1"
                        ),
                        params![tenant, error.map(str::to_string)],
                    )
                    .await?;
            }
        }
        Ok(())
    }

    async fn claim_message(
        &self,
        tenant: TenantId,
        source: &str,
        message_id: &str,
    ) -> ApiResult<bool> {
        Ok(self
            .db
            .exec(
                "INSERT INTO inbound_email_seen (tenant_id, source, message_id)
                 VALUES ($1, $2, $3)
                 ON CONFLICT (tenant_id, source, message_id) DO NOTHING",
                params![tenant, source.to_string(), message_id.to_string()],
            )
            .await?
            > 0)
    }

    async fn record_filed(
        &self,
        tenant: TenantId,
        source: &str,
        message_id: &str,
        task: TaskId,
    ) -> ApiResult<()> {
        self.db
            .exec(
                "UPDATE inbound_email_seen SET task_id = $4
                  WHERE tenant_id = $1 AND source = $2 AND message_id = $3",
                params![tenant, source.to_string(), message_id.to_string(), task],
            )
            .await?;
        Ok(())
    }

    async fn release_message(
        &self,
        tenant: TenantId,
        source: &str,
        message_id: &str,
    ) -> ApiResult<()> {
        self.db
            .exec(
                "DELETE FROM inbound_email_seen
                  WHERE tenant_id = $1 AND source = $2 AND message_id = $3",
                params![tenant, source.to_string(), message_id.to_string()],
            )
            .await?;
        Ok(())
    }
}
