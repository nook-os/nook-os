//! Channels, their categories, and read cursors (MAIN-257).
//!
//! `chat_channels` carries two owner models on one pair of generic owner
//! columns — `tenant` and `org` — and every method here preserves that shape
//! rather than splitting it, because the visibility rule the callers enforce is
//! written against it (MAIN-112).

use async_trait::async_trait;

use super::RepoResult;
use chrono::{DateTime, Utc};
use nook_db::dialect::type_mapping;
use nook_db::{params, Db, DbPool};
use uuid::Uuid;

/// A channel row as stored, plus the unread rollup the list query adds.
///
/// `unread_count` is filled only by [`ChannelRepository::list`]; the create,
/// update and place paths do not run the aggregate, so a freshly created or
/// renamed channel reports no unread rather than paying for the count.
#[derive(Debug, Clone, nook_db::FromDbRow)]
pub struct ChannelRow {
    pub id: Uuid,
    pub name: String,
    pub slug: String,
    pub owner_type: String,
    pub archived_at: Option<DateTime<Utc>>,
    pub category_id: Option<Uuid>,
    pub position: i32,
    #[db(default)]
    pub unread_count: i64,
    pub created_at: DateTime<Utc>,
}

/// A category row as stored.
#[derive(Debug, Clone, nook_db::FromDbRow)]
pub struct CategoryRow {
    pub id: Uuid,
    pub name: String,
    pub owner_type: String,
    pub position: i32,
    pub created_at: DateTime<Utc>,
}

/// Who owns a channel, and whether it is archived — the facts every handler
/// that touches a channel by id needs before it does anything else.
#[derive(Debug, Clone)]
pub struct ChannelOwner {
    pub owner_type: String,
    pub owner_id: Uuid,
    pub archived_at: Option<DateTime<Utc>>,
}

/// The owner scope a category belongs to. Categories are owned exactly as
/// channels are, which is what lets a channel be placed only in a category of
/// its own scope.
#[derive(Debug, Clone, Copy)]
pub struct OwnerScope {
    pub owner_type: &'static str,
    pub owner_id: Uuid,
}

const CHANNEL_COLS: &str =
    "id, name, slug, owner_type, archived_at, category_id, position, created_at";

const CATEGORY_COLS: &str = "id, name, owner_type, position, created_at";

/// The predicate that scopes a category to a caller: their tenant's categories
/// plus their org's. Mirrors the channel-scope query, so a caller only ever
/// touches categories they can see. `$N` is the tenant-id bind index.
fn category_scope(bind: &str) -> String {
    format!(
        "((owner_type = 'tenant' AND owner_id = {bind})\n     OR (owner_type = 'org' AND owner_id = (SELECT org_id FROM tenants WHERE id = {bind})))"
    )
}

/// A correlated subquery counting a channel's messages newer than the reader's
/// read cursor, excluding the reader's own and deleted ones (MAIN-117).
/// `{chan}` is the channel-id expression to correlate on and `{reader}` the
/// caller bind. No cursor row → `-infinity`, so everything counts until the
/// first read; the boundary is strict, so a message at the cursor instant is
/// already read.
fn unread_subquery(engine: nook_db::Engine, chan: &str, reader: &str) -> String {
    format!(
        "(SELECT count(*) FROM chat_messages m
            WHERE m.channel_id = {chan}
              AND m.author_id <> {reader}
              AND m.deleted_at IS NULL
              AND m.created_at > COALESCE(
                  (SELECT r.last_read_at FROM chat_read_cursors r
                     WHERE r.channel_id = {chan} AND r.user_id = {reader}),
                  {ninf}))",
        ninf = type_mapping(engine).cast("'-infinity'", "timestamptz")
    )
}

#[async_trait]
pub trait ChannelRepository: Send + Sync {
    // ── the scope facts every handler resolves first ────────────────────────

    /// A channel's owner and archived state, or `None` if there is no such
    /// channel. The caller turns that `None` into 404 and a scope mismatch into
    /// 403 — the two are kept distinct on purpose (MAIN-49 AC-5).
    async fn owner_of(&self, channel: Uuid) -> RepoResult<Option<ChannelOwner>>;

