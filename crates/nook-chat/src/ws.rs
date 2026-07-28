//! The chat delivery websocket (MAIN-49 AC-3; per-user stream MAIN-117 AC-4/AC-6).
//!
//! A client opens ONE `GET /api/ws` and receives every new message and update
//! from every channel/DM it belongs to, each tagged by `channel_id` (the message
//! carries it). This replaces the old per-open-channel socket: the browser keeps
//! a single connection and routes frames by channel, so a message in a channel
//! the user is NOT viewing still arrives and bumps its unread badge instantly —
//! no polling.
//!
//! The socket is receive-only: posting is the REST endpoint, so there is exactly
//! one write path and one place scope is enforced.
//!
//! **Authorization boundary (AC-6).** At connect the stream subscribes only to
//! the caller's own channels ([`channels::member_channel_ids`]). Each delivered
//! event is then RE-authorized by [`channels::access`] before it is forwarded, so
//! a member removed mid-connection (a DM participant dropped, a person who left an
//! org) stops receiving immediately, and a cross-tenant intruder never receives a
//! foreign channel's messages even if a sender is shared on this instance.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use nook_types::ChatServerMessage;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;

use crate::{channels, AppState, Caller, ChatError};

/// Bound on the merged per-socket queue: a slow client that fills it is dropped
/// from the newest events and resyncs on reconnect, never blocking a sender.
const SOCKET_CAP: usize = 256;

pub async fn stream(
    State(state): State<AppState>,
    caller: Caller,
    ws: WebSocketUpgrade,
) -> Result<Response, ChatError> {
    // Resolve the caller's channel set BEFORE the upgrade, so a caller with no
    // access establishes an (empty) stream rather than leaking anything.
    let channel_ids = channels::member_channel_ids(&state.db, &caller).await?;
    let receivers = channel_ids
        .iter()
        .map(|id| state.registry.subscribe(*id))
        .collect::<Vec<_>>();
    Ok(ws.on_upgrade(move |socket| pump(state, caller, socket, receivers)))
}

/// Merge every subscribed channel's broadcast receiver into one socket. A slim
/// forwarder task per channel drains its broadcast into a shared mpsc; the pump
/// re-authorizes and writes. Forwarders are aborted when the socket closes.
async fn pump(
    state: AppState,
    caller: Caller,
    socket: WebSocket,
    receivers: Vec<tokio::sync::broadcast::Receiver<ChatServerMessage>>,
) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::channel::<ChatServerMessage>(SOCKET_CAP);

    let mut forwarders = Vec::with_capacity(receivers.len());
    for mut brx in receivers {
        let tx = tx.clone();
        forwarders.push(tokio::spawn(async move {
            loop {
                match brx.recv().await {
                    Ok(event) => {
                        if tx.send(event).await.is_err() {
                            break; // the socket went away
                        }
                    }
                    // A slow socket fell behind on this channel; keep going.
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => break,
                }
            }
        }));
    }
    // Keep our own `tx` alive so the socket stays open even with zero channels
    // (the client waits and resyncs); it drops when this task returns.
    let _keepalive = tx;

    loop {
        tokio::select! {
            delivered = rx.recv() => match delivered {
                Some(event) => {
                    // AC-6: re-authorize by the event's channel; a removed member
                    // or an intruder is skipped, delivering nothing.
                    let channel_id = crate::registry::event_channel(&event);
                    if channels::access(&state.db, channel_id, &caller).await.is_err() {
                        continue;
                    }
                    let Ok(text) = serde_json::to_string(&event) else { continue };
                    if sink.send(Message::Text(text.into())).await.is_err() {
                        break; // the client went away
                    }
                }
                None => break,
            },
            incoming = stream.next() => match incoming {
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(_)) => break,
            },
        }
    }

    for f in forwarders {
        f.abort();
    }
}
