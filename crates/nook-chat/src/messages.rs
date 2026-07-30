//! Posting and reading messages (MAIN-49 AC-2, AC-4, AC-5).
//!
//! A post is stored in `chat_messages` with a UUID v7 id — time-ordered, so
//! history keysets on it like the rest of NookOS — then delivered to local
//! subscribers and announced on the bus for peer instances. History pages
//! newest-first with a `before=<id>` cursor.

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use nook_types::{
    ChatMessage, ChatMessagePage, ChatReactionAggregate, ChatServerMessage, ChatThread,
    PostChatMessage, UpdateChatMessage,
};
use serde::Deserialize;
use uuid::Uuid;

use crate::internal;
use crate::repo::messages::{MessageRepository, MessageRow, NewMessage, Page};
use crate::{AppState, Caller};
use nook_errors::ApiError;

/// The body a deleted message shows in every payload — the real content is
/// redacted server-side and never leaves the database (MAIN-116 AC-4).
const DELETED_PLACEHOLDER: &str = "message deleted";

impl From<MessageRow> for ChatMessage {
    fn from(r: MessageRow) -> Self {
        // Redaction is centralized HERE — every read path funnels rows through
        // this impl, so a deleted message's real body can never reach a payload
        // (MAIN-116 AC-4). Reactions are attached separately (they need a viewer).
        let deleted = r.deleted_at.is_some();
        ChatMessage {
            id: r.id,
            channel_id: r.channel_id,
            author_id: r.author_id,
            author_name: r.author_name,
            body: if deleted {
                DELETED_PLACEHOLDER.to_string()
            } else {
                r.body
            },
            parent_message_id: r.parent_message_id,
            reply_count: r.reply_count,
            last_reply_at: r.last_reply_at,
            created_at: r.created_at,
            reactions: Vec::new(),
            edited_at: r.edited_at,
            deleted,
        }
    }
}

/// Aggregate the reactions for a set of messages in ONE query (no N+1). `viewer`
/// scopes the per-emoji `reacted` flag to a caller; `None` (the bus/broadcast
/// path, which has no single viewer) yields `reacted = false` everywhere — the
/// counts are still accurate, and each client overlays its own reacted state.
async fn load_reactions(
    repo: &dyn MessageRepository,
    viewer: Option<Uuid>,
    ids: &[Uuid],
) -> HashMap<Uuid, Vec<ChatReactionAggregate>> {
    if ids.is_empty() {
        return HashMap::new();
    }
    let rows = repo.reactions_for(ids, viewer).await.unwrap_or_default();

    let mut out: HashMap<Uuid, Vec<ChatReactionAggregate>> = HashMap::new();
    for r in rows {
        out.entry(r.message_id)
            .or_default()
            .push(ChatReactionAggregate {
                emoji: r.emoji,
                count: r.count,
                reacted: r.reacted,
            });
    }
    out
}

/// Attach reactions to a batch of messages for `viewer`. A deleted message keeps
/// an empty reaction set (its content — and its tally — is gone from view).
async fn attach_reactions(
    repo: &dyn MessageRepository,
    viewer: Option<Uuid>,
    messages: &mut [ChatMessage],
) {
    let ids: Vec<Uuid> = messages
        .iter()
        .filter(|m| !m.deleted)
        .map(|m| m.id)
        .collect();
    let mut map = load_reactions(repo, viewer, &ids).await;
    for m in messages.iter_mut() {
        if !m.deleted {
            m.reactions = map.remove(&m.id).unwrap_or_default();
        }
    }
}

pub async fn post(
    State(state): State<AppState>,
    caller: Caller,
    Path(channel_id): Path<Uuid>,
    Json(req): Json<PostChatMessage>,
) -> Result<(StatusCode, Json<ChatMessage>), ApiError> {
    let scope = crate::channels::access(&*state.channels, channel_id, &caller).await?;
    if scope.archived {
        return Err(ApiError::Conflict("this channel is archived".into()));
    }
    let body = req.body.trim();
    if body.is_empty() {
        return Err(ApiError::BadRequest("a message needs a body".into()));
    }

    // A reply's parent must live in THIS channel and must itself be top-level —
    // threads are one level deep (MAIN-114 AC-1). Both are the client's error, so
    // 400s with a specific message rather than a silent drop or a 500.
    if let Some(parent_id) = req.parent_message_id {
        let parent = state
            .messages
            .parent_of(parent_id)
            .await
            .map_err(|_| internal())?
            .ok_or_else(|| ApiError::BadRequest("parent message not found".into()))?;
        let (parent_channel, parents_parent) = (parent.channel_id, parent.parent_message_id);
        if parent_channel != channel_id {
            return Err(ApiError::BadRequest(
                "parent message is in another channel".into(),
            ));
        }
        if parents_parent.is_some() {
            return Err(ApiError::BadRequest(
                "cannot reply to a reply — threads are one level deep".into(),
            ));
        }
    }

    let row = state
        .messages
        .post(NewMessage {
            channel_id,
            author_id: caller.user_id,
            tenant_id: caller.tenant_id,
            body: body.to_owned(),
            parent_message_id: req.parent_message_id,
        })
        .await
        .map_err(|_| internal())?;
    let msg: ChatMessage = row.into(); // no reactions on a brand-new message

    // Deliver to subscribers here now, and announce it so peer instances do the
    // same (AC-3). The origin guard on the bus stops a double send here.
    state
        .registry
        .publish_local(ChatServerMessage::Message(msg.clone()));
    crate::bus::publish(&state.db, msg.id, state.registry.instance(), false).await;

    Ok((StatusCode::CREATED, Json(msg)))
}

