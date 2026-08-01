//! Cross-instance chat fan-out over the event-bus seam (MAIN-49 AC-3).
//!
//! Mirrors the control plane's bus but far smaller: a post publishes the new
//! message's id and the instance that posted it; every instance's listener
//! fetches that message and delivers it to its local subscribers, skipping the
//! origin (which already delivered it locally). No extra infrastructure — the
//! shared Postgres is the bus. A message's body can exceed NOTIFY's ~8 KB limit,
//! so only the id travels in the payload and the row is read back by id.
//!
//! The transport itself — `pg_notify` to publish and a `LISTEN` connection to
//! subscribe — lives in `nook-db`'s event-bus seam (`PgEventBus`, MAIN-200), so
//! the Postgres-specific SQL is there, not here. This module only composes the
//! `Notice` payload, picks the channel, and routes received notices to local
//! subscribers.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use nook_db::{DbPool, EventBus, PgEventBus};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::registry::Registry;

/// The single NOTIFY channel chat uses. (LISTEN/NOTIFY channel names are
/// database-global, not tied to the `chat` schema search_path.)
const NOTIFY_CHANNEL: &str = "nook_chat_msg";

#[derive(Serialize, Deserialize)]
struct Notice {
    id: Uuid,
    origin: Uuid,
    /// Whether this announces a CHANGE to an existing message (edit/delete/
    /// reaction — MAIN-116) rather than a brand-new post, so peers re-deliver it
    /// under the right WS variant. `#[serde(default)]` keeps old-format notices
    /// (none in flight, but harmless) readable as new posts.
    #[serde(default)]
    updated: bool,
}

/// Announce a posted OR updated message so peer instances deliver it too. Best
/// effort: a failed publish costs cross-instance liveness for one message, never
/// correctness of what was stored.
pub async fn publish(pool: &DbPool, message_id: Uuid, origin: Uuid, updated: bool) {
    let payload = serde_json::to_string(&Notice {
        id: message_id,
        origin,
        updated,
    })
    .unwrap_or_default();
    // Cross-instance fan-out is a Postgres capability (MAIN-196). On SQLite the
    // deployment IS one process against one file — there is no peer to announce
    // to — and `PgEventBus` would panic reaching for a Postgres pool. Local
    // subscribers are served by the in-memory registry either way, so returning
    // here loses nothing that engine could have had. Mirrors how the control
    // plane declines to start its own bus on SQLite.
    if pool.engine() != nook_db::Engine::Postgres {
        return;
    }
    // The NOTIFY itself lives in the event-bus seam's Postgres impl; chat only
    // composes the payload and picks the channel.
    if let Err(e) = PgEventBus::new(pool.clone())
        .publish(NOTIFY_CHANNEL, &payload)
        .await
    {
        tracing::warn!(error = %e, "chat NOTIFY failed; peers miss this message live");
    }
}