    /// Which org a tenant belongs to.
    async fn org_of_tenant(&self, tenant: Uuid) -> RepoResult<Option<Uuid>>;

    /// Does the person behind `user` belong to any tenant under `org`? The org
    /// visibility rule: a person in two of an org's tenants sees the one
    /// channel (MAIN-112).
    ///
    /// Reads `users`/`tenants` — nook-control's data, unreachable
    /// from this crate's repositories. Named for the question, not the table.
    async fn person_in_org(&self, user: Uuid, org: Uuid) -> RepoResult<bool>;

    /// The caller's role in their tenant. Chat's ONLY role source (MAIN-94
    /// NG-5): the existing per-tenant `users.role`, never a new catalog.
    /// `None` when there is no membership row.
    async fn tenant_role(&self, user: Uuid, tenant: Uuid) -> RepoResult<Option<String>>;

    /// Is the person behind `user` a participant of this channel? The DM
    /// membership check.
    async fn person_is_participant(&self, channel: Uuid, user: Uuid) -> RepoResult<bool>;

    // ── channels ────────────────────────────────────────────────────────────

    async fn create(&self, owner: OwnerScope, name: &str, slug: &str) -> RepoResult<ChannelRow>;

    /// The caller's tenant channels plus their org's, with unread counts.
    /// Archived channels drop out unless asked for, but keep their history.
    async fn list(
        &self,
        tenant: Uuid,
        include_archived: bool,
        reader: Uuid,
    ) -> RepoResult<Vec<ChannelRow>>;

    /// Rename and/or archive. `archived = None` leaves the flag alone, which is
    /// why it is threaded as an `Option<bool>` rather than a `bool`.
    async fn update(
        &self,
        id: Uuid,
        name: Option<String>,
        archived: Option<bool>,
    ) -> RepoResult<Option<ChannelRow>>;

    /// Does that category belong to the same owner as that channel? `None` when
    /// either row is missing. The guard that stops a channel being filed under
    /// another tenant's category.
    async fn category_matches_channel(
        &self,
        category: Uuid,
        channel: Uuid,
    ) -> RepoResult<Option<bool>>;

    async fn place(
        &self,
        id: Uuid,
        category: Option<Uuid>,
        position: i32,
    ) -> RepoResult<Option<ChannelRow>>;

    /// Move the reader's cursor to now. `GREATEST` so an out-of-order write
    /// never rewinds it — a stale request must not resurrect read messages.
    async fn mark_read(&self, channel: Uuid, user: Uuid) -> RepoResult<()>;

    // ── categories ──────────────────────────────────────────────────────────

    /// Every category the caller can see: their tenant's plus their org's.
    async fn categories(&self, tenant: Uuid) -> RepoResult<Vec<CategoryRow>>;

    async fn category_count(&self, owner: OwnerScope) -> RepoResult<i64>;

    async fn create_category(
        &self,
        owner: OwnerScope,
        name: &str,
        position: i32,
    ) -> RepoResult<CategoryRow>;

    /// Rename, scoped to what the caller can see — a category outside their
    /// tenant/org yields `None`, never a rename.
    async fn rename_category(
        &self,
        id: Uuid,
        tenant: Uuid,
        name: &str,
    ) -> RepoResult<Option<CategoryRow>>;

    async fn delete_category(&self, id: Uuid, tenant: Uuid) -> RepoResult<u64>;

    /// Apply an ordering. Every write carries the same scope predicate, so a
    /// reorder can only ever touch categories the caller can see.
    async fn reorder_categories(&self, tenant: Uuid, ordered: &[Uuid]) -> RepoResult<()>;
}

pub struct DbChannelRepository {
    db: DbPool,
}

impl DbChannelRepository {
    pub fn new(db: DbPool) -> Self {
        Self { db }
    }
}

#[async_trait]
impl ChannelRepository for DbChannelRepository {
    async fn owner_of(&self, channel: Uuid) -> RepoResult<Option<ChannelOwner>> {
        let row: Option<(String, Uuid, Option<DateTime<Utc>>)> = self
            .db
            .query_opt(
                "SELECT owner_type, owner_id, archived_at FROM chat_channels WHERE id = $1",
                params![channel],
            )
            .await?;
        Ok(row.map(|(owner_type, owner_id, archived_at)| ChannelOwner {
            owner_type,
            owner_id,
            archived_at,
        }))
    }

