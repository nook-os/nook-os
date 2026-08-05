//! Cross-instance message bus over Postgres LISTEN/NOTIFY — no extra infra.
//!
//! Every control-plane instance listens on its own channel
//! (`nook_bus_<instance>`) for directed messages and on `nook_events` for
//! fan-out. Payloads that exceed NOTIFY's ~8KB limit ride through the
//! `bus_outbox` table (the NOTIFY then carries just the row id). A NATS (or
//! other) backend can replace this behind the same `Outbound` contract later.

use std::sync::Arc;

use nook_db::dialect::time_math;
use nook_db::{params, Db, DbPool};
use nook_proto::{AttachServerMessage, ControlToNode, UiEvent};
use nook_types::{NodeId, SessionId, TenantId};
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgListener;
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
        /// See `NodeToControl::GitStatusResult` — same default, same reason:
        /// a peer that omits it is not asserting "not a repository".
        #[serde(default = "crate::ws::bus::yes")]
        is_repo: bool,
        branch: Option<String>,
        files: Vec<nook_types::GitFileStatus>,
        diff: String,
    },
    /// A tunnel frame travelling back to the replica that ISSUED the request
    /// (MAIN-402 AC-4).
    ///
    /// The single-shot pattern beside it — `OpReply`, `GitReply` — cannot carry
    /// this: those resolve one `oneshot` and are done, while a tunnel response
    /// is a head frame followed by an unbounded run of chunks, and every one of
    /// them has to find the same waiting request on a replica that never held
    /// the node's socket.
    ///
    /// `frame` is a `NodeToControl` because that is what the node actually
    /// sent and re-encoding it here would be a second definition of the same
    /// thing (`SessionFrame` carries `AttachServerMessage` for the same
    /// reason). Only the three tunnel variants are legal — `TunnelResponse`,
    /// `TunnelChunk`, `TunnelFailed`; anything else is dropped by the receiver
    /// rather than trusted.
    TunnelFrame {
        request_id: Uuid,
        frame: nook_proto::NodeToControl,
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
    pool: DbPool,
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
                let row: Result<i64, _> = pump_pool
                    .query_scalar(
                        "INSERT INTO bus_outbox (payload) VALUES ($1) RETURNING id",
                        params![&inline],
                    )
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
            if let Err(e) = pump_pool
                .exec("SELECT pg_notify($1, $2)", params![&channel, &payload])
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
            let mut listener = match PgListener::connect_with(listen_pool.pg()).await {
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
                        // Clear readiness FIRST: from here until the reconnect's
                        // new LISTEN is live there is no active listener, so
                        // bus_ready() must not keep reporting true (MAIN-93 AC-2).
                        listen_registry.mark_bus_unready();
                        tracing::warn!(error = %e, "bus listener dropped — reconnecting");
                        break;
                    }
                }
            }
        }
    });

    // ── Maintenance: lease cache refresh + outbox pruning ──────────────────
    // The lease read is the node aggregate's (MAIN-305), so this builds the
    // repository from the pool it already holds. The `bus_outbox` prune below
    // stays inline: this file is the bus MECHANISM and a permanent inline-SQL
    // exemption.
    let lease_nodes = crate::repo::nodes::DbNodeRepository::new(pool.clone());
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(3));
        loop {
            tick.tick().await;
            registry.refresh_lease_cache(&lease_nodes).await;
            let _ = pool
                .exec(
                    &format!(
                        "DELETE FROM bus_outbox WHERE created_at < {cutoff}",
                        cutoff = time_math(pool.engine()).now_minus("60 seconds")
                    ),
                    params![],
                )
                .await;
        }
    });
}

async fn fetch_outbox(pool: &DbPool, id: i64) -> Option<BusMessage> {
    let payload: Option<String> = pool
        .query_scalar_opt(
            "DELETE FROM bus_outbox WHERE id = $1 RETURNING payload",
            params![id],
        )
        .await
        .ok()
        .flatten();
    match serde_json::from_str::<Wire>(&payload?) {
        Ok(Wire::Inline(msg)) => Some(*msg),
        _ => None,
    }
}

/// serde default for booleans that mean "assume yes when an older peer omits
/// the field" — see `BusMessage::GitReply`.
pub(crate) fn yes() -> bool {
    true
}

