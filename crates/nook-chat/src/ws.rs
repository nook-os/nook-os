//! The chat delivery websocket (MAIN-49 AC-3, AC-5).
//!
//! A client opens `GET /api/channels/{id}/ws` and receives each new message
//! posted to that channel — its own tenant's channel only; the scope check runs
//! before the upgrade, so a non-member never even establishes the socket
//! (AC-5). The socket is receive-only: posting is the REST endpoint, so there is
//! exactly one write path and one place tenant scope is enforced.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use nook_types::ChatServerMessage;
use tokio::sync::broadcast::error::RecvError;
use uuid::Uuid;

use crate::{AppState, Caller, ChatError};

pub async fn subscribe(
    State(state): State<AppState>,
    caller: Caller,
    Path(channel_id): Path<Uuid>,
    ws: WebSocketUpgrade,
) -> Result<Response, ChatError> {
    // Authorize BEFORE the upgrade: another tenant's channel is refused here, so
    // the socket the leak would flow through is never created (AC-5).
    crate::channels::access(&state.db, channel_id, &caller).await?;
    let rx = state.registry.subscribe(channel_id);
    Ok(ws.on_upgrade(move |socket| pump(socket, rx)))
}

async fn pump(
    socket: WebSocket,
    mut rx: tokio::sync::broadcast::Receiver<nook_types::ChatMessage>,
) {
    let (mut sink, mut stream) = socket.split();
    loop {
        tokio::select! {
            // New message on the subscribed channel → push it.
            delivered = rx.recv() => match delivered {
                Ok(msg) => {
                    let envelope = ChatServerMessage::Message(msg);
                    let Ok(text) = serde_json::to_string(&envelope) else { continue };
                    if sink.send(Message::Text(text.into())).await.is_err() {
                        break; // the client went away
                    }
                }
                // A slow client fell behind; it stays connected and resumes with
                // the newest messages (it can backfill the gap over history).
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            },
            // The client hung up (or sent a frame we ignore — receive-only).
            incoming = stream.next() => match incoming {
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(_)) => break,
            },
        }
    }
}
