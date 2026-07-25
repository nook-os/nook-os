//! Cross-instance chat fan-out over Postgres LISTEN/NOTIFY (MAIN-49 AC-3).
//!
//! Mirrors the control plane's bus but far smaller: a post NOTIFYs the new
//! message's id and the instance that posted it; every instance's listener
//! fetches that message and delivers it to its local subscribers, skipping the
//! origin (which already delivered it locally). No extra infrastructure — the
//! shared Postgres is the bus. A message's body can exceed NOTIFY's ~8 KB limit,
//! so only the id travels in the payload and the row is read back by id.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sqlx::postgres::PgListener;
use sqlx::PgPool;
use uuid::Uuid;

use crate::registry::Registry;

/// The single NOTIFY channel chat uses. (LISTEN/NOTIFY channel names are
/// database-global, not tied to the `chat` schema search_path.)
const NOTIFY_CHANNEL: &str = "nook_chat_msg";

#[derive(Serialize, Deserialize)]
struct Notice {
    id: Uuid,
    origin: Uuid,
}

/// Announce a freshly posted message so peer instances deliver it too. Best
/// effort: a failed NOTIFY costs cross-instance liveness for one message, never
/// correctness of what was stored.
pub async fn publish(pool: &PgPool, message_id: Uuid, origin: Uuid) {
    let payload = serde_json::to_string(&Notice {
        id: message_id,
        origin,
    })
    .unwrap_or_default();
    if let Err(e) = sqlx::query("SELECT pg_notify($1, $2)")
        .bind(NOTIFY_CHANNEL)
        .bind(payload)
        .execute(pool)
        .await
    {
        tracing::warn!(error = %e, "chat NOTIFY failed; peers miss this message live");
    }
}

/// Spawn the listener for the life of the process: receive peers' announcements,
/// read the message back, and deliver it to local subscribers. Reconnects on
/// error so a dropped listener connection is self-healing.
pub fn start(registry: Arc<Registry>, pool: PgPool) {
    tokio::spawn(async move {
        loop {
            if let Err(e) = run(&registry, &pool).await {
                tracing::warn!(error = %e, "chat bus listener dropped; reconnecting");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    });
}

async fn run(registry: &Registry, pool: &PgPool) -> anyhow::Result<()> {
    let mut listener = PgListener::connect_with(pool).await?;
    listener.listen(NOTIFY_CHANNEL).await?;
    loop {
        let note = listener.recv().await?;
        let Ok(notice) = serde_json::from_str::<Notice>(note.payload()) else {
            continue;
        };
        // The origin instance already delivered this to its own subscribers.
        if notice.origin == registry.instance() {
            continue;
        }
        if let Some(msg) = crate::messages::fetch(pool, notice.id).await {
            registry.publish_local(msg);
        }
    }
}