/// Spawn the listener for the life of the process: receive peers' announcements,
/// read the message back, and deliver it to local subscribers. Reconnects on
/// error so a dropped listener connection is self-healing.
pub fn start(
    registry: Arc<Registry>,
    pool: DbPool,
    messages: Arc<dyn crate::repo::messages::MessageRepository>,
) {
    // Same reason as `publish`: nothing to listen for on a single-process engine.
    if pool.engine() != nook_db::Engine::Postgres {
        tracing::info!("single-instance engine — chat bus listener not started");
        return;
    }
    tokio::spawn(async move {
        loop {
            if let Err(e) = run(&registry, &pool, &*messages).await {
                tracing::warn!(error = %e, "chat bus listener dropped; reconnecting");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    });
}

async fn run(
    registry: &Registry,
    pool: &DbPool,
    messages: &dyn crate::repo::messages::MessageRepository,
) -> anyhow::Result<()> {
    // Subscribe through the event-bus seam: it owns the LISTEN connection and
    // yields each peer's raw payload. Only the id travels in NOTIFY, so we still
    // read the message body back by id through the same pool.
    let mut notices = PgEventBus::new(pool.clone())
        .subscribe(NOTIFY_CHANNEL)
        .await?;
    while let Some(payload) = notices.next().await {
        let Ok(notice) = serde_json::from_str::<Notice>(&payload) else {
            continue;
        };
        // The origin instance already delivered this to its own subscribers.
        if notice.origin == registry.instance() {
            continue;
        }
        if let Some(msg) = crate::messages::fetch(messages, notice.id).await {
            // Re-deliver under the right variant so a peer's edit/delete/reaction
            // arrives as an update, not a duplicate new message (MAIN-116 AC-5).
            let event = if notice.updated {
                nook_types::ChatServerMessage::MessageUpdated(msg)
            } else {
                nook_types::ChatServerMessage::Message(msg)
            };
            registry.publish_local(event);
        }
    }
    // The subscription stream ends only when the LISTEN connection failed
    // unrecoverably; surface it so `start` applies its reconnect backoff — the
    // same self-healing the pre-seam `recv()?` produced.
    anyhow::bail!("chat bus subscription ended; reconnecting")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A message published through `publish` reaches a subscriber on the same
    /// channel via the event-bus seam, carrying the id/origin/updated we set —
    /// i.e. chat's fan-out rides the trait, not inline `pg_notify`/`LISTEN`.
    /// DB-backed; no-ops without `NOOK_REQUIRE_DB=1`, matching the suite.
    ///
    /// **This test reads past notices it did not publish, on purpose** (MAIN-234).
    /// `NOTIFY_CHANNEL` is database-global, and every sibling test that posts a
    /// message goes through the real `post` handler, which publishes here — as
    /// does any `nook-chat` process sharing the database. Taking the *first*
    /// notice off the stream and asserting it is ours was therefore a race the
    /// suite lost roughly one run in three: the assertion failed with two v7
    /// uuids milliseconds apart, one of them a sibling's.
    ///
    /// Filtering by our own id is the fix rather than a sleep or a global test
    /// mutex, because it removes the assumption instead of hiding it — the test
    /// still proves exactly what it is for (that `publish` rides the seam on the
    /// channel chat really uses, with the payload we set) while being indifferent
    /// to traffic it did not create.
    #[tokio::test]
    async fn publish_reaches_a_subscriber_through_the_seam() {
        // LISTEN/NOTIFY is Postgres. There is no SQLite behaviour to assert here
        // (MAIN-294) — see `crate::testdb::skip_unless_postgres`.
        if crate::testdb::skip_unless_postgres("the chat event-bus seam test") {
            return;
        }
        let Some(st) = crate::testdb::chat_test("the chat event-bus seam test").await else {
            return;
        };
        let pool = st.db.clone();

        // Subscribe first (the seam's LISTEN), then publish, then read it back.
        let mut sub = PgEventBus::new(pool.clone())
            .subscribe(NOTIFY_CHANNEL)
            .await
            .expect("subscribe");
        // Give LISTEN a beat to register before NOTIFY.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let id = Uuid::now_v7();
        let origin = Uuid::now_v7();
        publish(&pool, id, origin, true).await;

        // One timeout around the whole search, not per notice: a busy channel
        // must not be able to extend the test's deadline indefinitely.
        let notice = tokio::time::timeout(Duration::from_secs(3), async {
            while let Some(payload) = sub.next().await {
                let Ok(notice) = serde_json::from_str::<Notice>(&payload) else {
                    continue; // not a Notice at all — somebody else's payload
                };
                if notice.id == id {
                    return notice;
                }
                // A sibling test's post, or another instance's. Not ours; keep reading.
            }
            panic!("the subscription ended before our notice arrived")
        })
        .await
        .expect("our notice arrives before the timeout");

        assert_eq!(notice.id, id);
        assert_eq!(notice.origin, origin);
        assert!(notice.updated);

        st.teardown().await;
    }

    /// The isolation itself, pinned deterministically (MAIN-234 AC-1).
    ///
    /// A repeat loop is weak evidence here: the broken version passed 15
    /// consecutive runs during this investigation before the race happened to
    /// land. So this reproduces the race *on purpose* — a foreign notice before
    /// ours and another after it, which is exactly what a sibling test's `post`
    /// does — and asserts we still receive our own. Against the pre-fix logic
    /// (take the first notice) this fails every time; a future rewrite that
    /// reintroduces the assumption fails here rather than one CI run in three.
    #[tokio::test]
    async fn a_siblings_notice_does_not_satisfy_this_subscription() {
        // LISTEN/NOTIFY is Postgres. There is no SQLite behaviour to assert here
        // (MAIN-294) — see `crate::testdb::skip_unless_postgres`.
        if crate::testdb::skip_unless_postgres("the chat event-bus seam test") {
            return;
        }
        let Some(st) = crate::testdb::chat_test("the chat event-bus seam test").await else {
            return;
        };
        let pool = st.db.clone();

        let mut sub = PgEventBus::new(pool.clone())
            .subscribe(NOTIFY_CHANNEL)
            .await
            .expect("subscribe");
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Somebody else's post lands first — the sibling this test used to lose to.
        let foreign_before = Uuid::now_v7();
        publish(&pool, foreign_before, Uuid::now_v7(), false).await;

        let id = Uuid::now_v7();
        let origin = Uuid::now_v7();
        publish(&pool, id, origin, true).await;

        // …and one lands after, so "take the last" would be no better a rule.
        publish(&pool, Uuid::now_v7(), Uuid::now_v7(), false).await;

        let notice = tokio::time::timeout(Duration::from_secs(3), async {
            while let Some(payload) = sub.next().await {
                let Ok(notice) = serde_json::from_str::<Notice>(&payload) else {
                    continue;
                };
                if notice.id == id {
                    return notice;
                }
            }
            panic!("the subscription ended before our notice arrived")
        })
        .await
        .expect("our notice arrives despite the surrounding traffic");

        assert_eq!(notice.id, id, "we received ours, not the sibling's");
        assert_ne!(notice.id, foreign_before);
        assert_eq!(notice.origin, origin);
        assert!(notice.updated);

        st.teardown().await;
    }
}
