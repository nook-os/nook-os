//! `/api/v1/ws/ui` — live event push to signed-in browsers: node status,
//! session status, activity. Deltas only; the UI fetches state over REST.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};

use crate::auth::AuthCtx;
use crate::state::AppState;

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
        &state.db,
        auth.tenant_id,
        &auth,
        state.identity.as_ref(),
    )
    .await
    else {
        return;
    };

    loop {
        tokio::select! {
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
