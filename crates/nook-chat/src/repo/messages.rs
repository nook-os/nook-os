//! Messages, their reactions, and the revision trail (MAIN-257).
//!
//! Every read funnels through one of two projections. [`select_message`] is the
//! cheap one; [`SELECT_MESSAGE_WITH_REPLIES`] adds the per-parent thread
//! rollups so a parent in channel history carries `reply_count` and
//! `last_reply_at` from the same query rather than an N+1 (MAIN-114 AC-3).
//! Keeping both here, rather than letting callers compose their own, is what
//! stops a third projection appearing that forgets one of the columns
//! `MessageRow` needs.

use async_trait::async_trait;

use super::RepoResult;
use chrono::{DateTime, Utc};
use nook_db::dialect::type_mapping;
use nook_db::{params, Db, DbPool};
use uuid::Uuid;

/// A message row as stored. `deleted_at` is carried, not applied — redaction
/// happens once in the caller's `From<MessageRow>`, so no read path can forget
/// it (MAIN-116 AC-4).
#[derive(Debug, Clone, nook_db::FromDbRow)]
pub struct MessageRow {
    pub id: Uuid,
    pub channel_id: Uuid,
    pub author_id: Uuid,
    pub author_name: Option<String>,
    pub body: String,
    pub parent_message_id: Option<Uuid>,
    pub reply_count: i64,
    pub last_reply_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub edited_at: Option<DateTime<Utc>>,
    pub deleted_at: Option<DateTime<Utc>>,
    /// NULL for an ordinary message; `"action"` for a `/me` post (MAIN-528).
    pub kind: Option<String>,
}

/// One emoji's tally on one message, with whether the viewer is among them.
#[derive(Debug, Clone, nook_db::FromDbRow)]
pub struct ReactionRow {
    pub message_id: Uuid,
    pub emoji: String,
    pub count: i64,
    pub reacted: bool,
}

/// The three facts an authz decision on a message needs: where it lives, who
/// wrote it, and whether it is already gone.
#[derive(Debug, Clone)]
pub struct MessageMeta {
    pub channel_id: Uuid,
    pub author_id: Uuid,
    pub deleted_at: Option<DateTime<Utc>>,
}

/// Where a reply hangs: its channel, and whether it is itself a reply.
#[derive(Debug, Clone)]
pub struct MessageParent {
    pub channel_id: Uuid,
    pub parent_message_id: Option<Uuid>,
}

/// One file hanging off a message, as stored (MAIN-535). The rendering facts
/// are copied from the upload at post time — see the migration for why they are
/// not read back through `public.user_content` on every render.
#[derive(Debug, Clone, nook_db::FromDbRow)]
pub struct AttachmentRow {
    pub id: Uuid,
    pub message_id: Uuid,
    pub content_id: Uuid,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: i64,
}

/// An upload the caller may hang off a message: the control plane's record,
/// narrowed to what chat copies. Answered only for the caller's OWN uploads in
/// their own tenant, which is what makes attaching a bare id safe.
#[derive(Debug, Clone, nook_db::FromDbRow)]
pub struct UploadRef {
    pub id: Uuid,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: i64,
}

/// A message to post.
#[derive(Debug, Clone)]
pub struct NewMessage {
    pub channel_id: Uuid,
    pub author_id: Uuid,
    pub tenant_id: Uuid,
    pub body: String,
    pub parent_message_id: Option<Uuid>,
    /// `None` posts an ordinary message; a command sets its own kind (MAIN-528).
    pub kind: Option<String>,
    /// The uploads to hang off it, in the order the sender picked them
    /// (MAIN-535). Resolved by the caller through [`MessageRepository::uploads_of`]
    /// so this cannot carry an id the poster does not own.
    pub attachments: Vec<UploadRef>,
}

/// A keyset page: v7 ids are time-ordered, so `id < before` is "older" and
/// `ORDER BY id DESC` is newest-first — the shape the rest of NookOS uses.
#[derive(Debug, Clone, Copy)]
pub struct Page {
    pub before: Option<Uuid>,
    pub limit: i64,
}

