//! Notification and feedback data access (MAIN-256).
//!
//! - [`NotificationRepository`] — `notifications` (the inbox) and
//!   `notification_channels` (where they fan out to).
//! - [`FeedbackRepository`] — `feedback`, plus the three `settings` rows that
//!   configure the feedback surface.
//!
//! **Two reads of `notification_channels`, deliberately.**
//! [`NotificationRepository::list_channels`] returns [`NotificationChannel`],
//! which has no `config` and no `secret` — it is what the settings page
//! renders. [`NotificationRepository::enabled_channels`] and
//! [`NotificationRepository::channel_target`] return [`ChannelTarget`], which
//! carries both, and exist only for the fan-out and the test-send. Keeping
//! them apart is what stops a signing secret riding along on the endpoint the
//! browser calls; merging them would be one `SELECT *` away from a leak.
//!
//! **Why `settings` lives here and is not a passthrough.** `settings` is a
//! generic key/value table each aggregate keeps its own keys in — the loops
//! switch owns `loops.enabled`, and the feedback surface owns
//! `feedback_workspace_id`, `feedback_branch` and `feedback_instructions`. The
//! trait takes a [`FeedbackSetting`] rather than a `&str`, so the key strings
//! stay inside this file and no caller can reach another aggregate's row
//! through it. That is the difference between owning three settings and being
//! a generic settings API (AC-1's "no raw-SQL passthrough").
//!
//! Channel delivery — ntfy, SMTP — stays on its provider (AC-1); only the DB
//! access moved.

use async_trait::async_trait;
use nook_db::dialect::type_mapping;
use nook_db::{params, Db, DbPool, Postgres, TypeMapping};
use nook_types::*;
use serde_json::Value;
use uuid::Uuid;

use crate::error::ApiResult;

/// A notification to raise.
#[derive(Debug, Clone)]
pub struct NewNotification {
    pub tenant: TenantId,
    /// `None` is tenant-wide — everyone sees it.
    pub user_id: Option<Uuid>,
    pub level: String,
    pub title: String,
    pub body: String,
    pub kind: String,
    pub link: Option<String>,
    pub payload: Value,
}

/// A channel as the dispatcher needs it — including `config` and `secret`,
/// which is why it is returned only by the two send paths.
///
/// Plainly derived since MAIN-327. It used to carry a hand-written `FromRow`
/// pair — a real `PgRow` impl and a `SqliteRow` one that returned an error —
/// because `levels`/`kinds` are Postgres `text[]`, which has no SQLite `Decode`,
/// and the dispatch pool bound its fetchers on both arms. The engine-neutral
/// mapper gives `text[]` an actual SQLite representation, so there is nothing
/// left here to hand-write.
#[derive(Debug, Clone, nook_db::FromDbRow)]
pub struct ChannelTarget {
    pub id: Uuid,
    pub kind: String,
    pub config: Value,
    pub levels: Vec<String>,
    pub kinds: Vec<String>,
    pub secret: Option<String>,
}

/// A new channel. `secret` is write-only — no read that the UI calls returns it.
#[derive(Debug, Clone)]
pub struct NewChannel {
    pub tenant: TenantId,
    pub kind: String,
    pub name: String,
    pub config: Value,
    pub levels: Vec<String>,
    pub kinds: Vec<String>,
    pub secret: Option<String>,
}

/// A partial channel edit; `None` leaves the column alone. `config` omitted
/// therefore keeps the stored secrets — a UI that cannot read them back must
/// be able to rename a channel without blanking the token it never saw.
#[derive(Debug, Clone, Default)]
pub struct ChannelEdit {
    pub name: Option<String>,
    pub config: Option<Value>,
    pub enabled: Option<bool>,
    pub levels: Option<Vec<String>>,
    pub kinds: Option<Vec<String>>,
}

/// Which notifications a list call wants.
#[derive(Debug, Clone, Default)]
pub struct NotificationFilter {
    pub unread_only: bool,
    pub limit: i64,
}

