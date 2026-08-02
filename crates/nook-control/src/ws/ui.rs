//! `/api/v1/ws/ui` — live event push to signed-in browsers: node status,
//! session status, activity. Deltas only; the UI fetches state over REST.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};

use crate::auth::AuthCtx;
use crate::state::AppState;

/// How often to ping an idle browser socket (MAIN-365).
///
/// The node socket has pinged on this cadence since it shipped and its links
/// stay up for hours; this one sent nothing at all between events, so on a
/// quiet tenant it was a silent TCP connection — and a silent connection is
/// what every intermediary reaps. `nook.hein.network` answers on a fronting
/// address, not the control-plane host, so there is at least one proxy in the
/// path whose idle timeout nobody here controls.
///
/// The cost of being wrong in each direction is lopsided, which is why this is
/// short: a frame every twenty seconds is nothing, whereas a dropped socket
/// makes the client refetch its entire cache — the whole UI blanks and
/// repaints at once.
const PING_INTERVAL: std::time::Duration = std::time::Duration::from_secs(20);

pub async fn ui_ws(State(state): State<AppState>, auth: AuthCtx, ws: WebSocketUpgrade) -> Response {
    // Echo the subprotocol. A client that offered one and gets nothing back
    // closes the connection itself, so omitting this breaks exactly the
    // clients that need it.
    ws.protocols([crate::auth::WS_BEARER_PROTOCOL])
        .on_upgrade(move |socket| handle(state, auth, socket))
}

async fn handle(state: AppState, auth: AuthCtx, socket: WebSocket) {
    let mut rx = state.registry.ui_sender(auth.tenant_id).subscribe();
    let (mut sink, mut stream) = socket.split();

    // This connection's activity scope, resolved once (MAIN-134). A member sees
    // only their own activity; owner/admin the whole tenant's. The SAME scope
    // that filters the REST list filters the live push here, so the Activity
    // page's live buffer can't leak what its fetch would hide. If it can't be
    // resolved (a DB blip), close rather than fall open — the UI reconnects.
    let Ok(scope) = crate::services::activity_queries::ActivityScope::load(
        &*state.read_model,
        auth.tenant_id,
        &auth,
        state.identity.as_ref(),
    )
    .await
    else {
        return;
    };

    let mut ping = tokio::time::interval(PING_INTERVAL);
    // The first tick is immediate; skip it so a fresh socket does not open with
    // a ping, and so a burst of reconnects does not all fire at once.
    ping.tick().await;

    loop {
        tokio::select! {
            _ = ping.tick() => {
                if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
            event = rx.recv() => {
                match event {
                    Ok(event) => {
                        // Only the activity feed is scoped per person; other
                        // deltas (node/session status, notifications, task
                        // changes) pass through as before.
                        if let nook_proto::UiEvent::Activity { event: ev } = &event {
                            if !scope.allows(ev) {
                                continue;
                            }
                        }
                        let Ok(json) = serde_json::to_string(&event) else { continue };
                        if sink.send(Message::Text(json.into())).await.is_err() {
                            break;
                        }
                    }
                    // Lagged: this client fell behind; drop it rather than
                    // buffer unboundedly. The UI reconnects and refetches.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            msg = stream.next() => {
                match msg {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                    _ => {} // ignore client chatter (pings handled by axum)
                }
            }
        }
    }
}