/// The cheap projection: no thread rollups, so `reply_count` is 0 and
/// `last_reply_at` NULL. The casts route through the type-mapping seam
/// (MAIN-212), which is why this is a fn and not a const.
fn select_message(engine: nook_db::Engine) -> String {
    format!(
        "SELECT m.id, m.channel_id, m.author_id, \
         u.display_name AS author_name, m.body, m.parent_message_id, m.created_at, \
         m.edited_at, m.deleted_at, m.kind, \
         {zero} AS reply_count, {null_ts} AS last_reply_at \
         FROM chat_messages m LEFT JOIN users u ON u.id = m.author_id",
        zero = type_mapping(engine).cast("0", "bigint"),
        null_ts = type_mapping(engine).cast("NULL", "timestamptz"),
    )
}

/// As above, with per-parent rollups. The correlated subqueries hit
/// `chat_messages_parent_idx`.
const SELECT_MESSAGE_WITH_REPLIES: &str = "SELECT m.id, m.channel_id, m.author_id, \
     u.display_name AS author_name, m.body, m.parent_message_id, m.created_at, \
     m.edited_at, m.deleted_at, m.kind, \
     (SELECT count(*) FROM chat_messages r WHERE r.parent_message_id = m.id) AS reply_count, \
     (SELECT max(r.created_at) FROM chat_messages r WHERE r.parent_message_id = m.id) \
       AS last_reply_at \
     FROM chat_messages m LEFT JOIN users u ON u.id = m.author_id";

#[async_trait]
pub trait MessageRepository: Send + Sync {
    /// Reactions for a set of messages in ONE query — no N+1. `viewer` scopes
    /// the per-emoji `reacted` flag; `None` (the bus broadcast, which has no
    /// single reader) leaves every flag false.
    async fn reactions_for(
        &self,
        messages: &[Uuid],
        viewer: Option<Uuid>,
    ) -> RepoResult<Vec<ReactionRow>>;

    /// Where a message hangs — used to refuse a reply to a reply before any
    /// write happens (threads are one level deep).
    async fn parent_of(&self, message: Uuid) -> RepoResult<Option<MessageParent>>;

    /// Channel, author and deleted state — everything the edit/delete/react
    /// authz decisions need, in one read.
    async fn meta(&self, message: Uuid) -> RepoResult<Option<MessageMeta>>;

    async fn post(&self, new: NewMessage) -> RepoResult<MessageRow>;

    /// One message, with its thread rollups.
    async fn get(&self, message: Uuid) -> RepoResult<Option<MessageRow>>;

    /// A channel's top-level history, newest first. Replies are excluded — they
    /// live only under their parent's thread (MAIN-114 AC-2) — and each parent
    /// carries its rollups from the same query (AC-3).
    async fn history(&self, channel: Uuid, page: Page) -> RepoResult<Vec<MessageRow>>;

    /// One thread's replies, newest first. Replies have no rollups of their
    /// own, so this uses the cheap projection.
    async fn replies(&self, parent: Uuid, page: Page) -> RepoResult<Vec<MessageRow>>;

    /// Add a reaction, idempotently — reacting twice is not an error and does
    /// not double-count.
    async fn add_reaction(&self, message: Uuid, user: Uuid, emoji: &str) -> RepoResult<()>;

    async fn remove_reaction(&self, message: Uuid, user: Uuid, emoji: &str) -> RepoResult<()>;

    /// Edit, keeping the prior body. The revision is written from the row
    /// itself (`SELECT … FROM chat_messages`) rather than from anything the
    /// caller passed, so what is preserved is what was actually stored.
    async fn edit(&self, message: Uuid, body: &str, actor: Uuid) -> RepoResult<()>;