/// The three settings the feedback surface owns. An enum rather than a `&str`
/// so no caller can reach another aggregate's key through this trait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackSetting {
    Workspace,
    Branch,
    Instructions,
}

impl FeedbackSetting {
    fn key(self) -> &'static str {
        match self {
            FeedbackSetting::Workspace => "feedback_workspace_id",
            FeedbackSetting::Branch => "feedback_branch",
            FeedbackSetting::Instructions => "feedback_instructions",
        }
    }
}

/// The `notifications` columns every read returns, so two SELECTs cannot drift.
const NOTIFICATION_COLUMNS: &str =
    "id, tenant_id, user_id, level, title, body, kind, link, payload, read_at, created_at";

/// The `notification_channels` columns safe to hand back — note the absence of
/// `config` and `secret`.
const CHANNEL_COLUMNS: &str = "id, tenant_id, kind, name, enabled, levels, kinds, \
     last_ok_at, last_error, created_at, updated_at";

#[async_trait]
pub trait NotificationRepository: Send + Sync {
    /// The inbox for one person: tenant-wide rows plus those addressed to
    /// them, never somebody else's.
    async fn list(
        &self,
        tenant: TenantId,
        user: UserId,
        filter: NotificationFilter,
    ) -> ApiResult<Vec<Notification>>;

    async fn unread_count(&self, tenant: TenantId, user: UserId) -> ApiResult<i64>;

    async fn raise(&self, new: NewNotification) -> ApiResult<Notification>;

    /// Mark one as read. Guarded on `read_at IS NULL` so re-reading does not
    /// move the timestamp.
    async fn mark_read(&self, id: Uuid, tenant: TenantId) -> ApiResult<u64>;

    /// Mark everything this person can see as read.
    async fn mark_all_read(&self, tenant: TenantId, user: UserId) -> ApiResult<u64>;

    async fn clear(&self, tenant: TenantId, user: UserId) -> ApiResult<u64>;

    // ── channels ────────────────────────────────────────────────────────────

    /// For the settings page. Returns no `config` and no `secret`.
    async fn list_channels(&self, tenant: TenantId) -> ApiResult<Vec<NotificationChannel>>;

    /// Enabled channels with everything needed to send, secret included.
    async fn enabled_channels(&self, tenant: TenantId) -> ApiResult<Vec<ChannelTarget>>;

    /// One channel with its secret, for the test-send.
    async fn channel_target(&self, id: Uuid, tenant: TenantId) -> ApiResult<Option<ChannelTarget>>;

    async fn create_channel(&self, new: NewChannel) -> ApiResult<NotificationChannel>;

    async fn update_channel(
        &self,
        id: Uuid,
        tenant: TenantId,
        edit: ChannelEdit,
    ) -> ApiResult<Option<NotificationChannel>>;

    async fn delete_channel(&self, id: Uuid, tenant: TenantId) -> ApiResult<u64>;

    /// Record whether a send worked, so the settings page can show a channel
    /// that has quietly stopped working. A failure never clears `last_ok_at` —
    /// "it worked at 09:00 and has failed since" is the useful shape.
    async fn record_outcome(&self, id: Uuid, ok: bool, error: Option<&str>) -> ApiResult<()>;
}

#[async_trait]
pub trait FeedbackRepository: Send + Sync {
    async fn list(&self, tenant: TenantId) -> ApiResult<Vec<FeedbackItem>>;

    async fn submit(
        &self,
        tenant: TenantId,
        workspace: Option<WorkspaceId>,
        session: Option<SessionId>,
        body: &str,
        created_by: UserId,
    ) -> ApiResult<FeedbackItem>;

    async fn set_status(&self, id: Uuid, status: &str) -> ApiResult<Option<FeedbackItem>>;

    async fn update(
        &self,
        id: Uuid,
        tenant: TenantId,
        status: Option<String>,
        pr_url: Option<String>,
    ) -> ApiResult<Option<FeedbackItem>>;