#[derive(Deserialize)]
pub struct HistoryQuery {
    /// Keyset cursor: return messages strictly older than this id (AC-4).
    pub before: Option<Uuid>,
    pub limit: Option<i64>,
}

pub async fn history(
    State(state): State<AppState>,
    caller: Caller,
    Path(channel_id): Path<Uuid>,
    Query(q): Query<HistoryQuery>,
) -> Result<Json<ChatMessagePage>, ApiError> {
    // Read is allowed on archived channels (history stays readable, AC-1); the
    // scope check still refuses another tenant's channel (AC-5).
    crate::channels::access(&*state.channels, channel_id, &caller).await?;

    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    // v7 ids are time-ordered, so `id < before` is "older" and `ORDER BY id DESC`
    // is newest-first — the same keyset shape the rest of NookOS uses. Replies
    // are excluded from channel history (`parent_message_id IS NULL`) — they live
    // only under their parent's thread (MAIN-114 AC-2); each parent carries its
    // reply rollups from the same query (AC-3).
    let rows = state
        .messages
        .history(
            channel_id,
            Page {
                before: q.before,
                limit,
            },
        )
        .await
        .map_err(|_| internal())?;

    // A full page implies there may be more; the cursor is the oldest id shown.
    let next_cursor = (rows.len() as i64 == limit)
        .then(|| rows.last().map(|m| m.id))
        .flatten();
    let mut messages: Vec<ChatMessage> = rows.into_iter().map(Into::into).collect();
    attach_reactions(&*state.messages, Some(caller.user_id), &mut messages).await;
    Ok(Json(ChatMessagePage {
        messages,
        next_cursor,
    }))
}

/// Read one message back by id, viewer-neutral (`reacted = false`) — used by the
/// bus listener and the update handlers to build the broadcast payload. Uses the
/// reply-rollup select so an edited/reacted PARENT keeps its `reply_count` in the
/// update event (MAIN-116). Redaction + reactions applied.
pub async fn fetch(repo: &dyn MessageRepository, id: Uuid) -> Option<ChatMessage> {
    read_message(repo, None, id).await
}

/// One message by id with redaction + reactions attached for `viewer`.
async fn read_message(
    repo: &dyn MessageRepository,
    viewer: Option<Uuid>,
    id: Uuid,
) -> Option<ChatMessage> {
    let row = repo.get(id).await.ok().flatten()?;
    let mut msg: ChatMessage = row.into();
    if !msg.deleted {
        msg.reactions = load_reactions(repo, viewer, &[id])
            .await
            .remove(&id)
            .unwrap_or_default();
    }
    Some(msg)
}

/// A message's thread: the parent plus a keyset page of its replies (MAIN-114
/// AC-2). Authorized on the parent's own channel, exactly as `history` is — a
/// caller who can read the channel can read its threads. Replies page
/// newest-first on id like channel history; the client orders them for reading.
pub async fn thread(
    State(state): State<AppState>,
    caller: Caller,
    Path(message_id): Path<Uuid>,
    Query(q): Query<HistoryQuery>,
) -> Result<Json<ChatThread>, ApiError> {
    // Resolve the parent (with its reply rollups), then authorize on its channel
    // BEFORE revealing anything else — a cross-tenant caller gets 403, not the
    // is-it-a-reply distinction below.
    let parent = state
        .messages
        .get(message_id)
        .await
        .map_err(|_| internal())?
        .ok_or(ApiError::NotFound)?;
    crate::channels::access(&*state.channels, parent.channel_id, &caller).await?;

    // A thread hangs off a top-level message; asking for a reply's thread is a
    // 400 — replies are one level deep (AC-1/AC-2).
    if parent.parent_message_id.is_some() {
        return Err(ApiError::BadRequest(
            "that message is itself a reply — threads are one level deep".into(),
        ));
    }

    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let rows = state
        .messages
        .replies(
            message_id,
            Page {
                before: q.before,
                limit,
            },
        )
        .await
        .map_err(|_| internal())?;

    let next_cursor = (rows.len() as i64 == limit)
        .then(|| rows.last().map(|m| m.id))
        .flatten();
    let mut parent: ChatMessage = parent.into();
    let mut replies: Vec<ChatMessage> = rows.into_iter().map(Into::into).collect();
    attach_reactions(
        &*state.messages,
        Some(caller.user_id),
        std::slice::from_mut(&mut parent),
    )
    .await;
    attach_reactions(&*state.messages, Some(caller.user_id), &mut replies).await;
    Ok(Json(ChatThread {
        parent,
        replies,
        next_cursor,
    }))
}

// ── Reactions + edit/delete (MAIN-116) ───────────────────────────────────────

/// A curated allowlist of reaction emoji (MAIN-116 AC-2). An allowlist is the
/// "sane approach": it guarantees a single, renderable grapheme and blocks
/// arbitrary text or oversized/compound sequences masquerading as an emoji,
/// without pulling in a grapheme-segmentation dependency.
const ALLOWED_EMOJI: &[&str] = &[
    "👍", "👎", "❤️", "😄", "🎉", "😕", "🚀", "👀", "🙌", "🔥", "✅", "❌",
];

