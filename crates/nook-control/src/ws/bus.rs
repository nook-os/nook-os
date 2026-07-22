//! Cross-instance message bus over Postgres LISTEN/NOTIFY — no extra infra.
//!
//! Every control-plane instance listens on its own channel
//! (`nook_bus_<instance>`) for directed messages and on `nook_events` for
//! fan-out. Payloads that exceed NOTIFY's ~8KB limit ride through the
//! `bus_outbox` table (the NOTIFY then carries just the row id). A NATS (or
//! other) backend can replace this behind the same `Outbound` contract later.

use std::sync::Arc;

use nook_proto::{AttachServerMessage, ControlToNode, UiEvent};
use nook_types::{NodeId, SessionId, TenantId};
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgListener;
use sqlx::PgPool;
use tokio::sync::mpsc;
use uuid::Uuid;

use super::registry::Registry;

/// How long a node lease lives without renewal (renewed each 15s heartbeat).
pub const LEASE_SECONDS: i64 = 45;
/// Inline NOTIFY payload budget; larger messages go through `bus_outbox`.
const MAX_INLINE: usize = 7000;
/// Broadcast channel every instance subscribes to.
const EVENTS_CHANNEL: &str = "nook_events";

pub fn instance_channel(instance: Uuid) -> String {
    format!("nook_bus_{}", instance.simple())
}

/// Everything that crosses instances. `origin` guards against echo on the
/// broadcast channel.
#[derive(Debug, Serialize, Deserialize)]
pub enum BusMessage {
    /// Deliver a control message to a node owned by the receiving instance.
    /// `reply_to` is set when `msg` carries a request id whose answer must be
    /// routed back to the requesting instance.
    ToNode {
        node_id: NodeId,
        reply_to: Option<Uuid>,
        msg: ControlToNode,
    },
    /// Answer to a request that originated on the receiving instance.
    OpReply {
        request_id: Uuid,
        ok: bool,
        path: Option<String>,
        message: String,
    },
    GitReply {
        request_id: Uuid,
        branch: Option<String>,
        files: Vec<nook_types::GitFileStatus>,
        diff: String,
    },
    /// Terminal frame for viewers attached on the receiving instance.
    SessionFrame {
        session_id: SessionId,
        frame: AttachServerMessage,
    },
    /// Tenant UI event fan-out (broadcast).
    UiEvt {
        origin: Uuid,
        tenant: TenantId,
        event: UiEvent,
    },
    /// The sending instance has viewers for this session (owned by receiver).
    Subscribe {
        session_id: SessionId,
        instance: Uuid,
    },
    Unsubscribe {
        session_id: SessionId,
        instance: Uuid,
    },
    /// Viewer/driver sizing events routed to the session's owning instance so
    /// driver state lives in exactly one place.
    Viewer {
        session_id: SessionId,
        instance: Uuid,
        viewer: u64,
        event: ViewerEvent,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ViewerEvent {
    Attached,
    Resize { cols: u16, rows: u16 },
    Input,
    Detached,
}

/// What the registry queues for delivery.
pub enum Outbound {
    Direct { to: Uuid, msg: BusMessage },
    Broadcast(BusMessage),
}

/// Wire envelope: inline JSON or an outbox row reference.
#[derive(Serialize, Deserialize)]
enum Wire {
    #[serde(rename = "i")]
    Inline(Box<BusMessage>),
    #[serde(rename = "o")]
    Outbox(i64),
}

/// Spawn the bus tasks: outbound pump, listener, and maintenance loop.
pub(crate) fn start(
    registry: Arc<Registry>,
    pool: PgPool,
    mut outbound: mpsc::UnboundedReceiver<Outbound>,
) {
    let me = registry.instance_id();

    // ── Outbound pump ──────────────────────────────────────────────────────
    let pump_pool = pool.clone();
    tokio::spawn(async move {
        while let Some(out) = outbound.recv().await {
            let (channel, msg) = match out {
                Outbound::Direct { to, msg } => (instance_channel(to), msg),
                Outbound::Broadcast(msg) => (EVENTS_CHANNEL.to_string(), msg),
            };
            let Ok(inline) = serde_json::to_string(&Wire::Inline(Box::new(msg))) else {
                continue;
            };
            let payload = if inline.len() <= MAX_INLINE {
                inline
            } else {
                // Oversized: park the full envelope in the outbox and notify
                // with just the row id.
                let row: Result<i64, _> =
                    sqlx::query_scalar("INSERT INTO bus_outbox (payload) VALUES ($1) RETURNING id")
                        .bind(&inline)
                        .fetch_one(&pump_pool)
                        .await;
                match row {
                    Ok(id) => match serde_json::to_string(&Wire::Outbox(id)) {
                        Ok(s) => s,
                        Err(_) => continue,
                    },
                    Err(e) => {
                        tracing::warn!(error = %e, "bus outbox insert failed");
                        continue;
                    }
                }
            };
            if let Err(e) = sqlx::query("SELECT pg_notify($1, $2)")
                .bind(&channel)
                .bind(&payload)
                .execute(&pump_pool)
                .await
            {
                tracing::warn!(error = %e, "bus notify failed");
            }
        }
    });

    // ── Listener (with reconnect) ──────────────────────────────────────────
    let listen_pool = pool.clone();
    let listen_registry = registry.clone();
    tokio::spawn(async move {
        loop {
            let mut listener = match PgListener::connect_with(&listen_pool).await {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!(error = %e, "bus listener connect failed");
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    continue;
                }
            };
            if listener
                .listen_all([instance_channel(me).as_str(), EVENTS_CHANNEL])
                .await
                .is_err()
            {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                continue;
            }
            // LISTEN is live now — anything published from here on reaches us.
            // Callers awaiting bus_ready() can proceed. (Signalled on every
            // (re)connect; the watch collapses repeats.)
            listen_registry.mark_bus_ready();
            loop {
                match listener.recv().await {
                    Ok(n) => {
                        let msg = match serde_json::from_str::<Wire>(n.payload()) {
                            Ok(Wire::Inline(msg)) => Some(*msg),
                            Ok(Wire::Outbox(id)) => fetch_outbox(&listen_pool, id).await,
                            Err(e) => {
                                tracing::debug!(error = %e, "bad bus payload");
                                None
                            }
                        };
                        if let Some(msg) = msg {
                            listen_registry.handle_bus(msg);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "bus listener dropped — reconnecting");
                        break;
                    }
                }
            }
        }
    });

    // ── Maintenance: lease cache refresh + outbox pruning ──────────────────
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(3));
        loop {
            tick.tick().await;
            registry.refresh_lease_cache(&pool).await;
            let _ = sqlx::query(
                "DELETE FROM bus_outbox WHERE created_at < now() - interval '60 seconds'",
            )
            .execute(&pool)
            .await;
        }
    });
}

async fn fetch_outbox(pool: &PgPool, id: i64) -> Option<BusMessage> {
    let payload: Option<String> =
        sqlx::query_scalar("DELETE FROM bus_outbox WHERE id = $1 RETURNING payload")
            .bind(id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
    match serde_json::from_str::<Wire>(&payload?) {
        Ok(Wire::Inline(msg)) => Some(*msg),
        _ => None,
    }
}