    /// A per-user setting, falling back to the tenant-wide one.
    async fn setting(
        &self,
        tenant: TenantId,
        user: UserId,
        which: FeedbackSetting,
    ) -> ApiResult<Option<String>>;

    async fn set_setting(
        &self,
        tenant: TenantId,
        user: UserId,
        which: FeedbackSetting,
        value: Value,
    ) -> ApiResult<()>;
}

// ── the DbPool implementations ──────────────────────────────────────────────

pub struct DbNotificationRepository {
    db: DbPool,
}

impl DbNotificationRepository {
    pub fn new(db: DbPool) -> Self {
        Self { db }
    }
}

#[async_trait]
impl NotificationRepository for DbNotificationRepository {
    async fn list(
        &self,
        tenant: TenantId,
        user: UserId,
        filter: NotificationFilter,
    ) -> ApiResult<Vec<Notification>> {
        Ok(self
            .db
            .query_all(
                &format!(
                    "SELECT {NOTIFICATION_COLUMNS}
                     FROM notifications
                     WHERE tenant_id = $1
                       -- Tenant-wide (user_id IS NULL) or addressed to this
                       -- person. Never somebody else's.
                       AND (user_id IS NULL OR user_id = $2)
                       AND (NOT {} OR read_at IS NULL)
                     ORDER BY created_at DESC
                     LIMIT $4",
                    Postgres.cast("$3", "bool")
                ),
                params![tenant, user.0, filter.unread_only, filter.limit],
            )
            .await?)
    }

    async fn unread_count(&self, tenant: TenantId, user: UserId) -> ApiResult<i64> {
        Ok(self
            .db
            .query_scalar::<i64>(
                "SELECT count(*) FROM notifications
                 WHERE tenant_id = $1 AND (user_id IS NULL OR user_id = $2)
                   AND read_at IS NULL",
                params![tenant, user.0],
            )
            .await?)
    }

    async fn raise(&self, new: NewNotification) -> ApiResult<Notification> {
        Ok(self
            .db
            .query_one(
                &format!(
                    "INSERT INTO notifications
                        (id, tenant_id, user_id, level, title, body, kind, link, payload)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                     RETURNING {NOTIFICATION_COLUMNS}"
                ),
                params![
                    Uuid::now_v7(),
                    new.tenant,
                    new.user_id,
                    new.level,
                    new.title,
                    new.body,
                    new.kind,
                    new.link,
                    new.payload
                ],
            )
            .await?)
    }