#[cfg(test)]
mod tunnel_relay_tests {
    use super::*;
    use nook_proto::NodeToControl;

    /// MAIN-402 AC-5: a tunnel frame survives the bus.
    ///
    /// The relay is JSON over `LISTEN/NOTIFY`, so "it compiles" says nothing
    /// about whether the frame that comes out the other side is the one that
    /// went in. This is the only place that can tell — the alternative is
    /// discovering a renamed field with a live tunnel and two replicas.
    fn round_trip(msg: BusMessage) -> BusMessage {
        let wire = serde_json::to_string(&msg).expect("encode");
        serde_json::from_str(&wire).expect("decode")
    }

    #[test]
    fn a_response_head_survives_the_relay() {
        let id = Uuid::now_v7();
        let back = round_trip(BusMessage::TunnelFrame {
            request_id: id,
            frame: NodeToControl::TunnelResponse {
                request_id: id,
                version: nook_proto::TUNNEL_PROTOCOL_VERSION,
                status: 204,
                // Repeats are the reason headers are a Vec and not a map — a
                // map would keep one `set-cookie` and drop the rest, silently.
                headers: vec![
                    ("set-cookie".into(), "a=1".into()),
                    ("set-cookie".into(), "b=2".into()),
                ],
            },
        });
        match back {
            BusMessage::TunnelFrame {
                request_id,
                frame:
                    NodeToControl::TunnelResponse {
                        status, headers, ..
                    },
            } => {
                assert_eq!(request_id, id);
                assert_eq!(status, 204);
                assert_eq!(headers.len(), 2, "both repeats survive: {headers:?}");
            }
            other => panic!("wrong variant back: {other:?}"),
        }
    }

    #[test]
    fn a_chunk_keeps_its_sequence_and_its_end_marker() {
        let id = Uuid::now_v7();
        let back = round_trip(BusMessage::TunnelFrame {
            request_id: id,
            frame: NodeToControl::TunnelChunk {
                request_id: id,
                seq: 7,
                data_b64: "aGVsbG8=".into(),
                last: true,
            },
        });
        match back {
            BusMessage::TunnelFrame {
                frame:
                    NodeToControl::TunnelChunk {
                        seq,
                        data_b64,
                        last,
                        ..
                    },
                ..
            } => {
                assert_eq!(seq, 7);
                assert_eq!(data_b64, "aGVsbG8=");
                // `last` is what tells the far end the body is complete rather
                // than stalled; losing it in transit would hang the response.
                assert!(last);
            }
            other => panic!("wrong variant back: {other:?}"),
        }
    }

    #[test]
    fn a_failure_survives_with_its_reason() {
        let id = Uuid::now_v7();
        let back = round_trip(BusMessage::TunnelFrame {
            request_id: id,
            frame: NodeToControl::TunnelFailed {
                request_id: id,
                message: "nothing listening on port 3000".into(),
            },
        });
        match back {
            BusMessage::TunnelFrame {
                frame: NodeToControl::TunnelFailed { message, .. },
                ..
            } => assert!(message.contains("port 3000"), "{message}"),
            other => panic!("wrong variant back: {other:?}"),
        }
    }

    #[test]
    fn a_request_frame_survives_and_defaults_its_version() {
        // The `#[serde(default)]` on `version` is what lets a peer that predates
        // the field be understood as generation 1 rather than rejected — assert
        // it against a wire form with the field genuinely absent.
        let id = Uuid::now_v7();
        let wire = serde_json::json!({
            "type": "tunnel_request",
            "data": {
                "request_id": id,
                "port": 3000,
                "method": "GET",
                "path": "/x?y=1",
                "headers": [],
                "body_b64": ""
            }
        })
        .to_string();
        let back: ControlToNode = serde_json::from_str(&wire).expect("decode");
        match back {
            ControlToNode::TunnelRequest { version, path, .. } => {
                assert_eq!(version, 1, "an absent version is generation 1");
                // The query survives intact — re-encoding it is how a signature
                // check on the other side starts failing.
                assert_eq!(path, "/x?y=1");
            }
            other => panic!("wrong variant back: {other:?}"),
        }
    }
}
