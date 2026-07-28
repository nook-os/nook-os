//! Posting and reading messages (MAIN-49 AC-2, AC-4, AC-5).
//!
//! A post is stored in `chat_messages` with a UUID v7 id — time-ordered, so
//! history keysets on it like the rest of NookOS — then delivered to local
//! subscribers and announced on the bus for peer instances. History pages
//! newest-first with a `before=<id>` cursor.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use nook_types::{ChatMessage, ChatMessagePage, ChatThread, PostChatMessage};
use serde::Deserialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::{AppState, Caller, ChatError};

#[derive(sqlx::FromRow)]
struct MessageRow {
    id: Uuid,
    channel_id: Uuid,
    author_id: Uuid,
    author_name: Option<String>,
    body: String,
    parent_message_id: Option<Uuid>,
    reply_count: i64,
    last_reply_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
}

impl From<MessageRow> for ChatMessage {
    fn from(r: MessageRow) -> Self {
        ChatMessage {
            id: r.id,
            channel_id: r.channel_id,
            author_id: r.author_id,
            author_name: r.author_name,
            body: r.body,
            parent_message_id: r.parent_message_id,
            reply_count: r.reply_count,
            last_reply_at: r.last_reply_at,
            created_at: r.created_at,
        }
    }
}

/// Reads carry the author's display name resolved from `public.users` by
/// `author_id` — so an org channel shows names for authors in other tenants
/// (MAIN-112 AC-4). `public.` is qualified so the join works on both the running
/// `chat,public` search_path and the `chat`-only test pool. A LEFT join keeps a
/// message with a since-deleted author readable (name `None`).
///
/// `reply_count`/`last_reply_at` are the cheap constants here — the single-row
/// reads (`fetch`, and a reply in a thread) don't need thread rollups. Channel
/// history uses [`SELECT_MESSAGE_WITH_REPLIES`] instead, which fills them in.
const SELECT_MESSAGE: &str = "SELECT m.id, m.channel_id, m.author_id, \
     u.display_name AS author_name, m.body, m.parent_message_id, m.created_at, \
     0::bigint AS reply_count, NULL::timestamptz AS last_reply_at \
     FROM chat_messages m LEFT JOIN public.users u ON u.id = m.author_id";

/// As [`SELECT_MESSAGE`], but with per-parent thread rollups so a parent in
/// channel history carries its `reply_count` and `last_reply_at` (MAIN-114 AC-3)
/// in the same query — no N+1. The correlated subqueries hit
/// `chat_messages_parent_idx`.
const SELECT_MESSAGE_WITH_REPLIES: &str = "SELECT m.id, m.channel_id, m.author_id, \
     u.display_name AS author_name, m.body, m.parent_message_id, m.created_at, \
     (SELECT count(*) FROM chat_messages r WHERE r.parent_message_id = m.id) AS reply_count, \
     (SELECT max(r.created_at) FROM chat_messages r WHERE r.parent_message_id = m.id) \
       AS last_reply_at \
     FROM chat_messages m LEFT JOIN public.users u ON u.id = m.author_id";

pub async fn post(
    State(state): State<AppState>,
    caller: Caller,
    Path(channel_id): Path<Uuid>,
    Json(req): Json<PostChatMessage>,
) -> Result<(StatusCode, Json<ChatMessage>), ChatError> {
    let scope = crate::channels::access(&state.db, channel_id, &caller).await?;
    if scope.archived {
        return Err(ChatError::Conflict("this channel is archived".into()));
    }
    let body = req.body.trim();
    if body.is_empty() {
        return Err(ChatError::BadRequest("a message needs a body".into()));
    }

    // A reply's parent must live in THIS channel and must itself be top-level —
    // threads are one level deep (MAIN-114 AC-1). Both are the client's error, so
    // 400s with a specific message rather than a silent drop or a 500.
    if let Some(parent_id) = req.parent_message_id {
        let parent: Option<(Uuid, Option<Uuid>)> =
            sqlx::query_as("SELECT channel_id, parent_message_id FROM chat_messages WHERE id = $1")
                .bind(parent_id)
                .fetch_optional(&state.db)
                .await
                .map_err(|_| ChatError::Internal)?;
        let (parent_channel, parents_parent) =
            parent.ok_or_else(|| ChatError::BadRequest("parent message not found".into()))?;
        if parent_channel != channel_id {
            return Err(ChatError::BadRequest(
                "parent message is in another channel".into(),
            ));
        }
        if parents_parent.is_some() {
            return Err(ChatError::BadRequest(
                "cannot reply to a reply — threads are one level deep".into(),
            ));
        }
    }

    let row = sqlx::query_as::<_, MessageRow>(
        "INSERT INTO chat_messages (id, channel_id, author_id, tenant_id, body, parent_message_id)
         VALUES ($1, $2, $3, $4, $5, $6)
         RETURNING id, channel_id, author_id,
             (SELECT display_name FROM public.users WHERE id = author_id) AS author_name,
             body, parent_message_id, created_at,
             0::bigint AS reply_count, NULL::timestamptz AS last_reply_at",
    )
    .bind(Uuid::now_v7())
    .bind(channel_id)
    .bind(caller.user_id)
    .bind(caller.tenant_id)
    .bind(body)
    .bind(req.parent_message_id)
    .fetch_one(&state.db)
    .await
    .map_err(|_| ChatError::Internal)?;
    let msg: ChatMessage = row.into();

    // Deliver to subscribers here now, and announce it so peer instances do the
    // same (AC-3). The origin guard on the bus stops a double send here.
    state.registry.publish_local(msg.clone());
    crate::bus::publish(&state.db, msg.id, state.registry.instance()).await;

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
) -> Result<Json<ChatMessagePage>, ChatError> {
    // Read is allowed on archived channels (history stays readable, AC-1); the
    // scope check still refuses another tenant's channel (AC-5).
    crate::channels::access(&state.db, channel_id, &caller).await?;

    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    // v7 ids are time-ordered, so `id < before` is "older" and `ORDER BY id DESC`
    // is newest-first — the same keyset shape the rest of NookOS uses. Replies
    // are excluded from channel history (`parent_message_id IS NULL`) — they live
    // only under their parent's thread (MAIN-114 AC-2); each parent carries its
    // reply rollups from the same query (AC-3).
    let rows = sqlx::query_as::<_, MessageRow>(&format!(
        "{SELECT_MESSAGE_WITH_REPLIES} WHERE m.channel_id = $1 AND m.parent_message_id IS NULL \
         AND ($2::uuid IS NULL OR m.id < $2) ORDER BY m.id DESC LIMIT $3"
    ))
    .bind(channel_id)
    .bind(q.before)
    .bind(limit)
    .fetch_all(&state.db)
    .await
    .map_err(|_| ChatError::Internal)?;

    // A full page implies there may be more; the cursor is the oldest id shown.
    let next_cursor = (rows.len() as i64 == limit)
        .then(|| rows.last().map(|m| m.id))
        .flatten();
    Ok(Json(ChatMessagePage {
        messages: rows.into_iter().map(Into::into).collect(),
        next_cursor,
    }))
}