    async fn mark_read(&self, id: Uuid, tenant: TenantId) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                &format!(
                    "UPDATE notifications SET read_at = {}
                     WHERE id = $1 AND tenant_id = $2 AND read_at IS NULL",
                    type_mapping(self.db.engine()).now()
                ),
                params![id, tenant],
            )
            .await?)
    }

    async fn mark_all_read(&self, tenant: TenantId, user: UserId) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                &format!(
                    "UPDATE notifications SET read_at = {}
                     WHERE tenant_id = $1 AND (user_id IS NULL OR user_id = $2)
                       AND read_at IS NULL",
                    type_mapping(self.db.engine()).now()
                ),
                params![tenant, user.0],
            )
            .await?)
    }

    async fn clear(&self, tenant: TenantId, user: UserId) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                "DELETE FROM notifications
                 WHERE tenant_id = $1 AND (user_id IS NULL OR user_id = $2)",
                params![tenant, user.0],
            )
            .await?)
    }

    async fn list_channels(&self, tenant: TenantId) -> ApiResult<Vec<NotificationChannel>> {
        Ok(self
            .db
            .query_all(
                &format!(
                    "SELECT {CHANNEL_COLUMNS}
                     FROM notification_channels WHERE tenant_id = $1 ORDER BY name"
                ),
                params![tenant],
            )
            .await?)
    }

    async fn enabled_channels(&self, tenant: TenantId) -> ApiResult<Vec<ChannelTarget>> {
        Ok(self
            .db
            .query_all(
                "SELECT id, kind, config, levels, kinds, secret FROM notification_channels
                 WHERE tenant_id = $1 AND enabled",
                params![tenant],
            )
            .await?)
    }

    async fn channel_target(&self, id: Uuid, tenant: TenantId) -> ApiResult<Option<ChannelTarget>> {
        Ok(self
            .db
            .query_opt(
                "SELECT id, kind, config, levels, kinds, secret FROM notification_channels
                 WHERE id = $1 AND tenant_id = $2",
                params![id, tenant],
            )
            .await?)
    }

    async fn create_channel(&self, new: NewChannel) -> ApiResult<NotificationChannel> {
        Ok(self
            .db
            .query_one(
                &format!(
                    "INSERT INTO notification_channels
                        (id, tenant_id, kind, name, config, levels, kinds, secret)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                     RETURNING {CHANNEL_COLUMNS}"
                ),
                params![
                    Uuid::now_v7(),
                    new.tenant,
                    new.kind,
                    new.name,
                    new.config,
                    // `Some(..)` deliberately: a bare `Vec<String>` binds as
                    // `DbValue::TextList`, which is the `= ANY($n)` operand and
                    // gets expanded into one placeholder per element on SQLite.
                    // A text[] COLUMN value is the OptTextArray arm — one bind,
                    // written as JSON on SQLite (MAIN-327 AC-4).
                    Some(new.levels),
                    Some(new.kinds),
                    new.secret
                ],
            )
            .await?)
    }

    async fn update_channel(
        &self,
        id: Uuid,
        tenant: TenantId,
        edit: ChannelEdit,
    ) -> ApiResult<Option<NotificationChannel>> {
        Ok(self
            .db
            .query_opt(
                &format!(
                    "UPDATE notification_channels SET
                        name = COALESCE($3, name),
                        config = COALESCE($4, config),
                        enabled = COALESCE($5, enabled),
                        levels = COALESCE($6, levels),
                        kinds = COALESCE($7, kinds),
                        updated_at = {}
                     WHERE id = $1 AND tenant_id = $2
                     RETURNING {CHANNEL_COLUMNS}",
                    type_mapping(self.db.engine()).now()
                ),
                params![
                    id,
                    tenant,
                    edit.name,
                    edit.config,
                    edit.enabled,
                    edit.levels,
                    edit.kinds
                ],
            )
            .await?)
    }

    async fn delete_channel(&self, id: Uuid, tenant: TenantId) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                "DELETE FROM notification_channels WHERE id = $1 AND tenant_id = $2",
                params![id, tenant],
            )
            .await?)
    }

    async fn record_outcome(&self, id: Uuid, ok: bool, error: Option<&str>) -> ApiResult<()> {
        self.db
            .exec(
                &format!(
                    "UPDATE notification_channels
                     SET last_ok_at = CASE WHEN $2 THEN {now} ELSE last_ok_at END,
                         last_error = $3,
                         updated_at = {now}
                     WHERE id = $1",
                    now = type_mapping(self.db.engine()).now()
                ),
                params![id, ok, error],
            )
            .await?;
        Ok(())
    }
}

pub struct DbFeedbackRepository {
    db: DbPool,
}

impl DbFeedbackRepository {
    pub fn new(db: DbPool) -> Self {
        Self { db }
    }
}

#[async_trait]
impl FeedbackRepository for DbFeedbackRepository {
    async fn list(&self, tenant: TenantId) -> ApiResult<Vec<FeedbackItem>> {
        Ok(self
            .db
            .query_all(
                "SELECT * FROM feedback WHERE tenant_id = $1 ORDER BY created_at DESC LIMIT 200",
                params![tenant],
            )
            .await?)
    }

