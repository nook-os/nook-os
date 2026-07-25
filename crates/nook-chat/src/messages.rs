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
use nook_types::{ChatMessage, ChatMessagePage, PostChatMessage};
use serde::Deserialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::{AppState, Caller, ChatError};

#[derive(sqlx::FromRow)]
struct MessageRow {
    id: Uuid,
    channel_id: Uuid,
    author_id: Uuid,
    body: String,
    created_at: DateTime<Utc>,
}

impl From<MessageRow> for ChatMessage {
    fn from(r: MessageRow) -> Self {
        ChatMessage {
            id: r.id,
            channel_id: r.channel_id,
            author_id: r.author_id,
            body: r.body,
            created_at: r.created_at,
        }
    }
}

const SELECT_MESSAGE: &str =
    "SELECT id, channel_id, author_id, body, created_at FROM chat_messages";

pub async fn post(
    State(state): State<AppState>,
    caller: Caller,
    Path(channel_id): Path<Uuid>,
    Json(req): Json<PostChatMessage>,
) -> Result<(StatusCode, Json<ChatMessage>), ChatError> {
    let scope = crate::channels::access(&state.db, channel_id, caller.tenant_id).await?;
    if scope.archived {
        return Err(ChatError::Conflict("this channel is archived".into()));
    }
    let body = req.body.trim();
    if body.is_empty() {
        return Err(ChatError::BadRequest("a message needs a body".into()));
    }

    let row = sqlx::query_as::<_, MessageRow>(
        "INSERT INTO chat_messages (id, channel_id, author_id, tenant_id, body)
         VALUES ($1, $2, $3, $4, $5)
         RETURNING id, channel_id, author_id, body, created_at",
    )
    .bind(Uuid::now_v7())
    .bind(channel_id)
    .bind(caller.user_id)
    .bind(caller.tenant_id)
    .bind(body)
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
    crate::channels::access(&state.db, channel_id, caller.tenant_id).await?;

    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    // v7 ids are time-ordered, so `id < before` is "older" and `ORDER BY id DESC`
    // is newest-first — the same keyset shape the rest of NookOS uses.
    let rows = sqlx::query_as::<_, MessageRow>(&format!(
        "{SELECT_MESSAGE} WHERE channel_id = $1 AND ($2::uuid IS NULL OR id < $2)
         ORDER BY id DESC LIMIT $3"
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
    sqlx::query_as::<_, MessageRow>(&format!("{SELECT_MESSAGE} WHERE id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .map(Into::into)
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
        sqlx::query("CREATE SCHEMA IF NOT EXISTS chat")
            .execute(&db)
            .await
            .ok()?;
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
}