fn valid_emoji(emoji: &str) -> bool {
    ALLOWED_EMOJI.contains(&emoji)
}

/// Load a message's `(channel_id, author_id, deleted_at)` for an authz decision.
async fn message_meta(
    repo: &dyn MessageRepository,
    id: Uuid,
) -> Result<(Uuid, Uuid, Option<DateTime<Utc>>), ApiError> {
    let m = repo
        .meta(id)
        .await
        .map_err(|_| internal())?
        .ok_or(ApiError::NotFound)?;
    Ok((m.channel_id, m.author_id, m.deleted_at))
}

/// Announce a change to an existing message (edit/delete/reaction — AC-5): the
/// `MessageUpdated` event locally, then the same over the bus so peers re-fetch
/// and re-deliver. `read_message(None)` gives a viewer-neutral payload.
async fn broadcast_update(state: &AppState, message_id: Uuid) {
    if let Some(msg) = read_message(&*state.messages, None, message_id).await {
        state
            .registry
            .publish_local(ChatServerMessage::MessageUpdated(msg));
    }
    crate::bus::publish(&state.db, message_id, state.registry.instance(), true).await;
}

/// Toggle the caller's reaction ON (`PUT`). Idempotent — a repeat is a no-op via
/// the primary key. The caller must be able to see the message's channel.
pub async fn add_reaction(
    State(state): State<AppState>,
    caller: Caller,
    Path((message_id, emoji)): Path<(Uuid, String)>,
) -> Result<Json<ChatMessage>, ApiError> {
    react(&state, &caller, message_id, &emoji, true).await
}

/// Toggle the caller's reaction OFF (`DELETE`). Idempotent.
pub async fn remove_reaction(
    State(state): State<AppState>,
    caller: Caller,
    Path((message_id, emoji)): Path<(Uuid, String)>,
) -> Result<Json<ChatMessage>, ApiError> {
    react(&state, &caller, message_id, &emoji, false).await
}

async fn react(
    state: &AppState,
    caller: &Caller,
    message_id: Uuid,
    emoji: &str,
    add: bool,
) -> Result<Json<ChatMessage>, ApiError> {
    if !valid_emoji(emoji) {
        return Err(ApiError::BadRequest("not a supported reaction".into()));
    }
    let (channel_id, _author, deleted_at) = message_meta(&*state.messages, message_id).await?;
    // Same visibility gate as reading — a caller who cannot see the channel
    // cannot react in it. A deleted message takes no new reactions.
    crate::channels::access(&*state.channels, channel_id, caller).await?;
    if deleted_at.is_some() {
        return Err(ApiError::Conflict("this message was deleted".into()));
    }

    if add {
        state
            .messages
            .add_reaction(message_id, caller.user_id, emoji)
            .await
            .map_err(|_| internal())?;
    } else {
        state
            .messages
            .remove_reaction(message_id, caller.user_id, emoji)
            .await
            .map_err(|_| internal())?;
    }

    broadcast_update(state, message_id).await;
    // The acting caller gets a viewer-accurate payload (its own `reacted` flags).
    read_message(&*state.messages, Some(caller.user_id), message_id)
        .await
        .map(Json)
        .ok_or(ApiError::NotFound)
}

/// Edit a message's body (`PATCH`) — author-only, validated like a post; the
/// prior content is kept as a revision and `edited_at` is set (AC-3).
pub async fn update(
    State(state): State<AppState>,
    caller: Caller,
    Path(message_id): Path<Uuid>,
    Json(req): Json<UpdateChatMessage>,
) -> Result<Json<ChatMessage>, ApiError> {
    let body = req.body.trim();
    if body.is_empty() {
        return Err(ApiError::BadRequest("a message needs a body".into()));
    }
    let (channel_id, author_id, deleted_at) = message_meta(&*state.messages, message_id).await?;
    crate::channels::access(&*state.channels, channel_id, &caller).await?;
    if author_id != caller.user_id {
        return Err(ApiError::Forbidden);
    }
    if deleted_at.is_some() {
        return Err(ApiError::Conflict("this message was deleted".into()));
    }

    // Record the prior content as an audit revision, then update in place. The
    // revision INSERT reads the current body, so it must run before the UPDATE.
    state
        .messages
        .edit(message_id, body, caller.user_id)
        .await
        .map_err(|_| internal())?;

    broadcast_update(&state, message_id).await;
    read_message(&*state.messages, Some(caller.user_id), message_id)
        .await
        .map(Json)
        .ok_or(ApiError::NotFound)
}