    async fn submit(
        &self,
        tenant: TenantId,
        workspace: Option<WorkspaceId>,
        session: Option<SessionId>,
        body: &str,
        created_by: UserId,
    ) -> ApiResult<FeedbackItem> {
        Ok(self
            .db
            .query_one(
                "INSERT INTO feedback
                    (id, tenant_id, workspace_id, session_id, body, status, created_by)
                 VALUES ($1, $2, $3, $4, $5, 'queued', $6) RETURNING *",
                params![
                    Uuid::now_v7(),
                    tenant,
                    workspace.map(|w| w.0),
                    session.map(|s| s.0),
                    body,
                    created_by
                ],
            )
            .await?)
    }

    async fn set_status(&self, id: Uuid, status: &str) -> ApiResult<Option<FeedbackItem>> {
        Ok(self
            .db
            .query_opt(
                &format!(
                    "UPDATE feedback SET status = $2, updated_at = {} WHERE id = $1 RETURNING *",
                    type_mapping(self.db.engine()).now()
                ),
                params![id, status],
            )
            .await?)
    }

    async fn update(
        &self,
        id: Uuid,
        tenant: TenantId,
        status: Option<String>,
        pr_url: Option<String>,
    ) -> ApiResult<Option<FeedbackItem>> {
        Ok(self
            .db
            .query_opt(
                &format!(
                    "UPDATE feedback SET
                        status = COALESCE($3, status),
                        pr_url = COALESCE($4, pr_url),
                        updated_at = {}
                     WHERE id = $1 AND tenant_id = $2 RETURNING *",
                    type_mapping(self.db.engine()).now()
                ),
                params![id, tenant, status, pr_url],
            )
            .await?)
    }

    async fn setting(
        &self,
        tenant: TenantId,
        user: UserId,
        which: FeedbackSetting,
    ) -> ApiResult<Option<String>> {
        let row: Option<(Value,)> = self
            .db
            .query_opt(
                "SELECT value FROM settings
                 WHERE tenant_id = $1 AND key = $2
                   AND (user_id = $3 OR user_id IS NULL)
                 ORDER BY (user_id = $3) DESC LIMIT 1",
                params![tenant, which.key(), user],
            )
            .await?;
        Ok(row.and_then(|(v,)| v.as_str().map(str::to_string)))
    }

    async fn set_setting(
        &self,
        tenant: TenantId,
        user: UserId,
        which: FeedbackSetting,
        value: Value,
    ) -> ApiResult<()> {
        self.db
            .exec(
                "INSERT INTO settings (id, tenant_id, scope, user_id, key, value)
                 VALUES ($1, $2, 'user', $3, $4, $5)
                 ON CONFLICT (tenant_id, scope, user_id, key)
                 DO UPDATE SET value = EXCLUDED.value",
                params![SettingId::new().0, tenant, user, which.key(), value],
            )
            .await?;
        Ok(())
    }
}

// ── in-memory fakes (AC-3) ──────────────────────────────────────────────────
//
// Enough behavior that a caller test is worth trusting: the "mine or everyone's"
// inbox scope, the `read_at IS NULL` guard, the COALESCE that lets a rename keep
// a secret the UI never saw, and the two channel reads differing in what they
// hand back.

use std::sync::Mutex;

#[derive(Default)]
struct FakeNotifyState {
    inbox: Vec<Notification>,
    /// The channel plus the columns `NotificationChannel` deliberately omits.
    channels: Vec<(NotificationChannel, Value, Option<String>)>,
}

#[derive(Default)]
pub struct FakeNotificationRepository {
    inner: Mutex<FakeNotifyState>,
}

impl FakeNotificationRepository {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_read(&self, id: Uuid) -> Option<bool> {
        self.inner
            .lock()
            .unwrap()
            .inbox
            .iter()
            .find(|n| n.id == id)
            .map(|n| n.read_at.is_some())
    }

    pub fn inbox_len(&self) -> usize {
        self.inner.lock().unwrap().inbox.len()
    }

