//! The node's single outbound connection to the control plane, with
//! jittered exponential backoff reconnect. No inbound ports, no SSH.

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use nook_proto::{ControlToNode, NodeToControl};
use rand::Rng;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use crate::config::NodeConfig;
use crate::{capabilities, discovery, sessions, tmux};

const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);
const DISCOVERY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);

pub fn ws_url(server: &str) -> String {
    let base = server.trim_end_matches('/');
    let ws = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        format!("ws://{base}")
    };
    format!("{ws}/api/v1/ws/node")
}

pub async fn run(cfg: NodeConfig) -> Result<()> {
    let mut backoff_secs: u64 = 1;
    loop {
        match connect_once(&cfg).await {
            Ok(()) => {
                tracing::info!("connection closed — reconnecting");
                backoff_secs = 1;
            }
            Err(e) => {
                tracing::warn!(error = %e, "connection failed");
                backoff_secs = (backoff_secs * 2).min(60);
            }
        }
        let jitter = rand::rng().random_range(0..500);
        tokio::time::sleep(std::time::Duration::from_millis(
            backoff_secs * 1000 + jitter,
        ))
        .await;
    }
}

/// One connection lifetime: register, resync, pump until the socket closes.
pub async fn connect_once(cfg: &NodeConfig) -> Result<()> {
    let mut request = ws_url(&cfg.server)
        .into_client_request()
        .context("bad server URL")?;
    request.headers_mut().insert(
        axum_http::AUTHORIZATION,
        format!("Bearer {}", cfg.node_token)
            .parse()
            .context("bad token")?,
    );

    let (socket, _) = connect_async(request).await.context("websocket connect")?;
    tracing::info!(server = %cfg.server, "connected to control plane");
    let (mut sink, mut stream) = socket.split();

    let (out_tx, mut out_rx) = mpsc::channel::<NodeToControl>(1024);

    // Writer: everything → socket.
    let writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let Ok(json) = serde_json::to_string(&msg) else {
                continue;
            };
            if sink.send(Message::Text(json.into())).await.is_err() {
                break;
            }
        }
    });

    // Register: idempotent full resync on every connect.
    out_tx
        .send(NodeToControl::Register {
            capabilities: capabilities::detect(),
            live_tmux_sessions: tmux::list_nook_sessions(),
        })
        .await
        .ok();
    out_tx
        .send(NodeToControl::WorkspacesDiscovered {
            workspaces: discovery::scan(&cfg.workspace_roots),
        })
        .await
        .ok();

    // Heartbeat carries a live resource sample so triage/humans can see which
    // machine can take the work.
    let hb_tx = out_tx.clone();
    let heartbeat = tokio::spawn(async move {
        let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
        let mut sampler = crate::resources::Sampler::new();
        loop {
            interval.tick().await;
            let load = serde_json::to_value(sampler.sample()).unwrap_or_default();
            if hb_tx.send(NodeToControl::Heartbeat { load }).await.is_err() {
                break;
            }
        }
    });

    // Periodic re-discovery.
    let disc_tx = out_tx.clone();
    let roots = cfg.workspace_roots.clone();
    let discovery_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(DISCOVERY_INTERVAL);
        interval.tick().await; // skip immediate (already sent above)
        loop {
            interval.tick().await;
            let workspaces = tokio::task::spawn_blocking({
                let roots = roots.clone();
                move || discovery::scan(&roots)
            })
            .await
            .unwrap_or_default();
            if disc_tx
                .send(NodeToControl::WorkspacesDiscovered { workspaces })
                .await
                .is_err()
            {
                break;
            }
        }
    });

    let mut manager = sessions::Manager::new(out_tx.clone());

    while let Some(msg) = stream.next().await {
        let msg = match msg {
            Ok(Message::Text(t)) => t,
            Ok(Message::Close(_)) | Err(_) => break,
            _ => continue,
        };
        let parsed: ControlToNode = match serde_json::from_str(&msg) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "bad control message");
                continue;
            }
        };
        match parsed {
            ControlToNode::Ping => {
                out_tx.send(NodeToControl::Pong).await.ok();
            }
            ControlToNode::RegisterAck { node_name, .. } => {
                tracing::info!(node = %node_name, "registered");
            }
            ControlToNode::StartSession {
                session_id,
                runtime,
                workspace_path,
                cols,
                rows,
            } => manager.start(session_id, &runtime, &workspace_path, cols, rows),
            ControlToNode::AttachSession {
                session_id,
                tmux_session,
            } => manager.attach(session_id, tmux_session.as_deref()),
            ControlToNode::SessionInput {
                session_id,
                data_b64,
            } => manager.input(session_id, &data_b64),
            ControlToNode::ResizeSession {
                session_id,
                cols,
                rows,
            } => manager.resize(session_id, cols, rows),
            ControlToNode::KillSession { session_id } => manager.kill(session_id),
            ControlToNode::DetachSession { session_id } => manager.detach(session_id),
            ControlToNode::GetGitStatus {
                request_id,
                workspace_path,
            } => {
                let tx = out_tx.clone();
                tokio::task::spawn_blocking(move || {
                    let (branch, files, diff) = discovery::git_status(&workspace_path);
                    let _ = tx.blocking_send(NodeToControl::GitStatusResult {
                        request_id,
                        branch,
                        files,
                        diff,
                    });
                });
            }
            ControlToNode::CloneRepo {
                request_id,
                url,
                dest_name,
                ssh_key,
            } => {
                let tx = out_tx.clone();
                let root = cfg
                    .workspace_roots
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "~/workspace".into());
                let roots = cfg.workspace_roots.clone();
                tokio::task::spawn_blocking(move || {
                    let outcome = crate::gitops::clone_repo(
                        &root,
                        &url,
                        dest_name.as_deref(),
                        ssh_key.as_deref(),
                    );
                    let ok = outcome.ok;
                    let _ = tx.blocking_send(NodeToControl::OpResult {
                        request_id,
                        ok: outcome.ok,
                        path: outcome.path,
                        message: outcome.message,
                    });
                    if ok {
                        // Surface the new checkout immediately.
                        let _ = tx.blocking_send(NodeToControl::WorkspacesDiscovered {
                            workspaces: discovery::scan(&roots),
                        });
                    }
                });
            }
            ControlToNode::AddWorktree {
                request_id,
                repo_path,
                branch,
            } => {
                let tx = out_tx.clone();
                let roots = cfg.workspace_roots.clone();
                tokio::task::spawn_blocking(move || {
                    let outcome = crate::gitops::add_worktree(&repo_path, &branch);
                    let ok = outcome.ok;
                    let _ = tx.blocking_send(NodeToControl::OpResult {
                        request_id,
                        ok: outcome.ok,
                        path: outcome.path,
                        message: outcome.message,
                    });
                    if ok {
                        let _ = tx.blocking_send(NodeToControl::WorkspacesDiscovered {
                            workspaces: discovery::scan(&roots),
                        });
                    }
                });
            }
            ControlToNode::RemoveWorktree {
                request_id,
                worktree_path,
            } => {
                let tx = out_tx.clone();
                let roots = cfg.workspace_roots.clone();
                tokio::task::spawn_blocking(move || {
                    let outcome = crate::gitops::remove_worktree(&worktree_path);
                    let ok = outcome.ok;
                    let _ = tx.blocking_send(NodeToControl::OpResult {
                        request_id,
                        ok: outcome.ok,
                        path: outcome.path,
                        message: outcome.message,
                    });
                    if ok {
                        let _ = tx.blocking_send(NodeToControl::WorkspacesDiscovered {
                            workspaces: discovery::scan(&roots),
                        });
                    }
                });
            }
            ControlToNode::GitCommit {
                request_id,
                checkout_path,
                message,
            } => {
                let tx = out_tx.clone();
                let roots = cfg.workspace_roots.clone();
                tokio::task::spawn_blocking(move || {
                    let outcome = crate::gitops::commit_all(&checkout_path, &message);
                    let ok = outcome.ok;
                    let _ = tx.blocking_send(NodeToControl::OpResult {
                        request_id,
                        ok: outcome.ok,
                        path: outcome.path,
                        message: outcome.message,
                    });
                    // A commit changes the dirty/clean state the UI shows, so
                    // report the new truth rather than waiting for the next
                    // scheduled scan.
                    if ok {
                        let _ = tx.blocking_send(NodeToControl::WorkspacesDiscovered {
                            workspaces: discovery::scan(&roots),
                        });
                    }
                });
            }
            ControlToNode::GitPush {
                request_id,
                checkout_path,
                ssh_key_material,
            } => {
                let tx = out_tx.clone();
                tokio::task::spawn_blocking(move || {
                    let outcome =
                        crate::gitops::push_current(&checkout_path, ssh_key_material.as_deref());
                    let _ = tx.blocking_send(NodeToControl::OpResult {
                        request_id,
                        ok: outcome.ok,
                        path: outcome.path,
                        message: outcome.message,
                    });
                });
            }
            ControlToNode::RemoveCheckout { request_id, path } => {
                let tx = out_tx.clone();
                let roots = cfg.workspace_roots.clone();
                let scan_roots = roots.clone();
                tokio::task::spawn_blocking(move || {
                    let outcome = crate::gitops::remove_checkout(&path, &roots);
                    let ok = outcome.ok;
                    let _ = tx.blocking_send(NodeToControl::OpResult {
                        request_id,
                        ok: outcome.ok,
                        path: outcome.path,
                        message: outcome.message,
                    });
                    if ok {
                        let _ = tx.blocking_send(NodeToControl::WorkspacesDiscovered {
                            workspaces: discovery::scan(&scan_roots),
                        });
                    }
                });
            }
            ControlToNode::SessionWindows {
                request_id,
                tmux_session,
                action,
            } => {
                let tx = out_tx.clone();
                tokio::task::spawn_blocking(move || {
                    use nook_proto::WindowAction as W;
                    // Every action ends by reporting the resulting window list,
                    // so the UI always renders from truth rather than guessing.
                    let applied = match &action {
                        W::List => Ok(()),
                        W::New { cwd } => crate::tmux::new_window(&tmux_session, cwd.as_deref()),
                        W::Split { vertical } => {
                            crate::tmux::split_window(&tmux_session, *vertical)
                        }
                        W::Select { index } => crate::tmux::select_window(&tmux_session, *index),
                        W::Close { index } => crate::tmux::kill_window(&tmux_session, *index),
                        W::Rename { index, name } => {
                            crate::tmux::rename_window(&tmux_session, *index, name)
                        }
                    };
                    let result = applied.and_then(|()| crate::tmux::list_windows(&tmux_session));
                    let _ = tx.blocking_send(match result {
                        Ok(json) => NodeToControl::OpResult {
                            request_id,
                            ok: true,
                            path: None,
                            message: json,
                        },
                        Err(e) => NodeToControl::OpResult {
                            request_id,
                            ok: false,
                            path: None,
                            message: e.to_string(),
                        },
                    });
                });
            }
            ControlToNode::InitProject { request_id, name } => {
                let tx = out_tx.clone();
                let root = cfg
                    .workspace_roots
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "~/workspace".into());
                let roots = cfg.workspace_roots.clone();
                tokio::task::spawn_blocking(move || {
                    let outcome = crate::gitops::init_project(&root, &name);
                    let ok = outcome.ok;
                    let _ = tx.blocking_send(NodeToControl::OpResult {
                        request_id,
                        ok: outcome.ok,
                        path: outcome.path,
                        message: outcome.message,
                    });
                    if ok {
                        let _ = tx.blocking_send(NodeToControl::WorkspacesDiscovered {
                            workspaces: discovery::scan(&roots),
                        });
                    }
                });
            }
            ControlToNode::CaptureSession {
                request_id,
                tmux_session,
                history_lines,
            } => {
                let tx = out_tx.clone();
                tokio::task::spawn_blocking(move || {
                    let result = crate::tmux::capture_pane(&tmux_session, history_lines);
                    let _ = tx.blocking_send(match result {
                        Ok(text) => NodeToControl::OpResult {
                            request_id,
                            ok: true,
                            path: None,
                            message: text,
                        },
                        Err(e) => NodeToControl::OpResult {
                            request_id,
                            ok: false,
                            path: None,
                            message: e.to_string(),
                        },
                    });
                });
            }
            ControlToNode::WriteWorkspaceFile {
                checkout_path,
                name,
                content_b64,
            } => {
                use base64::Engine;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(content_b64.as_bytes())
                    .unwrap_or_default();
                if let Err(e) = crate::gitops::write_workspace_file(&checkout_path, &name, &bytes) {
                    out_tx
                        .send(NodeToControl::Error {
                            context: "write_workspace_file".into(),
                            message: e,
                        })
                        .await
                        .ok();
                } else {
                    tracing::info!(checkout = %checkout_path, file = %name, "workspace file synced");
                }
            }
            ControlToNode::ReadWorkspaceFile {
                request_id,
                checkout_path,
                name,
            } => {
                use base64::Engine;
                let (ok, message) = match crate::gitops::read_workspace_file(&checkout_path, &name)
                {
                    Ok(bytes) => (
                        true,
                        base64::engine::general_purpose::STANDARD.encode(&bytes),
                    ),
                    Err(e) => (false, e),
                };
                out_tx
                    .send(NodeToControl::OpResult {
                        request_id,
                        ok,
                        path: Some(checkout_path),
                        message,
                    })
                    .await
                    .ok();
            }
            ControlToNode::RescanWorkspaces => {
                let workspaces = discovery::scan(&cfg.workspace_roots);
                out_tx
                    .send(NodeToControl::WorkspacesDiscovered { workspaces })
                    .await
                    .ok();
            }
        }
    }

    writer.abort();
    heartbeat.abort();
    discovery_task.abort();
    Ok(())
}

// tokio-tungstenite re-exports http via tungstenite.
mod axum_http {
    pub use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
}