    async fn org_of_tenant(&self, tenant: Uuid) -> RepoResult<Option<Uuid>> {
        self.db
            .query_scalar_opt::<Uuid>("SELECT org_id FROM tenants WHERE id = $1", params![tenant])
            .await
            .map_err(Into::into)
    }

    async fn person_in_org(&self, user: Uuid, org: Uuid) -> RepoResult<bool> {
        self.db
            .query_scalar::<bool>(
                "SELECT EXISTS(
                     SELECT 1 FROM users u
                     JOIN tenants t ON t.id = u.tenant_id
                     WHERE t.org_id = $2
                       AND u.person_id = (SELECT person_id FROM users WHERE id = $1)
                 )",
                params![user, org],
            )
            .await
            .map_err(Into::into)
    }

    async fn tenant_role(&self, user: Uuid, tenant: Uuid) -> RepoResult<Option<String>> {
        let row: Option<(String,)> = self
            .db
            .query_opt(
                "SELECT role FROM users WHERE id = $1 AND tenant_id = $2",
                params![user, tenant],
            )
            .await?;
        Ok(row.map(|(r,)| r))
    }

    async fn person_is_participant(&self, channel: Uuid, user: Uuid) -> RepoResult<bool> {
        self.db
            .query_scalar::<bool>(
                "SELECT EXISTS(
                     SELECT 1 FROM chat_channel_participants
                     WHERE channel_id = $1
                       AND person_id = (SELECT person_id FROM users WHERE id = $2)
                 )",
                params![channel, user],
            )
            .await
            .map_err(Into::into)
    }

    async fn create(&self, owner: OwnerScope, name: &str, slug: &str) -> RepoResult<ChannelRow> {
        self.db
            .query_one::<ChannelRow>(
                &format!(
                    "INSERT INTO chat_channels (id, owner_type, owner_id, name, slug)
                     VALUES ($1, $2, $3, $4, $5)
                     RETURNING {CHANNEL_COLS}"
                ),
                params![Uuid::now_v7(), owner.owner_type, owner.owner_id, name, slug],
            )
            .await
            .map_err(Into::into)
    }

    async fn list(
        &self,
        tenant: Uuid,
        include_archived: bool,
        reader: Uuid,
    ) -> RepoResult<Vec<ChannelRow>> {
        self.db
            .query_all::<ChannelRow>(
                &format!(
                    "SELECT {CHANNEL_COLS}, {unread} AS unread_count
                       FROM chat_channels c
                      WHERE (
                              (c.owner_type = 'tenant' AND c.owner_id = $1)
                           OR (c.owner_type = 'org' AND c.owner_id =
                                 (SELECT org_id FROM tenants WHERE id = $1))
                            )
                        AND ($2 OR c.archived_at IS NULL)
                      ORDER BY c.created_at",
                    unread = unread_subquery(self.db.engine(), "c.id", "$3"),
                ),
                params![tenant, include_archived, reader],
            )
            .await
            .map_err(Into::into)
    }

    async fn update(
        &self,
        id: Uuid,
        name: Option<String>,
        archived: Option<bool>,
    ) -> RepoResult<Option<ChannelRow>> {
        self.db
            .query_opt::<ChannelRow>(
                &format!(
                    "UPDATE chat_channels
                     SET name = COALESCE($2, name),
                         archived_at = CASE
                             WHEN $3 THEN (CASE WHEN $4 THEN {now} ELSE NULL END)
                             ELSE archived_at
                         END
                     WHERE id = $1
                     RETURNING {CHANNEL_COLS}",
                    now = type_mapping(self.db.engine()).now(),
                ),
                params![id, name, archived.is_some(), archived.unwrap_or(false)],
            )
            .await
            .map_err(Into::into)
    }

    async fn category_matches_channel(
        &self,
        category: Uuid,
        channel: Uuid,
    ) -> RepoResult<Option<bool>> {
        let row: Option<(bool,)> = self
            .db
            .query_opt(
                "SELECT (c.owner_type = ch.owner_type AND c.owner_id = ch.owner_id)
                   FROM chat_channel_categories c, chat_channels ch
                  WHERE c.id = $1 AND ch.id = $2",
                params![category, channel],
            )
            .await?;
        Ok(row.map(|(same,)| same))
    }

    async fn place(
        &self,
        id: Uuid,
        category: Option<Uuid>,
        position: i32,
    ) -> RepoResult<Option<ChannelRow>> {
        self.db
            .query_opt::<ChannelRow>(
                &format!(
                    "UPDATE chat_channels SET category_id = $2, position = $3
                     WHERE id = $1 RETURNING {CHANNEL_COLS}"
                ),
                params![id, category, position],
            )
            .await
            .map_err(Into::into)
    }

    async fn mark_read(&self, channel: Uuid, user: Uuid) -> RepoResult<()> {
        self.db
            .exec(
                &format!(
                    "INSERT INTO chat_read_cursors (channel_id, user_id, last_read_at)
                     VALUES ($1, $2, {now})
                     ON CONFLICT (channel_id, user_id)
                     DO UPDATE SET last_read_at =
                         {greatest}",
                    greatest = type_mapping(self.db.engine())
                        .greatest("chat_read_cursors.last_read_at", "EXCLUDED.last_read_at"),
                    now = type_mapping(self.db.engine()).now()
                ),
                params![channel, user],
            )
            .await?;
        Ok(())
    }

    async fn categories(&self, tenant: Uuid) -> RepoResult<Vec<CategoryRow>> {
        self.db
            .query_all::<CategoryRow>(
                &format!(
                    "SELECT {CATEGORY_COLS} FROM chat_channel_categories
                     WHERE {scope} ORDER BY position, created_at",
                    scope = category_scope("$1")
                ),
                params![tenant],
            )
            .await
            .map_err(Into::into)
    }

    async fn category_count(&self, owner: OwnerScope) -> RepoResult<i64> {
        self.db
            .query_scalar::<i64>(
                "SELECT count(*) FROM chat_channel_categories
                 WHERE owner_type = $1 AND owner_id = $2",
                params![owner.owner_type, owner.owner_id],
            )
            .await
            .map_err(Into::into)
    }

    async fn create_category(
        &self,
        owner: OwnerScope,
        name: &str,
        position: i32,
    ) -> RepoResult<CategoryRow> {
        self.db
            .query_one::<CategoryRow>(
                &format!(
                    "INSERT INTO chat_channel_categories (id, owner_type, owner_id, name, position)
                     VALUES ($1, $2, $3, $4, $5) RETURNING {CATEGORY_COLS}"
                ),
                params![
                    Uuid::now_v7(),
                    owner.owner_type,
                    owner.owner_id,
                    name,
                    position
                ],
            )
            .await
            .map_err(Into::into)
    }

    async fn rename_category(
        &self,
        id: Uuid,
        tenant: Uuid,
        name: &str,
    ) -> RepoResult<Option<CategoryRow>> {
        self.db
            .query_opt::<CategoryRow>(
                &format!(
                    "UPDATE chat_channel_categories SET name = $2
                     WHERE id = $1 AND {scope} RETURNING {CATEGORY_COLS}",
                    scope = category_scope("$3")
                ),
                params![id, name, tenant],
            )
            .await
            .map_err(Into::into)
    }

    async fn delete_category(&self, id: Uuid, tenant: Uuid) -> RepoResult<u64> {
        self.db
            .exec(
                &format!(
                    "DELETE FROM chat_channel_categories WHERE id = $1 AND {scope}",
                    scope = category_scope("$2")
                ),
                params![id, tenant],
            )
            .await
            .map_err(Into::into)
    }

    async fn reorder_categories(&self, tenant: Uuid, ordered: &[Uuid]) -> RepoResult<()> {
        for (position, id) in ordered.iter().enumerate() {
            self.db
                .exec(
                    &format!(
                        "UPDATE chat_channel_categories SET position = $2 WHERE id = $1 AND {scope}",
                        scope = category_scope("$3")
                    ),
                    params![*id, position as i32, tenant],
                )
                .await?;
        }
        Ok(())
    }
}