    /// The stored secret, so a test can prove an edit did not blank it.
    pub fn secret_of(&self, id: Uuid) -> Option<Option<String>> {
        self.inner
            .lock()
            .unwrap()
            .channels
            .iter()
            .find(|(c, _, _)| c.id == id)
            .map(|(_, _, s)| s.clone())
    }

    pub fn config_of(&self, id: Uuid) -> Option<Value> {
        self.inner
            .lock()
            .unwrap()
            .channels
            .iter()
            .find(|(c, _, _)| c.id == id)
            .map(|(_, cfg, _)| cfg.clone())
    }
}

#[async_trait]
impl NotificationRepository for FakeNotificationRepository {
    async fn list(
        &self,
        tenant: TenantId,
        user: UserId,
        filter: NotificationFilter,
    ) -> ApiResult<Vec<Notification>> {
        let s = self.inner.lock().unwrap();
        let mut out: Vec<Notification> = s
            .inbox
            .iter()
            .filter(|n| n.tenant_id == tenant)
            // Tenant-wide (NULL) or addressed to this person. Never somebody
            // else's.
            .filter(|n| n.user_id.is_none() || n.user_id == Some(user.0))
            .filter(|n| !filter.unread_only || n.read_at.is_none())
            .cloned()
            .collect();
        out.sort_by_key(|n| std::cmp::Reverse(n.created_at));
        out.truncate(filter.limit.max(0) as usize);
        Ok(out)
    }

