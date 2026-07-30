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
//! **Authorization boundary (AC-6), enforced per event.** The socket taps the
//! instance firehose (every published event) and forwards an event ONLY if
//! [`channels::access`] authorizes the caller for that event's channel at that
//! moment. This makes the boundary fully dynamic: a member added mid-session (a
//! DM opened with the user, a person joining an org) starts receiving live with
//! no reconnect; a member removed stops immediately; and a cross-tenant intruder
//! never receives a foreign channel's messages. There is no connect-time
//! subscribe snapshot to go stale.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use nook_types::ChatServerMessage;
use tokio::sync::broadcast::error::RecvError;

use crate::{channels, AppState, Caller, ChatError};

pub async fn stream(
    State(state): State<AppState>,
    caller: Caller,
    ws: WebSocketUpgrade,
) -> Result<Response, ChatError> {
    let events = state.registry.subscribe_all();
    Ok(ws.on_upgrade(move |socket| pump(state, caller, socket, events)))
}

async fn pump(
    state: AppState,
    caller: Caller,
    socket: WebSocket,
    mut events: tokio::sync::broadcast::Receiver<ChatServerMessage>,
) {
    let (mut sink, mut stream) = socket.split();
    loop {
        tokio::select! {
            delivered = events.recv() => match delivered {
                Ok(event) => {
                    // AC-6: authorize the caller for THIS event's channel right
                    // now. A non-member (intruder or removed) is skipped; a member
                    // just added is admitted — evaluated per event, so membership
                    // changes take effect live.
                    let channel_id = crate::registry::event_channel(&event);
                    if channels::access(&*state.channels, channel_id, &caller).await.is_err() {
                        continue;
                    }
                    let Ok(text) = serde_json::to_string(&event) else { continue };
                    if sink.send(Message::Text(text.into())).await.is_err() {
                        break; // the client went away
                    }
                }
                // A slow client fell behind; it stays connected and resyncs on
                // reconnect (it can backfill over history).
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            },
            incoming = stream.next() => match incoming {
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(_)) => break,
            },
        }
    }
}
