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
pub fn start(registry: Arc<Registry>, pool: DbPool) {
    tokio::spawn(async move {
        loop {
            if let Err(e) = run(&registry, &pool).await {
                tracing::warn!(error = %e, "chat bus listener dropped; reconnecting");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    });
}

async fn run(registry: &Registry, pool: &DbPool) -> anyhow::Result<()> {
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
        if let Some(msg) = crate::messages::fetch(pool, notice.id).await {
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
    #[tokio::test]
    async fn publish_reaches_a_subscriber_through_the_seam() {
        if std::env::var("NOOK_REQUIRE_DB").ok().as_deref() != Some("1") {
            eprintln!(
                "skipping publish_reaches_a_subscriber_through_the_seam — no NOOK_REQUIRE_DB"
            );
            return;
        }
        let Ok(url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let pool = nook_db::connect(&url, 2).await.expect("connect");

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

        let payload = tokio::time::timeout(Duration::from_secs(3), sub.next())
            .await
            .expect("a notice arrives before the timeout")
            .expect("stream yields the notice");
        let notice: Notice = serde_json::from_str(&payload).expect("payload is a Notice");
        assert_eq!(notice.id, id);
        assert_eq!(notice.origin, origin);
        assert!(notice.updated);
    }
}