    /// Soft-delete, keeping the prior body in the revision trail. The row stays
    /// so threads do not lose their shape; the content is redacted on read.
    async fn soft_delete(&self, message: Uuid, actor: Uuid) -> RepoResult<()>;

    /// Which of `ids` are uploads `uploader` made in `tenant` **and has not
    /// already attached** — the check that turns a bare content id from a
    /// client into something safe to attach (MAIN-535 AC-1). An id belonging to
    /// nobody, to another tenant, to another person, or to a message that
    /// already carries it is simply absent from the answer, so the caller
    /// refuses the post rather than learning which of the four it was.
    ///
    /// The already-attached arm is what keeps that refusal a 400. The unique
    /// index is still the backstop for two posts racing the same id, and the
    /// transaction in `post` is what lets that backstop lose cleanly.
    ///
    /// This reads `public.user_content`, the control plane's table, for the
    /// same reason `tenant_role` reads `public.users`: the owning repository is
    /// in another crate and there is nowhere else for the read to go. Named for
    /// the question, not the table.
    async fn uploads_of(
        &self,
        ids: &[Uuid],
        tenant: Uuid,
        uploader: Uuid,
    ) -> RepoResult<Vec<UploadRef>>;

    /// Attachments for a set of messages in ONE query — the same no-N+1 shape
    /// reactions use, so a page of history costs one extra round trip however
    /// many files it carries.
    async fn attachments_for(&self, messages: &[Uuid]) -> RepoResult<Vec<AttachmentRow>>;

    /// Drop a message's attachment rows and report the content ids that were
    /// on them, so the caller can have the bytes forgotten too (AC-6). Returns
    /// an empty list for a message with none, and for one already detached —
    /// deleting twice must not ask the store to forget twice.
    async fn detach_all(&self, message: Uuid) -> RepoResult<Vec<Uuid>>;
}

pub struct DbMessageRepository {
    db: DbPool,
}

impl DbMessageRepository {
    pub fn new(db: DbPool) -> Self {
        Self { db }
    }
}

#[async_trait]
impl MessageRepository for DbMessageRepository {
    async fn reactions_for(
        &self,
        messages: &[Uuid],
        viewer: Option<Uuid>,
    ) -> RepoResult<Vec<ReactionRow>> {
        self.db
            .query_all::<ReactionRow>(
                &format!(
                    // `MAX(CASE …)` rather than `bool_or`, which is a Postgres
                    // aggregate SQLite does not have (MAIN-439). "Did anyone in
                    // this group match" is a maximum over 0/1, which both
                    // engines compute; the `= 1` puts it back to a boolean. The
                    // COALESCE is kept exactly as defensive as it was around
                    // `bool_or` — `user_id` is NOT NULL and a GROUP BY group is
                    // never empty, so neither spelling can actually yield NULL.
                    "SELECT message_id, emoji, {cnt} AS count,
                            COALESCE(MAX(CASE WHEN user_id = $2 THEN 1 ELSE 0 END) = 1, false)
                              AS reacted
                       FROM chat_reactions
                      WHERE message_id = ANY($1)
                      GROUP BY message_id, emoji
                      ORDER BY message_id, emoji",
                    cnt = type_mapping(self.db.engine()).cast("count(*)", "bigint")
                ),
                params![messages.to_vec(), viewer],
            )
            .await
            .map_err(Into::into)
    }

    async fn parent_of(&self, message: Uuid) -> RepoResult<Option<MessageParent>> {
        let row: Option<(Uuid, Option<Uuid>)> = self
            .db
            .query_opt(
                "SELECT channel_id, parent_message_id FROM chat_messages WHERE id = $1",
                params![message],
            )
            .await?;
        Ok(row.map(|(channel_id, parent_message_id)| MessageParent {
            channel_id,
            parent_message_id,
        }))
    }