/// Read one message back by id — used by the bus listener to deliver a peer
/// instance's post to local subscribers.
pub async fn fetch(pool: &PgPool, id: Uuid) -> Option<ChatMessage> {
    sqlx::query_as::<_, MessageRow>(&format!("{SELECT_MESSAGE} WHERE m.id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .map(Into::into)
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
) -> Result<Json<ChatThread>, ChatError> {
    // Resolve the parent (with its reply rollups), then authorize on its channel
    // BEFORE revealing anything else — a cross-tenant caller gets 403, not the
    // is-it-a-reply distinction below.
    let parent =
        sqlx::query_as::<_, MessageRow>(&format!("{SELECT_MESSAGE_WITH_REPLIES} WHERE m.id = $1"))
            .bind(message_id)
            .fetch_optional(&state.db)
            .await
            .map_err(|_| ChatError::Internal)?
            .ok_or(ChatError::NotFound)?;
    crate::channels::access(&state.db, parent.channel_id, &caller).await?;

    // A thread hangs off a top-level message; asking for a reply's thread is a
    // 400 — replies are one level deep (AC-1/AC-2).
    if parent.parent_message_id.is_some() {
        return Err(ChatError::BadRequest(
            "that message is itself a reply — threads are one level deep".into(),
        ));
    }

    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let rows = sqlx::query_as::<_, MessageRow>(&format!(
        "{SELECT_MESSAGE} WHERE m.parent_message_id = $1 AND ($2::uuid IS NULL OR m.id < $2)
         ORDER BY m.id DESC LIMIT $3"
    ))
    .bind(message_id)
    .bind(q.before)
    .bind(limit)
    .fetch_all(&state.db)
    .await
    .map_err(|_| ChatError::Internal)?;

    let next_cursor = (rows.len() as i64 == limit)
        .then(|| rows.last().map(|m| m.id))
        .flatten();
    Ok(Json(ChatThread {
        parent: parent.into(),
        replies: rows.into_iter().map(Into::into).collect(),
        next_cursor,
    }))
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use std::sync::Arc;

    use axum::extract::{Path, Query};
    use axum::Json;
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
        let db = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(opts)
            .await
            .ok()?;
        crate::ensure_chat_schema(&db).await.ok()?;
        crate::MIGRATOR.run(&db).await.ok()?;
        Some(AppState {
            db,
            registry: Arc::new(crate::registry::Registry::new()),
        })
    }

    fn caller(tenant: Uuid) -> Caller {
        Caller {
            user_id: Uuid::now_v7(),
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
        sqlx::query(
            "INSERT INTO chat_channels (id, owner_type, owner_id, name, slug)
             VALUES ($1, 'tenant', $2, $3, $3)",
        )
        .bind(id)
        .bind(tenant)
        .bind(name)
        .execute(&state.db)
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
        assert!(matches!(err, ChatError::Forbidden));

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
        assert!(matches!(err, ChatError::Forbidden));
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
        sqlx::query("UPDATE chat_channels SET archived_at = now() WHERE id = $1")
            .bind(channel)
            .execute(&state.db)
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
        assert!(matches!(err, ChatError::Conflict(_)));

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
        assert!(matches!(err, ChatError::BadRequest(_)));

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
        assert!(matches!(err, ChatError::BadRequest(_)));
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
        assert!(matches!(err, ChatError::BadRequest(_)));

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
        assert!(matches!(err, ChatError::BadRequest(_)));
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
        assert!(matches!(err, ChatError::NotFound));
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
        assert!(matches!(err, ChatError::Forbidden));
    }
}
