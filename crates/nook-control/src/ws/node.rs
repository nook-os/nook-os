//! `/api/v1/ws/node` — the single persistent connection every node keeps to
//! the control plane. Bearer-authed with the node token issued at join.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use nook_proto::{ControlToNode, NodeToControl, UiEvent};
use nook_types::{NodeId, TenantId};
use tokio::sync::mpsc;

use crate::error::ApiError;
use crate::events::{self, EventDraft};
use crate::seed::hash_token;
use crate::state::AppState;
use crate::ws::registry::NodeHandle;

const NODE_CHANNEL_CAP: usize = 1024;
const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);
const PING_INTERVAL: std::time::Duration = std::time::Duration::from_secs(20);

pub async fn node_ws(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    // Authenticate the upgrade request with the node bearer token.
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string);
    let Some(token) = token else {
        return ApiError::Unauthorized.into_response();
    };

    let row: Option<(NodeId, TenantId, String)> =
        match sqlx::query_as("SELECT id, tenant_id, name FROM nodes WHERE node_token_hash = $1")
            .bind(hash_token(&token))
            .fetch_optional(&state.db)
            .await
        {
            Ok(row) => row,
            Err(e) => return ApiError::from(e).into_response(),
        };
    let Some((node_id, tenant_id, name)) = row else {
        return ApiError::Unauthorized.into_response();
    };

    ws.on_upgrade(move |socket| handle(state, socket, node_id, tenant_id, name))
}

async fn handle(
    state: AppState,
    socket: WebSocket,
    node_id: NodeId,
    tenant: TenantId,
    name: String,
) {
    tracing::info!(%node_id, node = %name, "node connected");
    let (tx, mut rx) = mpsc::channel::<ControlToNode>(NODE_CHANNEL_CAP);
    let epoch = state.registry.register_node(
        node_id,
        NodeHandle {
            tenant_id: tenant,
            tx: tx.clone(),
        },
    );
    // Claim the ownership lease: this instance holds the node's socket. A
    // reconnect elsewhere overwrites it — last writer wins, matching reality.
    let _ = sqlx::query(
        "UPDATE nodes SET owning_instance_id = $2,
            lease_expires_at = now() + make_interval(secs => $3)
         WHERE id = $1",
    )
    .bind(node_id)
    .bind(state.registry.instance_id())
    .bind(crate::ws::bus::LEASE_SECONDS as f64)
    .execute(&state.db)
    .await;

    let (mut sink, mut stream) = socket.split();

    // Writer: registry → socket.
    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let Ok(json) = serde_json::to_string(&msg) else {
                continue;
            };
            if sink.send(Message::Text(json.into())).await.is_err() {
                break;
            }
        }
    });

    // Keepalive pinger.
    let ping_tx = tx.clone();
    let pinger = tokio::spawn(async move {
        let mut interval = tokio::time::interval(PING_INTERVAL);
        loop {
            interval.tick().await;
            if ping_tx.send(ControlToNode::Ping).await.is_err() {
                break;
            }
        }
    });

    // Reader with dead-man timeout.
    loop {
        let next = tokio::time::timeout(IDLE_TIMEOUT, stream.next()).await;
        let msg = match next {
            Err(_) => {
                tracing::warn!(%node_id, "node idle timeout");
                break;
            }
            Ok(None) => break,
            Ok(Some(Err(e))) => {
                tracing::debug!(%node_id, error = %e, "node socket error");
                break;
            }
            Ok(Some(Ok(msg))) => msg,
        };
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(_) => break,
            _ => continue,
        };
        let parsed: NodeToControl = match serde_json::from_str(&text) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(%node_id, error = %e, "bad node message");
                continue;
            }
        };
        if let Err(e) = handle_message(&state, node_id, tenant, &name, parsed, &tx).await {
            tracing::error!(%node_id, error = %e, "error handling node message");
        }
    }

    // Disconnect: offline + detach-preserving (tmux keeps sessions alive).
    writer.abort();
    pinger.abort();
    state.registry.unregister_node(node_id, epoch);
    // Release the lease and mark offline — but only if WE still own it; the
    // node may have already reconnected to another instance.
    let _ = sqlx::query(
        "UPDATE nodes SET status = 'offline', updated_at = now(),
            owning_instance_id = NULL, lease_expires_at = NULL
         WHERE id = $1 AND owning_instance_id = $2",
    )
    .bind(node_id)
    .bind(state.registry.instance_id())
    .execute(&state.db)
    .await;
    state.registry.publish(
        tenant,
        UiEvent::NodeStatus {
            node_id,
            name: name.clone(),
            status: "offline".into(),
        },
    );
    events::record(
        &state,
        tenant,
        EventDraft::new("node.disconnected")
            .actor("node", node_id.0)
            .node(node_id),
    )
    .await;
    tracing::info!(%node_id, node = %name, "node disconnected");
}

