//! `/api/v1/ws/sessions/:id/attach` — the browser side of a terminal.
//!
//! xterm.js ⇄ this socket ⇄ SessionRouter ⇄ node WS ⇄ PTY ⇄ tmux. Multiple
//! viewers fan out naturally: each subscribes to the session's broadcast.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use nook_proto::{AttachClientMessage, AttachServerMessage, ControlToNode, UiEvent};
use nook_types::{Session, SessionId};

use crate::auth::AuthCtx;
use crate::error::ApiError;
use crate::state::AppState;

pub async fn attach_ws(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<SessionId>,
    ws: WebSocketUpgrade,
) -> Response {
    let session: Option<Session> = match sqlx::query_as("SELECT * FROM sessions WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await
    {
        Ok(s) => s,
        Err(e) => return ApiError::from(e).into_response(),
    };
    let Some(session) = session else {
        return ApiError::NotFound.into_response();
    };
    // THE session-content route: this socket carries the raw terminal stream —
    // every keystroke and every byte of output. Membership only; no role at any
    // scope reaches it. See auth/session_guard.rs.
    if let Err(e) = auth.require_session_access(&state, session.tenant_id).await {
        return e.into_response();
    }
    // Attaching is a terminal on that machine: keystrokes in, output out.
    if let Err(e) = auth.require_node_self(session.node_id) {
        return e.into_response();
    }
    ws.protocols([crate::auth::WS_BEARER_PROTOCOL])
        .on_upgrade(move |socket| handle(state, socket, session))
}

async fn handle(state: AppState, socket: WebSocket, session: Session) {
    let session_id = session.id;
    let tenant = session.tenant_id;
    let sender = state.registry.attachment_sender(session_id);
    let mut rx = sender.subscribe();
    let viewer_id = state.registry.new_viewer_id();
    state.registry.viewer_attached(session_id, viewer_id);

    // Ask the node to (re)attach and replay the screen.
    state.registry.send_to_node(
        session.node_id,
        ControlToNode::AttachSession {
            session_id,
            tmux_session: session.tmux_session.clone(),
        },
    );

    // Viewer arrived → session is being watched.
    mark_status(&state, &session, "running").await;

    let (mut sink, mut stream) = socket.split();

    // Late joiner: adopt the current agreed grid immediately.
    if let Some((cols, rows)) = state.registry.current_size(session_id) {
        if let Ok(json) = serde_json::to_string(&AttachServerMessage::Size { cols, rows }) {
            let _ = sink.send(Message::Text(json.into())).await;
        }
    }

    loop {
        tokio::select! {
            out = rx.recv() => {
                match out {
                    Ok(msg) => {
                        let Ok(json) = serde_json::to_string(&msg) else { continue };
                        if sink.send(Message::Text(json.into())).await.is_err() {
                            break;
                        }
                    }
                    // Slow viewer: drop it, the browser reconnects and replays.
                    Err(_) => break,
                }
            }
            msg = stream.next() => {
                let msg = match msg {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                    Some(Ok(Message::Text(t))) => t,
                    _ => continue,
                };
                match serde_json::from_str::<AttachClientMessage>(&msg) {
                    Ok(AttachClientMessage::Input { data_b64 }) => {
                        state.registry.send_to_node(
                            session.node_id,
                            ControlToNode::SessionInput { session_id, data_b64 },
                        );
                        // Typing claims the session: the PTY adopts this
                        // viewer's size (tmux `window-size latest`, but keyed
                        // to typing so read-only viewers never shrink anyone).
                        if let Some((cols, rows)) =
                            state.registry.viewer_input(session_id, viewer_id)
                        {
                            state.registry.send_to_node(
                                session.node_id,
                                ControlToNode::ResizeSession { session_id, cols, rows },
                            );
                            state.registry.publish_session(
                                session_id,
                                AttachServerMessage::Size { cols, rows },
                            );
                        }
                    }
                    Ok(AttachClientMessage::Resize { cols, rows }) => {
                        // Applied only when this viewer is the driver;
                        // spectators' sizes are stored for a later takeover.
                        if let Some((cols, rows)) =
                            state.registry.viewer_resize(session_id, viewer_id, cols, rows)
                        {
                            state.registry.send_to_node(
                                session.node_id,
                                ControlToNode::ResizeSession { session_id, cols, rows },
                            );
                            state.registry.publish_session(
                                session_id,
                                AttachServerMessage::Size { cols, rows },
                            );
                        }
                    }
                    Err(e) => tracing::debug!(error = %e, "bad attach client message"),
                }
            }
        }
    }

    // If the driver left, the most recently active remaining viewer takes over
    // and the PTY adopts its size.
    if let Some((cols, rows)) = state.registry.viewer_detached(session_id, viewer_id) {
        state.registry.send_to_node(
            session.node_id,
            ControlToNode::ResizeSession {
                session_id,
                cols,
                rows,
            },
        );
        state
            .registry
            .publish_session(session_id, AttachServerMessage::Size { cols, rows });
    }

    // Last viewer gone → detached (tmux keeps the session alive on the node;
    // the node stops forwarding output frames until someone attaches again, so
    // idle sessions cost no bandwidth at fleet scale).
    drop(rx);
    if sender.receiver_count() == 0 {
        state
            .registry
            .send_to_node(session.node_id, ControlToNode::DetachSession { session_id });
        let still_running: Option<(String,)> =
            sqlx::query_as("SELECT status FROM sessions WHERE id = $1")
                .bind(session_id)
                .fetch_optional(&state.db)
                .await
                .ok()
                .flatten();
        if matches!(still_running, Some((ref s,)) if s == "running" || s == "starting") {
            mark_status(&state, &session, "detached").await;
        }
    }
    let _ = tenant;
}

async fn mark_status(state: &AppState, session: &Session, status: &str) {
    let res = sqlx::query(
        "UPDATE sessions SET status = $2, updated_at = now()
         WHERE id = $1 AND status IN ('starting', 'running', 'detached')",
    )
    .bind(session.id)
    .bind(status)
    .execute(&state.db)
    .await;
    if matches!(res, Ok(r) if r.rows_affected() > 0) {
        state.registry.publish(
            session.tenant_id,
            UiEvent::SessionStatus {
                session_id: session.id,
                status: status.to_string(),
            },
        );
    }
}