    async fn unread_count(&self, tenant: TenantId, user: UserId) -> ApiResult<i64> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .inbox
            .iter()
            .filter(|n| {
                n.tenant_id == tenant
                    && (n.user_id.is_none() || n.user_id == Some(user.0))
                    && n.read_at.is_none()
            })
            .count() as i64)
    }

    async fn raise(&self, new: NewNotification) -> ApiResult<Notification> {
        let n = Notification {
            id: Uuid::now_v7(),
            tenant_id: new.tenant,
            user_id: new.user_id,
            level: new.level,
            title: new.title,
            body: new.body,
            kind: new.kind,
            link: new.link,
            payload: new.payload,
            read_at: None,
            created_at: chrono::Utc::now(),
        };
        self.inner.lock().unwrap().inbox.push(n.clone());
        Ok(n)
    }

    async fn mark_read(&self, id: Uuid, tenant: TenantId) -> ApiResult<u64> {
        let mut s = self.inner.lock().unwrap();
        Ok(
            // `AND read_at IS NULL`: re-reading does not move the timestamp.
            match s
                .inbox
                .iter_mut()
                .find(|n| n.id == id && n.tenant_id == tenant && n.read_at.is_none())
            {
                Some(n) => {
                    n.read_at = Some(chrono::Utc::now());
                    1
                }
                None => 0,
            },
        )
    }

    async fn mark_all_read(&self, tenant: TenantId, user: UserId) -> ApiResult<u64> {
        let mut s = self.inner.lock().unwrap();
        let now = chrono::Utc::now();
        let mut n = 0;
        for x in s.inbox.iter_mut() {
            if x.tenant_id == tenant
                && (x.user_id.is_none() || x.user_id == Some(user.0))
                && x.read_at.is_none()
            {
                x.read_at = Some(now);
                n += 1;
            }
        }
        Ok(n)
    }

    async fn clear(&self, tenant: TenantId, user: UserId) -> ApiResult<u64> {
        let mut s = self.inner.lock().unwrap();
        let before = s.inbox.len();
        s.inbox.retain(|n| {
            !(n.tenant_id == tenant && (n.user_id.is_none() || n.user_id == Some(user.0)))
        });
        Ok((before - s.inbox.len()) as u64)
    }

    async fn list_channels(&self, tenant: TenantId) -> ApiResult<Vec<NotificationChannel>> {
        let s = self.inner.lock().unwrap();
        let mut out: Vec<NotificationChannel> = s
            .channels
            .iter()
            .filter(|(c, _, _)| c.tenant_id == tenant)
            .map(|(c, _, _)| c.clone())
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn enabled_channels(&self, tenant: TenantId) -> ApiResult<Vec<ChannelTarget>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .channels
            .iter()
            .filter(|(c, _, _)| c.tenant_id == tenant && c.enabled)
            .map(|(c, cfg, secret)| ChannelTarget {
                id: c.id,
                kind: c.kind.clone(),
                config: cfg.clone(),
                levels: c.levels.clone(),
                kinds: c.kinds.clone(),
                secret: secret.clone(),
            })
            .collect())
    }

    async fn channel_target(&self, id: Uuid, tenant: TenantId) -> ApiResult<Option<ChannelTarget>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .channels
            .iter()
            .find(|(c, _, _)| c.id == id && c.tenant_id == tenant)
            .map(|(c, cfg, secret)| ChannelTarget {
                id: c.id,
                kind: c.kind.clone(),
                config: cfg.clone(),
                levels: c.levels.clone(),
                kinds: c.kinds.clone(),
                secret: secret.clone(),
            }))
    }

    async fn create_channel(&self, new: NewChannel) -> ApiResult<NotificationChannel> {
        let now = chrono::Utc::now();
        let c = NotificationChannel {
            id: Uuid::now_v7(),
            tenant_id: new.tenant,
            kind: new.kind,
            name: new.name,
            enabled: true,
            levels: new.levels,
            kinds: new.kinds,
            last_ok_at: None,
            last_error: None,
            created_at: now,
            updated_at: now,
        };
        self.inner
            .lock()
            .unwrap()
            .channels
            .push((c.clone(), new.config, new.secret));
        Ok(c)
    }

    async fn update_channel(
        &self,
        id: Uuid,
        tenant: TenantId,
        edit: ChannelEdit,
    ) -> ApiResult<Option<NotificationChannel>> {
        let mut s = self.inner.lock().unwrap();
        Ok(s.channels
            .iter_mut()
            .find(|(c, _, _)| c.id == id && c.tenant_id == tenant)
            .map(|(c, cfg, _)| {
                // COALESCE: a None leaves the column — and, for `config`, the
                // secrets inside it — exactly as they were.
                if let Some(n) = edit.name {
                    c.name = n;
                }
                if let Some(v) = edit.config {
                    *cfg = v;
                }
                if let Some(e) = edit.enabled {
                    c.enabled = e;
                }
                if let Some(l) = edit.levels {
                    c.levels = l;
                }
                if let Some(k) = edit.kinds {
                    c.kinds = k;
                }
                c.updated_at = chrono::Utc::now();
                c.clone()
            }))
    }

    async fn delete_channel(&self, id: Uuid, tenant: TenantId) -> ApiResult<u64> {
        let mut s = self.inner.lock().unwrap();
        let before = s.channels.len();
        s.channels
            .retain(|(c, _, _)| !(c.id == id && c.tenant_id == tenant));
        Ok((before - s.channels.len()) as u64)
    }

    async fn record_outcome(&self, id: Uuid, ok: bool, error: Option<&str>) -> ApiResult<()> {
        let mut s = self.inner.lock().unwrap();
        if let Some((c, _, _)) = s.channels.iter_mut().find(|(c, _, _)| c.id == id) {
            // A failure never clears `last_ok_at` — the CASE only writes it on
            // success, so "worked at 09:00, failing since" stays legible.
            if ok {
                c.last_ok_at = Some(chrono::Utc::now());
            }
            c.last_error = error.map(str::to_string);
            c.updated_at = chrono::Utc::now();
        }
        Ok(())
    }
}

#[derive(Default)]
struct FakeFeedbackState {
    items: Vec<FeedbackItem>,
    /// (tenant, user, key) → value. A `None` user is the tenant-wide row.
    settings: Vec<(TenantId, Option<UserId>, &'static str, Value)>,
}

#[derive(Default)]
pub struct FakeFeedbackRepository {
    inner: Mutex<FakeFeedbackState>,
}

impl FakeFeedbackRepository {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the tenant-wide fallback the per-user read falls back to.
    pub fn set_tenant_setting(&self, tenant: TenantId, which: FeedbackSetting, value: Value) {
        self.inner
            .lock()
            .unwrap()
            .settings
            .push((tenant, None, which.key(), value));
    }