async fn handle_message(
    state: &AppState,
    node_id: NodeId,
    tenant: TenantId,
    name: &str,
    msg: NodeToControl,
    _tx: &mpsc::Sender<ControlToNode>,
) -> anyhow::Result<()> {
    match msg {
        NodeToControl::Register {
            capabilities,
            live_tmux_sessions,
        } => {
            sqlx::query(
                "UPDATE nodes SET capabilities = $2, hostname = $3, platform = $4,
                        status = 'online', last_seen_at = now(), updated_at = now()
                 WHERE id = $1",
            )
            .bind(node_id)
            .bind(serde_json::to_value(&capabilities)?)
            .bind(&capabilities.hostname)
            .bind(&capabilities.platform)
            .execute(&state.db)
            .await?;

            // Reconcile: node-reported tmux state is the truth. Any session
            // this node owns whose tmux session no longer exists has exited.
            sqlx::query(
                "UPDATE sessions SET status = 'exited', ended_at = now(), updated_at = now()
                 WHERE node_id = $1
                   AND status IN ('starting', 'running', 'detached')
                   AND (tmux_session IS NULL OR tmux_session != ALL($2))",
            )
            .bind(node_id)
            .bind(&live_tmux_sessions)
            .execute(&state.db)
            .await?;

            _tx.send(ControlToNode::RegisterAck {
                node_id,
                node_name: name.to_string(),
            })
            .await
            .ok();

            state.registry.publish(
                tenant,
                UiEvent::NodeStatus {
                    node_id,
                    name: name.to_string(),
                    status: "online".into(),
                },
            );
            events::record(
                state,
                tenant,
                EventDraft::new("node.connected")
                    .actor("node", node_id.0)
                    .node(node_id)
                    .payload(serde_json::json!({
                        "hostname": capabilities.hostname,
                        "runtimes": capabilities.runtimes,
                    })),
            )
            .await;
        }
        NodeToControl::Heartbeat { load } => {
            // Also renews the ownership lease (only while we still hold it).
            sqlx::query(
                "UPDATE nodes SET last_seen_at = now(), resources = $2,
                    lease_expires_at = CASE WHEN owning_instance_id = $3
                        THEN now() + make_interval(secs => $4)
                        ELSE lease_expires_at END
                 WHERE id = $1",
            )
            .bind(node_id)
            .bind(&load)
            .bind(state.registry.instance_id())
            .bind(crate::ws::bus::LEASE_SECONDS as f64)
            .execute(&state.db)
            .await?;
            state.registry.publish(
                tenant,
                UiEvent::NodeResources {
                    node_id,
                    resources: load,
                },
            );
        }
        NodeToControl::WorkspacesDiscovered { workspaces } => {
            crate::services::discovery::reconcile(state, tenant, node_id, workspaces).await?;
        }
        NodeToControl::SessionStarted {
            session_id,
            tmux_session,
        } => {
            sqlx::query(
                "UPDATE sessions SET status = 'running', tmux_session = $2, updated_at = now()
                 WHERE id = $1 AND tenant_id = $3",
            )
            .bind(session_id)
            .bind(&tmux_session)
            .bind(tenant)
            .execute(&state.db)
            .await?;
            state.registry.publish(
                tenant,
                UiEvent::SessionStatus {
                    session_id,
                    status: "running".into(),
                },
            );
            events::record(
                state,
                tenant,
                EventDraft::new("session.started")
                    .actor("node", node_id.0)
                    .session(session_id)
                    .node(node_id),
            )
            .await;
        }
        NodeToControl::SessionOutput {
            session_id,
            data_b64,
        } => {
            state.registry.publish_session(
                session_id,
                nook_proto::AttachServerMessage::Output { data_b64 },
            );
        }
        NodeToControl::SessionExited {
            session_id,
            exit_code,
        } => {
            sqlx::query(
                "UPDATE sessions SET status = 'exited', ended_at = now(), updated_at = now()
                 WHERE id = $1 AND tenant_id = $2",
            )
            .bind(session_id)
            .bind(tenant)
            .execute(&state.db)
            .await?;
            // Ephemeral secrets exist on disk only while a session is using
            // them; the encrypted copy stays in the vault.
            crate::services::secrets::wipe_ephemeral_for_session(state, tenant, session_id).await;
            state.registry.publish(
                tenant,
                UiEvent::SessionStatus {
                    session_id,
                    status: "exited".into(),
                },
            );
            state.registry.publish_session(
                session_id,
                nook_proto::AttachServerMessage::Status {
                    status: "exited".into(),
                },
            );
            state.registry.drop_attachment(session_id);
            events::record(
                state,
                tenant,
                EventDraft::new("session.exited")
                    .actor("node", node_id.0)
                    .session(session_id)
                    .node(node_id)
                    .payload(serde_json::json!({ "exit_code": exit_code })),
            )
            .await;
        }
        NodeToControl::SessionFailed {
            session_id,
            message,
        } => {
            // The session never opened. Record why on the row and tell both the
            // dashboard and anyone already staring at the terminal, rather than
            // leaving it stuck on "starting".
            sqlx::query(
                "UPDATE sessions SET status = 'error', error = $3, ended_at = now(),
                        updated_at = now()
                 WHERE id = $1 AND tenant_id = $2",
            )
            .bind(session_id)
            .bind(tenant)
            .bind(&message)
            .execute(&state.db)
            .await?;
            state.registry.publish(
                tenant,
                UiEvent::SessionStatus {
                    session_id,
                    status: "error".into(),
                },
            );
            state.registry.publish_session(
                session_id,
                nook_proto::AttachServerMessage::Status {
                    status: format!("error: {message}"),
                },
            );
            events::record(
                state,
                tenant,
                EventDraft::new("session.failed")
                    .actor("node", node_id.0)
                    .session(session_id)
                    .node(node_id)
                    .payload(serde_json::json!({ "message": message })),
            )
            .await;
        }
        NodeToControl::Error { context, message } => {
            tracing::warn!(%node_id, context, message, "node reported error");
            events::record(
                state,
                tenant,
                EventDraft::new("node.error")
                    .actor("node", node_id.0)
                    .node(node_id)
                    .payload(serde_json::json!({ "context": context, "message": message })),
            )
            .await;
        }
        NodeToControl::GitStatusResult {
            request_id,
            branch,
            files,
            diff,
        } => {
            state.registry.complete_git_status(
                request_id,
                crate::ws::registry::GitStatusPayload {
                    branch,
                    files,
                    diff,
                },
            );
        }
        NodeToControl::OpResult {
            request_id,
            ok,
            path,
            message,
        } => {
            state.registry.complete_op(
                request_id,
                crate::ws::registry::OpPayload { ok, path, message },
            );
        }
        NodeToControl::Pong => {}
    }
    Ok(())
}
