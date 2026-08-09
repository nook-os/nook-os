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

use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use nook_types::ChatServerMessage;
use tokio::sync::broadcast::error::RecvError;

use crate::{channels, AppState, Caller};
use nook_errors::ApiError;

/// How often the server pings, and half of how long a silent socket lives
/// (MAIN-115 AC-5). Browsers answer pings automatically, so a healthy client
/// costs one tiny frame each way per interval; a network-dropped socket stops
/// answering and is torn down within two intervals instead of lingering
/// "online" until TCP gives up — which can be many minutes.
const HEARTBEAT: Duration = Duration::from_secs(15);

pub async fn stream(
    State(state): State<AppState>,
    caller: Caller,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let events = state.registry.subscribe_all();
    // Who this socket is, as a PERSON (presence is person-derived; two tabs
    // and two tenants are one person). Resolved once — a user row gone
    // mid-session simply streams without presence rather than failing the
    // upgrade.
    let person = state
        .channels
        .person_ref_of(caller.user_id)
        .await
        .ok()
        .flatten()
        .map(|(person, _)| person);
    Ok(ws.on_upgrade(move |socket| pump(state, caller, person, socket, events)))
}

async fn pump(
    state: AppState,
    caller: Caller,
    person: Option<uuid::Uuid>,
    socket: WebSocket,
    mut events: tokio::sync::broadcast::Receiver<ChatServerMessage>,
) {
    // The online edge, only for the person's FIRST socket here (AC-5's
    // multi-tab dedupe lives in the registry).
    if let Some(person) = person {
        if state.registry.socket_opened(person) {
            crate::presence::announce(&state, person, true).await;
        }
    }
    let (mut sink, mut stream) = socket.split();
    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_heard = Instant::now();
    loop {
        tokio::select! {
            delivered = events.recv() => match delivered {
                Ok(event) => {
                    // Authorize per event, per scope (MAIN-115, owner ruling):
                    // a channel-scoped event through the channel gate exactly
                    // as before — a non-member (intruder or removed) is
                    // skipped, a member just added is admitted, evaluated per
                    // event so membership changes take effect live. A
                    // person-scoped event (`None`) through the presence
                    // visibility rule instead, which the channel gate cannot
                    // answer.
                    let allowed = match crate::registry::event_channel(&event) {
                        Some(channel_id) => {
                            channels::access(&*state.channels, channel_id, &caller)
                                .await
                                .is_ok()
                        }
                        None => crate::presence::visible(&state, &caller, &event).await,
                    };
                    if !allowed {
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
                // Anything the client sends — a pong above all — proves the
                // socket alive.
                Some(Ok(_)) => last_heard = Instant::now(),
                Some(Err(_)) => break,
            },
            _ = heartbeat.tick() => {
                // Two silent intervals is a dead peer; the offline edge below
                // fires within one heartbeat of the SECOND missed answer, which
                // is the "within one heartbeat interval" AC-5 asks of a drop.
                if last_heard.elapsed() > HEARTBEAT * 2 {
                    break;
                }
                if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            },
        }
    }
    // The offline edge, only when the person's LAST socket here closes —
    // however it closed: clean Close frame, sink error, or the liveness
    // timeout above.
    if let Some(person) = person {
        if state.registry.socket_closed(person) {
            crate::presence::announce(&state, person, false).await;
        }
    }
}