    async fn meta(&self, message: Uuid) -> RepoResult<Option<MessageMeta>> {
        let row: Option<(Uuid, Uuid, Option<DateTime<Utc>>)> = self
            .db
            .query_opt(
                "SELECT channel_id, author_id, deleted_at FROM chat_messages WHERE id = $1",
                params![message],
            )
            .await?;
        Ok(row.map(|(channel_id, author_id, deleted_at)| MessageMeta {
            channel_id,
            author_id,
            deleted_at,
        }))
    }

    /// A message and its attachments are ONE write.
    ///
    /// Each statement is its own autocommit otherwise, so the `chat_messages`
    /// row was already durable by the time an attachment insert could fail —
    /// and the 500 that followed left a message in everyone's history carrying
    /// no text and no files, which the client's Retry then duplicated on every
    /// press. A transaction is what makes the failure mean what the caller is
    /// told it means: nothing was posted, so retrying is a retry.
    async fn post(&self, new: NewMessage) -> RepoResult<MessageRow> {
        let id = Uuid::now_v7();
        let mut tx = self.db.begin().await.map_err(nook_db::DbError::from)?;
        let row = tx
            .query_one::<MessageRow>(
                &format!(
                    "INSERT INTO chat_messages
                        (id, channel_id, author_id, tenant_id, body, parent_message_id, kind)
                     VALUES ($1, $2, $3, $4, $5, $6, $7)
                     RETURNING id, channel_id, author_id,
                         (SELECT display_name FROM users WHERE id = author_id)
                           AS author_name,
                         body, parent_message_id, created_at, edited_at, deleted_at, kind,
                         {zero} AS reply_count, {null_ts} AS last_reply_at",
                    zero = type_mapping(self.db.engine()).cast("0", "bigint"),
                    null_ts = type_mapping(self.db.engine()).cast("NULL", "timestamptz"),
                ),
                params![
                    id,
                    new.channel_id,
                    new.author_id,
                    new.tenant_id,
                    new.body,
                    new.parent_message_id,
                    new.kind
                ],
            )
            .await?;

        // After the message, because the rows point at it — and inside the same
        // transaction, so a failure here takes the message with it. Dropping
        // `tx` on the `?` rolls back.
        for (position, upload) in new.attachments.iter().enumerate() {
            tx.exec(
                "INSERT INTO chat_message_attachments
                    (id, message_id, content_id, filename, content_type, size_bytes, position)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
                params![
                    Uuid::now_v7(),
                    id,
                    upload.id,
                    upload.filename.clone(),
                    upload.content_type.clone(),
                    upload.size_bytes,
                    position as i32
                ],
            )
            .await?;
        }
        tx.commit().await?;
        Ok(row)
    }

    async fn get(&self, message: Uuid) -> RepoResult<Option<MessageRow>> {
        self.db
            .query_opt::<MessageRow>(
                &format!("{SELECT_MESSAGE_WITH_REPLIES} WHERE m.id = $1"),
                params![message],
            )
            .await
            .map_err(Into::into)
    }

    async fn history(&self, channel: Uuid, page: Page) -> RepoResult<Vec<MessageRow>> {
        self.db
            .query_all::<MessageRow>(
                &format!(
                    "{SELECT_MESSAGE_WITH_REPLIES} WHERE m.channel_id = $1 \
                     AND m.parent_message_id IS NULL \
                     AND ({cursor} IS NULL OR m.id < $2) ORDER BY m.id DESC LIMIT $3",
                    cursor = type_mapping(self.db.engine()).cast("$2", "uuid")
                ),
                params![channel, page.before, page.limit],
            )
            .await
            .map_err(Into::into)
    }

    async fn replies(&self, parent: Uuid, page: Page) -> RepoResult<Vec<MessageRow>> {
        self.db
            .query_all::<MessageRow>(
                &format!(
                    "{sel} WHERE m.parent_message_id = $1 AND ({cursor} IS NULL OR m.id < $2)
                     ORDER BY m.id DESC LIMIT $3",
                    sel = select_message(self.db.engine()),
                    cursor = type_mapping(self.db.engine()).cast("$2", "uuid")
                ),
                params![parent, page.before, page.limit],
            )
            .await
            .map_err(Into::into)
    }