    pub fn status_of(&self, id: Uuid) -> Option<String> {
        self.inner
            .lock()
            .unwrap()
            .items
            .iter()
            .find(|i| i.id == id)
            .map(|i| i.status.clone())
    }
}

#[async_trait]
impl FeedbackRepository for FakeFeedbackRepository {
    async fn list(&self, tenant: TenantId) -> ApiResult<Vec<FeedbackItem>> {
        let s = self.inner.lock().unwrap();
        let mut out: Vec<FeedbackItem> = s
            .items
            .iter()
            .filter(|i| i.tenant_id == tenant)
            .cloned()
            .collect();
        out.sort_by_key(|i| std::cmp::Reverse(i.created_at));
        out.truncate(200);
        Ok(out)
    }

    async fn submit(
        &self,
        tenant: TenantId,
        workspace: Option<WorkspaceId>,
        session: Option<SessionId>,
        body: &str,
        created_by: UserId,
    ) -> ApiResult<FeedbackItem> {
        let item = FeedbackItem {
            id: Uuid::now_v7(),
            tenant_id: tenant,
            workspace_id: workspace,
            session_id: session,
            body: body.to_string(),
            status: "queued".into(),
            pr_url: None,
            created_by: Some(created_by),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        self.inner.lock().unwrap().items.push(item.clone());
        Ok(item)
    }

    async fn set_status(&self, id: Uuid, status: &str) -> ApiResult<Option<FeedbackItem>> {
        let mut s = self.inner.lock().unwrap();
        Ok(s.items.iter_mut().find(|i| i.id == id).map(|i| {
            i.status = status.to_string();
            i.updated_at = chrono::Utc::now();
            i.clone()
        }))
    }

    async fn update(
        &self,
        id: Uuid,
        tenant: TenantId,
        status: Option<String>,
        pr_url: Option<String>,
    ) -> ApiResult<Option<FeedbackItem>> {
        let mut s = self.inner.lock().unwrap();
        Ok(s.items
            .iter_mut()
            .find(|i| i.id == id && i.tenant_id == tenant)
            .map(|i| {
                if let Some(st) = status {
                    i.status = st;
                }
                if let Some(u) = pr_url {
                    i.pr_url = Some(u);
                }
                i.updated_at = chrono::Utc::now();
                i.clone()
            }))
    }

    async fn setting(
        &self,
        tenant: TenantId,
        user: UserId,
        which: FeedbackSetting,
    ) -> ApiResult<Option<String>> {
        let s = self.inner.lock().unwrap();
        let key = which.key();
        // `ORDER BY (user_id = $3) DESC LIMIT 1`: the person's own row wins,
        // the tenant-wide one is the fallback.
        let mine = s
            .settings
            .iter()
            .find(|(t, u, k, _)| *t == tenant && *u == Some(user) && *k == key);
        let shared = s
            .settings
            .iter()
            .find(|(t, u, k, _)| *t == tenant && u.is_none() && *k == key);
        Ok(mine
            .or(shared)
            .and_then(|(_, _, _, v)| v.as_str().map(str::to_string)))
    }

    async fn set_setting(
        &self,
        tenant: TenantId,
        user: UserId,
        which: FeedbackSetting,
        value: Value,
    ) -> ApiResult<()> {
        let mut s = self.inner.lock().unwrap();
        let key = which.key();
        // ON CONFLICT (tenant, scope, user, key): replace in place.
        if let Some(row) = s
            .settings
            .iter_mut()
            .find(|(t, u, k, _)| *t == tenant && *u == Some(user) && *k == key)
        {
            row.3 = value;
        } else {
            s.settings.push((tenant, Some(user), key, value));
        }
        Ok(())
    }
}
