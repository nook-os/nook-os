//! `/api/v1/ws/node` — the single persistent connection every node keeps to
//! the control plane.
//!
//! Authenticated by the node's **client certificate** when there is one, and by
//! the join-time bearer token otherwise. The certificate is strictly stronger:
//! a token is a shared secret that appears in headers, logs and process
//! listings, whereas the certificate proves possession of a private key that
//! never left the machine, and it can be revoked and rotated per node.
//!
//! Both are accepted because a fleet migrates one machine at a time. The token
//! path is what goes away once every node has enrolled — not something to
//! remove while nodes still depend on it.

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
use crate::repo::nodes::NodeIdentity;
use crate::seed::hash_token;
use crate::state::AppState;
use crate::ws::registry::NodeHandle;

const NODE_CHANNEL_CAP: usize = 1024;
const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);
const PING_INTERVAL: std::time::Duration = std::time::Duration::from_secs(20);

pub async fn node_ws(
    State(state): State<AppState>,
    peer_cert: Option<axum::Extension<crate::agent_tls::PeerCertificate>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    // Certificate first, when the connection came in on the mTLS listener.
    //
    // `agent_tls` lifted this straight off the completed handshake into the
    // request's extensions, which a client cannot write to — unlike a header,
    // there is no way to inject one from the wire. `verify_node_cert` then does
    // the real work: chain it against *that tenant's* live trust bundle, and
    // refuse a node that has been revoked.
    if let Some(axum::Extension(cert)) = peer_cert {
        return match crate::ca::verify_node_cert(&*state.tenant_cas, &*state.nodes, &cert.0).await {
            Ok(id) => {
                let name = state
                    .nodes
                    .name_of(NodeId(id.node_id))
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| "node".into());
                tracing::debug!(node_id = %id.node_id, "node authenticated by certificate");
                ws.on_upgrade(move |socket| {
                    handle(
                        state,
                        socket,
                        NodeId(id.node_id),
                        TenantId(id.tenant_id),
                        name,
                    )
                })
            }
            Err(e) => {
                // Presenting a certificate and having it rejected is not the
                // same as presenting none — say so, rather than silently
                // falling through to the token path and reporting a confusing
                // "unauthorized" for what is really an expired or revoked cert.
                tracing::warn!(error = %e, "node certificate rejected");
                ApiError::Unauthorized.into_response()
            }
        };
    }

    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string);
    let Some(token) = token else {
        return ApiError::Unauthorized.into_response();
    };

    let row = match state.nodes.by_token_hash(&hash_token(&token)).await {
        Ok(row) => row,
        Err(e) => return e.into_response(),
    };
    let Some(NodeIdentity {
        id: node_id,
        tenant: tenant_id,
        name,
    }) = row
    else {
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
    let _ = state
        .nodes
        .take_lease(
            node_id,
            state.registry.instance_id(),
            crate::ws::bus::LEASE_SECONDS as f64,
        )
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
    let _ = state
        .nodes
        .release_lease(node_id, state.registry.instance_id())
        .await;
    // Any loop job this node was executing died with it — fail it honestly with
    // its transcript tail preserved, rather than leaving it "running" forever
    // (MAIN-161 AC-4). The node cleans the orphaned worktree on its next connect.
    let _ = crate::services::jobs::fail_stranded_for_node(&state, tenant, node_id).await;
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
            state
                .nodes
                .record_capabilities(
                    node_id,
                    crate::repo::nodes::ReportedCapabilities {
                        capabilities: serde_json::to_value(&capabilities)?,
                        hostname: capabilities.hostname.clone(),
                        platform: capabilities.platform.clone(),
                    },
                )
                .await?;

            // Reconcile: node-reported tmux state is the truth. Any session
            // this node owns whose tmux session no longer exists has exited.
            state
                .nodes
                .expire_sessions_missing_from_tmux(node_id, &live_tmux_sessions)
                .await?;

            // What this tenant trusts, so the node can tell whether a rotation
            // is being staged and renew for it rather than waiting for expiry.
            let ca_fingerprints = crate::ca::trust_bundle(&*state.tenant_cas, tenant)
                .await
                .map(|cas| cas.into_iter().map(|c| c.fingerprint).collect())
                .unwrap_or_default();

            _tx.send(ControlToNode::RegisterAck {
                expected_agent_version: Some(env!("CARGO_PKG_VERSION").to_string()),
                node_id,
                node_name: name.to_string(),
                ca_fingerprints,
            })
            .await
            .ok();

            // Convergence, and the reason skills are stored rather than only
            // pushed. A node that was offline when something was taught — or
            // one joining for the first time — learns the whole set here, so
            // "my fleet knows this" stops depending on which machines happened
            // to be awake when somebody ran `nook teach`. The node skips writes
            // whose content it already has, so the steady state is free.
            match crate::routes::skills::all_for_tenant(&*state.skills, tenant).await {
                Ok(msgs) => {
                    if !msgs.is_empty() {
                        tracing::debug!(node = %name, count = msgs.len(), "syncing skills");
                    }
                    for m in msgs {
                        _tx.send(m).await.ok();
                    }
                }
                // Not fatal: a node that connects but misses a skill sync is
                // far better than one that cannot connect at all.
                Err(e) => tracing::warn!(node = %name, error = %e, "cannot sync skills"),
            }

            // Then the MANAGED store (MAIN-105 AC-3): the nookos skill and the
            // hook set the deployment ships. Global content, not tenant-scoped —
            // replayed to every node on connect, so a machine that was offline
            // when a new default shipped converges here. The node skips writes
            // whose sha it already has, so the steady state is free.
            match crate::routes::managed::managed_skills_as_install(&*state.managed).await {
                Ok(msgs) => {
                    for m in msgs {
                        _tx.send(m).await.ok();
                    }
                }
                Err(e) => tracing::warn!(node = %name, error = %e, "cannot sync managed skills"),
            }
            match crate::routes::managed::managed_hooks_as_install(&*state.managed).await {
                Ok(Some(m)) => {
                    _tx.send(m).await.ok();
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(node = %name, error = %e, "cannot sync managed hooks"),
            }

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
            state
                .nodes
                .record_resources(
                    node_id,
                    &load,
                    state.registry.instance_id(),
                    crate::ws::bus::LEASE_SECONDS as f64,
                )
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
        NodeToControl::SkillInstalled {
            name: skill,
            agents,
            paths,
            error,
        } => {
            // Recorded as an event rather than kept in a table. What an
            // operator needs to answer is "did this land, and where" at the
            // moment they taught it — a question about an occurrence, which is
            // what the activity log is for. A per-node skill-state table would
            // have to be reconciled against machines that get reinstalled, and
            // would then be one more thing that can be wrong.
            if let Some(e) = &error {
                tracing::warn!(node = %name, skill = %skill, error = %e, "node could not learn a skill");
            }
            events::record(
                state,
                tenant,
                EventDraft::new(if error.is_some() {
                    "skill.install_failed"
                } else {
                    "skill.installed"
                })
                .actor("node", node_id.0)
                .node(node_id)
                .payload(serde_json::json!({
                    "skill": skill,
                    "agents": agents,
                    "paths": paths,
                    "error": error,
                })),
            )
            .await;
        }
        NodeToControl::RuntimeCredentialInstalled {
            runtime,
            path,
            error,
        } => {
            // Same contract as SkillInstalled/HooksInstalled: an occurrence, so
            // it is an event rather than a table. The authorization STATE it
            // produces arrives separately as `RuntimeAuthStatus` and lands on
            // the node's capabilities — this record is the "did that delivery
            // work, and where did it land" question.
            //
            // The payload is never in the event. A credential in the activity
            // log is a credential in the database, which is the one thing this
            // whole path exists to avoid (MAIN-283 AC-4).
            if let Some(e) = &error {
                tracing::warn!(
                    node = %name, %runtime, error = %e,
                    "node could not install a runtime credential"
                );
            }
            events::record(
                state,
                tenant,
                EventDraft::new(if error.is_some() {
                    "runtime_credential.install_failed"
                } else {
                    "runtime_credential.installed"
                })
                .actor("node", node_id.0)
                .node(node_id)
                .payload(serde_json::json!({
                    "runtime": runtime,
                    "path": path,
                    "error": error,
                })),
            )
            .await;
        }
        NodeToControl::HooksInstalled { path, error } => {
            // Same contract as SkillInstalled (MAIN-105 AC-5): a failure is a
            // recorded event that flows through notable→notification, a success
            // is logged to activity. No per-node table — events are the record.
            if let Some(e) = &error {
                tracing::warn!(node = %name, error = %e, "node could not apply managed hooks");
            }
            events::record(
                state,
                tenant,
                EventDraft::new(if error.is_some() {
                    "hooks.install_failed"
                } else {
                    "hooks.installed"
                })
                .actor("node", node_id.0)
                .node(node_id)
                .payload(serde_json::json!({
                    "path": path,
                    "error": error,
                })),
            )
            .await;
        }
        NodeToControl::RuntimeAuthStatus { profiles } => {
            // A re-probe after an authorize session (MAIN-126 AC-4): merge the
            // fresh profiles into the node's stored capabilities and nudge the
            // UI, so the Agent-authorization panel flips to `authorized` without
            // waiting for the node to reconnect. The node stays online; the
            // NodeStatus event is only the "refetch this node" signal the Nodes
            // queries already listen for.
            let value = serde_json::to_value(&profiles).unwrap_or_else(|_| serde_json::json!([]));
            // The capabilities merge routes through the json seam's `set`
            // (MAIN-201): the node-supplied profiles stay bound; only the
            // static `{runtime_auth}` path and create-missing live in the SQL.
            let _ = state.nodes.merge_runtime_auth(node_id, &value).await;
            state.registry.publish(
                tenant,
                UiEvent::NodeStatus {
                    node_id,
                    name: name.to_string(),
                    status: "online".into(),
                },
            );
        }
        NodeToControl::SessionStarted {
            session_id,
            tmux_session,
        } => {
            state
                .nodes
                .mark_session_running(session_id, tenant, &tmux_session)
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
            state.nodes.mark_session_exited(session_id, tenant).await?;
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
            // A dead session has no agent state — clear it so a spinner does not
            // outlive the terminal, on screen now or on the next reload.
            clear_agent_state(state, tenant, session_id);
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
            state
                .nodes
                .mark_session_failed(session_id, tenant, &message)
                .await?;
            state.registry.publish(
                tenant,
                UiEvent::SessionStatus {
                    session_id,
                    status: "error".into(),
                },
            );
            clear_agent_state(state, tenant, session_id);
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
            is_repo,
            branch,
            files,
            diff,
        } => {
            state.registry.complete_git_status(
                request_id,
                crate::ws::registry::GitStatusPayload {
                    is_repo,
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
        // A running loop job streamed a chunk of output (MAIN-161). Appended to
        // the transcript verbatim — never interpreted (NG-2). A bad id or a job
        // that has since vanished is dropped, not fatal to the connection.
        // A real turn boundary from the streaming adapter (MAIN-240). Scoped to
        // this node's own job for the same reason transcript lines are: a node
        // token must not be able to drive another executor's UI.
        NodeToControl::JobTurn { job_id, active } => {
            if let Ok(id) = job_id.parse::<uuid::Uuid>() {
                crate::services::jobs::turn_from_node(
                    state,
                    tenant,
                    node_id,
                    nook_types::JobId(id),
                    active,
                )
                .await;
            }
        }
        NodeToControl::JobTranscript {
            job_id,
            source,
            content,
        } => {
            if let Ok(id) = job_id.parse::<uuid::Uuid>() {
                // Scoped to THIS node's own job (security): a node token cannot
                // inject into another executor's transcript.
                let _ = crate::services::jobs::transcript_from_node(
                    state,
                    tenant,
                    node_id,
                    nook_types::JobId(id),
                    &source,
                    &content,
                )
                .await;
            }
        }
        // A loop job's session ended (MAIN-161): completed on success, else
        // failed with the tail preserved (AC-4).
        NodeToControl::JobFinished {
            job_id,
            ok,
            message,
        } => {
            if let Ok(id) = job_id.parse::<uuid::Uuid>() {
                // Scoped to THIS node's own job (security): a node token cannot
                // complete or fail another executor's job.
                let _ = crate::services::jobs::finish_from_node(
                    state,
                    tenant,
                    node_id,
                    nook_types::JobId(id),
                    ok,
                    &message,
                )
                .await;
            }
        }
        NodeToControl::Pong => {}
    }
    Ok(())
}

/// Clear a session's agent state on death and tell every browser to drop the
/// spinner. Only publishes when there was something to clear, so an ordinary
/// exit of a session that never ran an agent stays silent.
fn clear_agent_state(state: &AppState, tenant: TenantId, session_id: nook_types::SessionId) {
    if state.registry.clear_agent_state(session_id) {
        state.registry.publish(
            tenant,
            UiEvent::SessionAgentState {
                session_id,
                window: None,
                state: "idle".into(),
            },
        );
    }
}