    async fn add_reaction(&self, message: Uuid, user: Uuid, emoji: &str) -> RepoResult<()> {
        self.db
            .exec(
                "INSERT INTO chat_reactions (message_id, user_id, emoji) VALUES ($1, $2, $3)
                 ON CONFLICT DO NOTHING",
                params![message, user, emoji],
            )
            .await?;
        Ok(())
    }

    async fn remove_reaction(&self, message: Uuid, user: Uuid, emoji: &str) -> RepoResult<()> {
        self.db
            .exec(
                "DELETE FROM chat_reactions WHERE message_id = $1 AND user_id = $2 AND emoji = $3",
                params![message, user, emoji],
            )
            .await?;
        Ok(())
    }

    async fn edit(&self, message: Uuid, body: &str, actor: Uuid) -> RepoResult<()> {
        self.db
            .exec(
                "INSERT INTO chat_message_revisions
                    (id, message_id, prior_content, action, acted_by)
                 SELECT $1, id, body, 'edit', $2 FROM chat_messages WHERE id = $3",
                params![Uuid::now_v7(), actor, message],
            )
            .await?;
        self.db
            .exec(
                &format!(
                    "UPDATE chat_messages SET body = $2, edited_at = {} WHERE id = $1",
                    type_mapping(self.db.engine()).now()
                ),
                params![message, body],
            )
            .await?;
        Ok(())
    }

    async fn soft_delete(&self, message: Uuid, actor: Uuid) -> RepoResult<()> {
        self.db
            .exec(
                "INSERT INTO chat_message_revisions
                    (id, message_id, prior_content, action, acted_by)
                 SELECT $1, id, body, 'delete', $2 FROM chat_messages WHERE id = $3",
                params![Uuid::now_v7(), actor, message],
            )
            .await?;
        self.db
            .exec(
                &format!(
                    "UPDATE chat_messages SET deleted_at = {} WHERE id = $1",
                    type_mapping(self.db.engine()).now()
                ),
                params![message],
            )
            .await?;
        Ok(())
    }

    async fn uploads_of(
        &self,
        ids: &[Uuid],
        tenant: Uuid,
        uploader: Uuid,
    ) -> RepoResult<Vec<UploadRef>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        self.db
            .query_all::<UploadRef>(
                "SELECT id, filename, content_type, size_bytes FROM user_content
                  WHERE id = ANY($1) AND tenant_id = $2 AND uploaded_by = $3
                    AND NOT EXISTS (SELECT 1 FROM chat_message_attachments a
                                     WHERE a.content_id = user_content.id)",
                params![ids.to_vec(), tenant, uploader],
            )
            .await
            .map_err(Into::into)
    }

    async fn attachments_for(&self, messages: &[Uuid]) -> RepoResult<Vec<AttachmentRow>> {
        if messages.is_empty() {
            return Ok(Vec::new());
        }
        self.db
            .query_all::<AttachmentRow>(
                "SELECT id, message_id, content_id, filename, content_type, size_bytes
                   FROM chat_message_attachments
                  WHERE message_id = ANY($1)
                  ORDER BY message_id, position, id",
                params![messages.to_vec()],
            )
            .await
            .map_err(Into::into)
    }

    async fn detach_all(&self, message: Uuid) -> RepoResult<Vec<Uuid>> {
        // Read then delete rather than `DELETE … RETURNING`: the return shape is
        // the same on both engines this way, and the pair is not a correctness
        // race — a message being deleted takes no new attachments (there is no
        // edit path for them, NG-4).
        let rows = self.attachments_for(&[message]).await?;
        self.db
            .exec(
                "DELETE FROM chat_message_attachments WHERE message_id = $1",
                params![message],
            )
            .await?;
        Ok(rows.into_iter().map(|r| r.content_id).collect())
    }
}