/// Soft-delete a message (`DELETE`) — author or tenant admin (AC-4). The content
/// is redacted in every payload from now on; the row and its revisions are kept
/// for audit. No hard delete exists.
pub async fn delete(
    State(state): State<AppState>,
    caller: Caller,
    Path(message_id): Path<Uuid>,
) -> Result<Json<ChatMessage>, ApiError> {
    let (channel_id, author_id, deleted_at) = message_meta(&*state.messages, message_id).await?;
    crate::channels::access(&*state.channels, channel_id, &caller).await?;
    // Author always may; otherwise the caller must be a tenant owner/admin.
    if author_id != caller.user_id {
        crate::require_admin(&*state.channels, &caller).await?;
    }
    if deleted_at.is_some() {
        // Already gone — idempotent success with the current (redacted) state.
        return read_message(&*state.messages, Some(caller.user_id), message_id)
            .await
            .map(Json)
            .ok_or(ApiError::NotFound);
    }

    state
        .messages
        .soft_delete(message_id, caller.user_id)
        .await
        .map_err(|_| internal())?;

    broadcast_update(&state, message_id).await;
    read_message(&*state.messages, Some(caller.user_id), message_id)
        .await
        .map(Json)
        .ok_or(ApiError::NotFound)
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use std::sync::Arc;

    use axum::extract::{Path, Query};
    use axum::Json;
    use nook_db::{params, Db, Postgres, TypeMapping};
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

    use super::*;

    /// A chat-schema pool + fresh registry, or `None` when the suite runs without
    /// a database (the same gate the rest of the suite uses). Channel/message
    /// rows reference tenant/user ids as bare uuids (no cross-schema FK), so
    /// random ids isolate every test without needing real tenant/user rows.
    async fn state() -> Option<AppState> {
        if std::env::var("NOOK_REQUIRE_DB").ok().as_deref() != Some("1") {
            eprintln!("skipping chat db test — no NOOK_REQUIRE_DB");
            return None;
        }
        let url = std::env::var("DATABASE_URL").ok()?;
        let opts = PgConnectOptions::from_str(&url)
            .ok()?
            .options([("search_path", "chat")]);
        let db = nook_db::EnginePool::from_pg(
            PgPoolOptions::new()
                .max_connections(2)
                .connect_with(opts)
                .await
                .ok()?,
        );
        crate::ensure_chat_schema(&db).await.ok()?;
        crate::MIGRATOR.run(db.pg()).await.ok()?;
        Some(AppState {
            channels: Arc::new(crate::repo::channels::DbChannelRepository::new(db.clone())),
            messages: Arc::new(crate::repo::messages::DbMessageRepository::new(db.clone())),
            dms: Arc::new(crate::repo::dms::DbDmRepository::new(db.clone())),
            db,
            registry: Arc::new(crate::registry::Registry::new()),
        })
    }

    fn caller(tenant: Uuid) -> Caller {
        caller_as(tenant, Uuid::now_v7())
    }

    /// A caller with a specific user id — for reaction/author tests that need the
    /// same identity to act twice or to be told apart from another.
    fn caller_as(tenant: Uuid, user: Uuid) -> Caller {
        Caller {
            user_id: user,
            tenant_id: tenant,
            cookie_session: true,
        }
    }

    // Insert a channel row directly, rather than through `channels::create` —
    // these tests run on a `chat`-only pool with no seeded `public.users`, and
    // create now gates on a tenant admin (MAIN-94), which that query cannot
    // resolve here. The message tests only need a channel to exist, not to
    // exercise create's authorization.
    async fn make_channel(state: &AppState, tenant: Uuid, name: &str) -> Uuid {
        let id = Uuid::now_v7();
        state
            .db
            .exec(
                "INSERT INTO chat_channels (id, owner_type, owner_id, name, slug)
             VALUES ($1, 'tenant', $2, $3, $3)",
                params![id, tenant, name],
            )
            .await
            .unwrap();
        id
    }

    #[tokio::test]
    async fn create_post_history_round_trips() {
        let Some(state) = state().await else { return };
        let tenant = Uuid::now_v7();
        let channel = make_channel(&state, tenant, "general").await;

        let (_, Json(posted)) = post(
            State(state.clone()),
            caller(tenant),
            Path(channel),
            Json(PostChatMessage {
                body: "  hello  ".into(),
                parent_message_id: None,
            }),
        )
        .await
        .unwrap();
        assert_eq!(posted.body, "hello"); // trimmed
        assert_eq!(posted.id.get_version_num(), 7, "message ids are UUID v7");

        let Json(page) = history(
            State(state.clone()),
            caller(tenant),
            Path(channel),
            Query(HistoryQuery {
                before: None,
                limit: None,
            }),
        )
        .await
        .unwrap();
        assert_eq!(page.messages.len(), 1);
        assert_eq!(page.messages[0].id, posted.id);
        assert!(page.next_cursor.is_none());
    }

    #[tokio::test]
    async fn history_keysets_with_no_overlap_or_gap() {
        let Some(state) = state().await else { return };
        let tenant = Uuid::now_v7();
        let channel = make_channel(&state, tenant, "keyset").await;

        let mut ids = Vec::new();
        for i in 0..5 {
            let (_, Json(m)) = post(
                State(state.clone()),
                caller(tenant),
                Path(channel),
                Json(PostChatMessage {
                    body: format!("m{i}"),
                    parent_message_id: None,
                }),
            )
            .await
            .unwrap();
            ids.push(m.id);
        }

        // Page 1: newest two.
        let Json(p1) = history(
            State(state.clone()),
            caller(tenant),
            Path(channel),
            Query(HistoryQuery {
                before: None,
                limit: Some(2),
            }),
        )
        .await
        .unwrap();
        let page1: Vec<_> = p1.messages.iter().map(|m| m.id).collect();
        assert_eq!(page1, vec![ids[4], ids[3]], "newest-first");
        let cursor = p1.next_cursor.expect("a full page has a cursor");
        assert_eq!(cursor, ids[3]);

        // Page 2: the next two older, no overlap with page 1.
        let Json(p2) = history(
            State(state.clone()),
            caller(tenant),
            Path(channel),
            Query(HistoryQuery {
                before: Some(cursor),
                limit: Some(2),
            }),
        )
        .await
        .unwrap();
        let page2: Vec<_> = p2.messages.iter().map(|m| m.id).collect();
        assert_eq!(page2, vec![ids[2], ids[1]]);
        assert!(page1.iter().all(|id| !page2.contains(id)), "no overlap");
    }

    #[tokio::test]
    async fn a_non_member_of_the_tenant_is_refused() {
        let Some(state) = state().await else { return };
        let owner = Uuid::now_v7();
        let intruder = Uuid::now_v7();
        let channel = make_channel(&state, owner, "private").await;

        // Read history as a different tenant → 403 (AC-5).
        let err = history(
            State(state.clone()),
            caller(intruder),
            Path(channel),
            Query(HistoryQuery {
                before: None,
                limit: None,
            }),
        )
        .await
        .expect_err("cross-tenant read is refused");
        assert!(matches!(err, ApiError::Forbidden));

        // Post as a different tenant → 403.
        let err = post(
            State(state.clone()),
            caller(intruder),
            Path(channel),
            Json(PostChatMessage {
                body: "sneaky".into(),
                parent_message_id: None,
            }),
        )
        .await
        .expect_err("cross-tenant post is refused");
        assert!(matches!(err, ApiError::Forbidden));
    }

    #[tokio::test]
    async fn an_archived_channel_refuses_new_posts_but_keeps_history() {
        let Some(state) = state().await else { return };
        let tenant = Uuid::now_v7();
        let channel = make_channel(&state, tenant, "old").await;
        let _ = post(
            State(state.clone()),
            caller(tenant),
            Path(channel),
            Json(PostChatMessage {
                body: "before archive".into(),
                parent_message_id: None,
            }),
        )
        .await
        .unwrap();

        // Archive it directly (create/update are admin-gated on a `public.users`
        // lookup this chat-only pool cannot resolve — MAIN-94; these tests are
        // about posting, not channel-management auth).
        state
            .db
            .exec(
                &format!(
                    "UPDATE chat_channels SET archived_at = {} WHERE id = $1",
                    Postgres.now()
                ),
                params![channel],
            )
            .await
            .unwrap();

        // Posting is refused…
        let err = post(
            State(state.clone()),
            caller(tenant),
            Path(channel),
            Json(PostChatMessage {
                body: "too late".into(),
                parent_message_id: None,
            }),
        )
        .await
        .expect_err("an archived channel refuses posts");
        assert!(matches!(err, ApiError::Conflict(_)));

        // …but history still reads.
        let Json(page) = history(
            State(state.clone()),
            caller(tenant),
            Path(channel),
            Query(HistoryQuery {
                before: None,
                limit: None,
            }),
        )
        .await
        .unwrap();
        assert_eq!(page.messages.len(), 1);
    }

    // ---- Threaded replies (MAIN-114) ----

    /// Post a message — a reply when `parent` is set — and return it.
    async fn post_msg(
        state: &AppState,
        tenant: Uuid,
        channel: Uuid,
        body: &str,
        parent: Option<Uuid>,
    ) -> ChatMessage {
        let (_, Json(m)) = post(
            State(state.clone()),
            caller(tenant),
            Path(channel),
            Json(PostChatMessage {
                body: body.into(),
                parent_message_id: parent,
            }),
        )
        .await
        .unwrap();
        m
    }

    #[tokio::test]
    async fn replies_are_hidden_from_history_and_roll_up_on_the_parent() {
        let Some(state) = state().await else { return };
        let tenant = Uuid::now_v7();
        let channel = make_channel(&state, tenant, "threads").await;

        let parent = post_msg(&state, tenant, channel, "root", None).await;
        let r1 = post_msg(&state, tenant, channel, "first reply", Some(parent.id)).await;
        let r2 = post_msg(&state, tenant, channel, "second reply", Some(parent.id)).await;
        assert_eq!(r1.parent_message_id, Some(parent.id));

        // Channel history shows only the parent, carrying the reply rollups
        // (AC-2 excludes replies; AC-3 counts them without an N+1).
        let Json(page) = history(
            State(state.clone()),
            caller(tenant),
            Path(channel),
            Query(HistoryQuery {
                before: None,
                limit: None,
            }),
        )
        .await
        .unwrap();
        assert_eq!(page.messages.len(), 1, "replies are excluded from history");
        assert_eq!(page.messages[0].id, parent.id);
        assert_eq!(page.messages[0].reply_count, 2);
        assert!(page.messages[0].last_reply_at.is_some());

        // The thread returns the parent plus both replies, newest-first (AC-2).
        let Json(t) = thread(
            State(state.clone()),
            caller(tenant),
            Path(parent.id),
            Query(HistoryQuery {
                before: None,
                limit: None,
            }),
        )
        .await
        .unwrap();
        assert_eq!(t.parent.id, parent.id);
        assert_eq!(t.parent.reply_count, 2);
        let reply_ids: Vec<_> = t.replies.iter().map(|m| m.id).collect();
        assert_eq!(reply_ids, vec![r2.id, r1.id], "newest-first");
        assert!(t.next_cursor.is_none());
    }

    #[tokio::test]
    async fn a_reply_parent_must_be_in_the_same_channel() {
        let Some(state) = state().await else { return };
        let tenant = Uuid::now_v7();
        let chan_a = make_channel(&state, tenant, "a").await;
        let chan_b = make_channel(&state, tenant, "b").await;
        let parent = post_msg(&state, tenant, chan_a, "in a", None).await;

        // A reply posted into channel B naming a parent from channel A → 400.
        let err = post(
            State(state.clone()),
            caller(tenant),
            Path(chan_b),
            Json(PostChatMessage {
                body: "wrong channel".into(),
                parent_message_id: Some(parent.id),
            }),
        )
        .await
        .expect_err("a cross-channel parent is refused");
        assert!(matches!(err, ApiError::BadRequest(_)));

        // An unknown parent id is likewise a 400, not a 500.
        let err = post(
            State(state.clone()),
            caller(tenant),
            Path(chan_a),
            Json(PostChatMessage {
                body: "ghost parent".into(),
                parent_message_id: Some(Uuid::now_v7()),
            }),
        )
        .await
        .expect_err("an unknown parent is refused");
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[tokio::test]
    async fn a_reply_cannot_itself_be_replied_to() {
        let Some(state) = state().await else { return };
        let tenant = Uuid::now_v7();
        let channel = make_channel(&state, tenant, "nesting").await;
        let parent = post_msg(&state, tenant, channel, "root", None).await;
        let reply = post_msg(&state, tenant, channel, "reply", Some(parent.id)).await;

        // Replying to a reply is refused — threads are one level deep (AC-1).
        let err = post(
            State(state.clone()),
            caller(tenant),
            Path(channel),
            Json(PostChatMessage {
                body: "reply to a reply".into(),
                parent_message_id: Some(reply.id),
            }),
        )
        .await
        .expect_err("nesting is refused");
        assert!(matches!(err, ApiError::BadRequest(_)));

        // And a reply has no thread of its own (AC-2).
        let err = thread(
            State(state.clone()),
            caller(tenant),
            Path(reply.id),
            Query(HistoryQuery {
                before: None,
                limit: None,
            }),
        )
        .await
        .expect_err("a reply has no thread");
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[tokio::test]
    async fn thread_replies_keyset_paginate() {
        let Some(state) = state().await else { return };
        let tenant = Uuid::now_v7();
        let channel = make_channel(&state, tenant, "pages").await;
        let parent = post_msg(&state, tenant, channel, "root", None).await;
        let mut ids = Vec::new();
        for i in 0..3 {
            ids.push(
                post_msg(&state, tenant, channel, &format!("r{i}"), Some(parent.id))
                    .await
                    .id,
            );
        }

        // Page 1: the newest two replies + a cursor.
        let Json(p1) = thread(
            State(state.clone()),
            caller(tenant),
            Path(parent.id),
            Query(HistoryQuery {
                before: None,
                limit: Some(2),
            }),
        )
        .await
        .unwrap();
        let page1: Vec<_> = p1.replies.iter().map(|m| m.id).collect();
        assert_eq!(page1, vec![ids[2], ids[1]], "newest-first");
        let cursor = p1.next_cursor.expect("a full page has a cursor");

        // Page 2: the oldest reply, no overlap.
        let Json(p2) = thread(
            State(state.clone()),
            caller(tenant),
            Path(parent.id),
            Query(HistoryQuery {
                before: Some(cursor),
                limit: Some(2),
            }),
        )
        .await
        .unwrap();
        let page2: Vec<_> = p2.replies.iter().map(|m| m.id).collect();
        assert_eq!(page2, vec![ids[0]]);
        assert!(p2.next_cursor.is_none());
    }

    #[tokio::test]
    async fn a_thread_for_a_missing_message_is_not_found() {
        let Some(state) = state().await else { return };
        let tenant = Uuid::now_v7();
        let err = thread(
            State(state.clone()),
            caller(tenant),
            Path(Uuid::now_v7()),
            Query(HistoryQuery {
                before: None,
                limit: None,
            }),
        )
        .await
        .expect_err("no such message");
        assert!(matches!(err, ApiError::NotFound));
    }

    #[tokio::test]
    async fn a_thread_read_is_refused_to_another_tenant() {
        let Some(state) = state().await else { return };
        let owner = Uuid::now_v7();
        let intruder = Uuid::now_v7();
        let channel = make_channel(&state, owner, "private-thread").await;
        let parent = post_msg(&state, owner, channel, "root", None).await;

        // A caller from another tenant is refused before any thread content — the
        // same channel-scope gate history uses (AC-5).
        let err = thread(
            State(state.clone()),
            caller(intruder),
            Path(parent.id),
            Query(HistoryQuery {
                before: None,
                limit: None,
            }),
        )
        .await
        .expect_err("cross-tenant thread read refused");
        assert!(matches!(err, ApiError::Forbidden));
    }

    // ── Reactions + edit/delete (MAIN-116) ──

    async fn post_as(
        state: &AppState,
        tenant: Uuid,
        channel: Uuid,
        author: Uuid,
        body: &str,
    ) -> ChatMessage {
        let (_, Json(m)) = post(
            State(state.clone()),
            caller_as(tenant, author),
            Path(channel),
            Json(PostChatMessage {
                body: body.into(),
                parent_message_id: None,
            }),
        )
        .await
        .unwrap();
        m
    }

    #[tokio::test]
    async fn reactions_toggle_aggregate_and_are_per_viewer() {
        let Some(state) = state().await else { return };
        let tenant = Uuid::now_v7();
        let channel = make_channel(&state, tenant, "react").await;
        let alice = Uuid::now_v7();
        let bob = Uuid::now_v7();
        let m = post_as(&state, tenant, channel, alice, "hi").await;

        // Alice reacts 👍 — she sees her own reaction, count 1.
        let Json(a) = add_reaction(
            State(state.clone()),
            caller_as(tenant, alice),
            Path((m.id, "👍".into())),
        )
        .await
        .unwrap();
        assert_eq!(a.reactions.len(), 1);
        assert_eq!(
            (a.reactions[0].emoji.as_str(), a.reactions[0].count),
            ("👍", 1)
        );
        assert!(a.reactions[0].reacted);

        // A repeat add is a no-op (idempotent via the PK, AC-2).
        let Json(again) = add_reaction(
            State(state.clone()),
            caller_as(tenant, alice),
            Path((m.id, "👍".into())),
        )
        .await
        .unwrap();
        assert_eq!(again.reactions[0].count, 1);

        // Bob reacts too → count 2.
        let Json(b) = add_reaction(
            State(state.clone()),
            caller_as(tenant, bob),
            Path((m.id, "👍".into())),
        )
        .await
        .unwrap();
        assert_eq!(b.reactions[0].count, 2);

        // A third viewer reading history sees count 2 but reacted=false.
        let Json(page) = history(
            State(state.clone()),
            caller(tenant),
            Path(channel),
            Query(HistoryQuery {
                before: None,
                limit: None,
            }),
        )
        .await
        .unwrap();
        let hm = page.messages.iter().find(|x| x.id == m.id).unwrap();
        assert_eq!(hm.reactions[0].count, 2);
        assert!(!hm.reactions[0].reacted, "a non-reactor sees reacted=false");

        // Alice removes hers → count 1, no longer reacted.
        let Json(rm) = remove_reaction(
            State(state.clone()),
            caller_as(tenant, alice),
            Path((m.id, "👍".into())),
        )
        .await
        .unwrap();
        assert_eq!(rm.reactions[0].count, 1);
        assert!(!rm.reactions[0].reacted);

        // An emoji outside the allowlist is refused (AC-2 validation).
        let bad = add_reaction(
            State(state.clone()),
            caller_as(tenant, alice),
            Path((m.id, "notemoji".into())),
        )
        .await;
        assert!(matches!(bad, Err(ApiError::BadRequest(_))));
    }

    #[tokio::test]
    async fn edit_is_author_only_and_records_a_revision() {
        let Some(state) = state().await else { return };
        let tenant = Uuid::now_v7();
        let channel = make_channel(&state, tenant, "edit").await;
        let author = Uuid::now_v7();
        let m = post_as(&state, tenant, channel, author, "typ0").await;

        // A non-author is refused BEFORE any change (AC-3).
        let refused = update(
            State(state.clone()),
            caller_as(tenant, Uuid::now_v7()),
            Path(m.id),
            Json(UpdateChatMessage {
                body: "hijack".into(),
            }),
        )
        .await;
        assert!(matches!(refused, Err(ApiError::Forbidden)));

        // The author edits (validated + trimmed like a post); edited_at is set.
        let Json(edited) = update(
            State(state.clone()),
            caller_as(tenant, author),
            Path(m.id),
            Json(UpdateChatMessage {
                body: "  typo  ".into(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(edited.body, "typo");
        assert!(edited.edited_at.is_some());

        // The prior content is preserved in the audit trail.
        let prior: String = state
            .db
            .query_scalar(
                "SELECT prior_content FROM chat_message_revisions WHERE message_id = $1 AND action = 'edit'",
                params![m.id],
            )
            .await
            .unwrap();
        assert_eq!(prior, "typ0");
    }

    #[tokio::test]
    async fn delete_redacts_everywhere_and_keeps_the_audit_trail() {
        let Some(state) = state().await else { return };
        let tenant = Uuid::now_v7();
        let channel = make_channel(&state, tenant, "del").await;
        let author = Uuid::now_v7();
        let m = post_as(&state, tenant, channel, author, "secret").await;

        // The author soft-deletes → redacted placeholder, deleted flag set (AC-4).
        let Json(deleted) = delete(State(state.clone()), caller_as(tenant, author), Path(m.id))
            .await
            .unwrap();
        assert!(deleted.deleted);
        assert_eq!(deleted.body, DELETED_PLACEHOLDER);

        // Channel history redacts it too — still present, never the real content.
        let Json(page) = history(
            State(state.clone()),
            caller(tenant),
            Path(channel),
            Query(HistoryQuery {
                before: None,
                limit: None,
            }),
        )
        .await
        .unwrap();
        let hm = page.messages.iter().find(|x| x.id == m.id).unwrap();
        assert!(hm.deleted);
        assert_eq!(hm.body, DELETED_PLACEHOLDER);

        // The real content survives ONLY in the audit trail.
        let prior: String = state
            .db
            .query_scalar(
                "SELECT prior_content FROM chat_message_revisions WHERE message_id = $1 AND action = 'delete'",
                params![m.id],
            )
            .await
            .unwrap();
        assert_eq!(prior, "secret");

        // A deleted message refuses further edits and reactions.
        let e = update(
            State(state.clone()),
            caller_as(tenant, author),
            Path(m.id),
            Json(UpdateChatMessage {
                body: "back".into(),
            }),
        )
        .await;
        assert!(matches!(e, Err(ApiError::Conflict(_))));
        let r = add_reaction(
            State(state.clone()),
            caller_as(tenant, author),
            Path((m.id, "👍".into())),
        )
        .await;
        assert!(matches!(r, Err(ApiError::Conflict(_))));
    }
}

/// Read-path behaviour against an in-memory [`FakeMessageRepository`] — no
/// database (MAIN-257 AC-3).
///
/// The three things this file gets wrong most easily are redaction (a deleted
/// body must never reach a payload), the viewer-neutral broadcast (`viewer =
/// None` must not mark anybody's reactions), and thread rollups on a parent.
/// All three are pure functions of the rows, so all three can be proven here.
#[cfg(test)]
mod fake_tests {
    use super::*;
    use crate::repo::fakes::FakeMessageRepository;

    #[tokio::test]
    async fn a_deleted_message_is_redacted_and_loses_its_reactions() {
        let (channel, author, viewer) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let gone = Uuid::now_v7();
        let repo = FakeMessageRepository::new()
            .with_deleted_message(gone, channel, author)
            .with_reaction(gone, viewer, "👍");

        let msg = fetch(&repo, gone).await.expect("the row is kept");
        assert!(msg.deleted);
        assert_eq!(msg.body, DELETED_PLACEHOLDER, "the real body never leaves");
        assert!(
            msg.reactions.is_empty(),
            "a deleted message's tally goes with it"
        );
        assert_eq!(
            repo.body_of(gone).as_deref(),
            Some("secret"),
            "redaction is on read — the row still holds the content for audit"
        );
    }

    #[tokio::test]
    async fn the_broadcast_payload_marks_nobody_as_having_reacted() {
        let (channel, author) = (Uuid::now_v7(), Uuid::now_v7());
        let (reader, other) = (Uuid::now_v7(), Uuid::now_v7());
        let id = Uuid::now_v7();
        let repo = FakeMessageRepository::new()
            .with_message(id, channel, author, "hello")
            .with_reaction(id, reader, "👍")
            .with_reaction(id, other, "👍");

        // `fetch` is the bus/broadcast path: one payload for every subscriber,
        // so it must not carry any single reader's `reacted` flag.
        let broadcast = fetch(&repo, id).await.unwrap();
        assert_eq!(broadcast.reactions.len(), 1);
        assert_eq!(
            broadcast.reactions[0].count, 2,
            "the tally is still accurate"
        );
        assert!(!broadcast.reactions[0].reacted);

        let mine = read_message(&repo, Some(reader), id).await.unwrap();
        assert!(
            mine.reactions[0].reacted,
            "the acting caller sees their own"
        );
    }

    #[tokio::test]
    async fn a_parent_carries_its_reply_rollups() {
        let (channel, author) = (Uuid::now_v7(), Uuid::now_v7());
        let parent = Uuid::now_v7();
        let repo = FakeMessageRepository::new()
            .with_message(parent, channel, author, "topic")
            .with_reply(Uuid::now_v7(), parent, channel, author)
            .with_reply(Uuid::now_v7(), parent, channel, author);

        let msg = fetch(&repo, parent).await.unwrap();
        assert_eq!(msg.reply_count, 2);
        assert!(msg.last_reply_at.is_some());
    }

    #[tokio::test]
    async fn an_edit_keeps_the_prior_body_as_a_revision() {
        let (channel, author) = (Uuid::now_v7(), Uuid::now_v7());
        let id = Uuid::now_v7();
        let repo = FakeMessageRepository::new().with_message(id, channel, author, "first");

        repo.edit(id, "second", author).await.unwrap();
        assert_eq!(repo.body_of(id).as_deref(), Some("second"));
        assert_eq!(
            repo.revisions_of(id),
            vec![("edit".to_string(), "first".to_string())],
            "what is preserved is what was actually stored, not what was sent"
        );
        assert!(fetch(&repo, id).await.unwrap().edited_at.is_some());
    }

    #[tokio::test]
    async fn attaching_reactions_leaves_deleted_messages_alone() {
        let (channel, author, viewer) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let (live, gone) = (Uuid::now_v7(), Uuid::now_v7());
        let repo = FakeMessageRepository::new()
            .with_message(live, channel, author, "here")
            .with_deleted_message(gone, channel, author)
            .with_reaction(live, viewer, "🎉")
            .with_reaction(gone, viewer, "🎉");

        let mut msgs: Vec<ChatMessage> = vec![
            repo.get(live).await.unwrap().unwrap().into(),
            repo.get(gone).await.unwrap().unwrap().into(),
        ];
        attach_reactions(&repo, Some(viewer), &mut msgs).await;
        assert_eq!(msgs[0].reactions.len(), 1);
        assert!(msgs[1].reactions.is_empty());
    }
}
