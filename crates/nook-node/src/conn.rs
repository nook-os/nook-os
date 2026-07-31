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
/// How often to reconsider our certificate.
///
/// Six hours against a seven-day renewal window: a node that is asleep, or one
/// whose control plane is briefly down, gets dozens of chances before anything
/// expires. The staged-CA case does not wait for this at all — it arrives as a
/// push.
const CERT_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);

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
    // Refuse plaintext before the first connection rather than after — a node
    // that has already streamed a session over the clear has nothing left to
    // protect. The hatch is checked once here and announced on every start.
    let insecure = crate::config::check_server_security(&cfg.server, false)?;
    crate::config::warn_if_insecure(insecure, &cfg.server);

    let mut backoff_secs: u64 = 1;
    loop {
        match connect_once(&cfg).await {
            Ok(()) => {
                tracing::info!("connection closed — reconnecting");
                backoff_secs = 1;
            }
            Err(e) => {
                // The whole chain, not just the top: "websocket connect
                // (mTLS)" on its own cannot distinguish a revoked certificate
                // from a wrong pin from a server that is simply down.
                let cause = e
                    .chain()
                    .map(|c| c.to_string())
                    .collect::<Vec<_>>()
                    .join(": ");
                tracing::warn!(error = %cause, "connection failed");
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
    let mut request = ws_url(cfg.agent_endpoint())
        .into_client_request()
        .context("bad server URL")?;
    request.headers_mut().insert(
        axum_http::AUTHORIZATION,
        format!("Bearer {}", cfg.node_token)
            .parse()
            .context("bad token")?,
    );

    // A pinned fingerprint applies to EVERY connection, not just the enrolment
    // that established it — pinning only at join would leave every later
    // reconnect trusting whatever the web PKI vouches for.
    // Three cases, strongest first: a machine that has enrolled presents its
    // certificate (mutual TLS); one that has only a pin verifies the server
    // but authenticates with its token; one with neither falls back to plain
    // web-PKI validation.
    let identity = crate::config::load_identity();
    let (socket, _) = match (&identity, cfg.server_fingerprint.as_deref()) {
        (Some((cert, key)), fp) => {
            let tls = tokio_tungstenite::Connector::Rustls(std::sync::Arc::new(
                crate::pinning::mutual_client_config(fp, cert, key)
                    .context("this machine's certificate is unusable")?,
            ));
            tokio_tungstenite::connect_async_tls_with_config(request, None, false, Some(tls))
                .await
                .context("websocket connect (mTLS)")?
        }
        (None, Some(fp)) => {
            let tls = tokio_tungstenite::Connector::Rustls(std::sync::Arc::new(
                crate::pinning::pinned_client_config(fp),
            ));
            tokio_tungstenite::connect_async_tls_with_config(request, None, false, Some(tls))
                .await
                .context("websocket connect (pinned)")?
        }
        (None, None) => connect_async(request).await.context("websocket connect")?,
    };
    if identity.is_some() {
        tracing::debug!("presenting this machine's client certificate");
    }
    // Report where we actually dialled and how we authenticated. Logging
    // `cfg.server` here was actively misleading once the agent moved to its
    // own endpoint: it named a host this connection never touched, so the log
    // looked identical whether or not the migration had taken effect.
    tracing::info!(
        server = %cfg.agent_endpoint(),
        auth = if identity.is_some() { "certificate" } else { "token" },
        "connected to control plane"
    );
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

    // Best-effort: on every (re)connect, prune loop-job worktrees orphaned by a
    // crash or a node restart (MAIN-161 AC-4). Blocking git, non-fatal.
    {
        let cfg = cfg.clone();
        tokio::task::spawn_blocking(move || crate::loop_job::reconcile(&cfg));
    }

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

    // Certificate upkeep.
    //
    // The push and the reconnect handle a staged rotation; this is the ordinary
    // expiry case for a node that stays connected for weeks. It passes an empty
    // server list because it has nothing newer to say about trust — the CA
    // comparison is driven by messages, and this timer only ever asks "am I
    // about to expire?".
    let cert_check = tokio::spawn(async move {
        let mut interval = tokio::time::interval(CERT_CHECK_INTERVAL);
        interval.tick().await; // the connect path already checked
        loop {
            interval.tick().await;
            maybe_renew(&[]).await;
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
            ControlToNode::UpdateAgent => {
                // Asked directly, so no version comparison: somebody pressed a
                // button and meant it.
                match crate::selfupdate::run("asked by the control plane").await {
                    Ok(()) => {}
                    Err(e) => tracing::warn!(error = %e, "cannot update this agent"),
                }
            }
            ControlToNode::Ping => {
                out_tx.send(NodeToControl::Pong).await.ok();
            }
            ControlToNode::TrustChanged { ca_fingerprints } => {
                // A CA was staged. Renew now so the operator can promote it
                // without waiting up to thirty days for this node's
                // certificate to expire on its own.
                maybe_renew(&ca_fingerprints).await;
            }
            ControlToNode::RegisterAck {
                node_name,
                expected_agent_version,
                ca_fingerprints,
                ..
            } => {
                tracing::info!(node = %node_name, "registered");

                // Every reconnect is a chance to notice both a staged rotation
                // and an approaching expiry, without waiting for the timer.
                maybe_renew(&ca_fingerprints).await;

                // Nothing polls. A node reconnects whenever the control plane
                // restarts, which is exactly when a fleet needs updating — so
                // a deploy carries the news without anyone asking for it.
                let expected = expected_agent_version.as_deref();
                let behind = expected.is_some_and(|e| e != env!("CARGO_PKG_VERSION"));

                if crate::selfupdate::should_update(expected, cfg) {
                    tracing::info!(
                        expected = %expected.unwrap_or_default(),
                        running = env!("CARGO_PKG_VERSION"),
                        "control plane expects a different agent version"
                    );
                    if let Err(e) =
                        crate::selfupdate::run("version differs from the control plane").await
                    {
                        tracing::warn!(error = %e, "cannot update this agent");
                    }
                } else if behind {
                    // Being behind and saying nothing is the worst of the three
                    // outcomes. Somebody deploys, watches the fleet stay on the
                    // old version, and has no thread to pull — the node knew,
                    // decided not to act, and kept it to itself. Say which
                    // version, and say what would have to change.
                    tracing::warn!(
                        expected = %expected.unwrap_or_default(),
                        running = env!("CARGO_PKG_VERSION"),
                        why = %crate::selfupdate::refusal(cfg)
                            .unwrap_or_else(|| "unknown reason".into()),
                        "this agent is not the version the control plane expects, and will not update itself"
                    );
                }
            }
            ControlToNode::StartSession {
                session_id,
                runtime,
                workspace_path,
                cols,
                rows,
            } => {
                // An empty path is the control plane's signal for an ad-hoc
                // terminal: a shell with no workspace, run in this machine's
                // home directory.
                let cwd = if workspace_path.is_empty() {
                    std::env::var("HOME").unwrap_or_else(|_| "/".into())
                } else {
                    workspace_path
                };
                manager.start(session_id, &runtime, &cwd, cols, rows)
            }
            ControlToNode::StartAuthSession {
                session_id,
                runtime,
                cols,
                rows,
            } => {
                // The node picks the allowlisted login command for `runtime`;
                // an unknown runtime fails the session rather than running
                // anything (MAIN-126).
                manager.start_auth(session_id, &runtime, cols, rows)
            }
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
                    let snap = discovery::git_status(&workspace_path);
                    let _ = tx.blocking_send(NodeToControl::GitStatusResult {
                        request_id,
                        is_repo: snap.is_repo,
                        branch: snap.branch,
                        files: snap.files,
                        diff: snap.diff,
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
                    .unwrap_or_else(|| crate::config::default_workspace_root(&cfg.server));
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
                paths,
            } => {
                let tx = out_tx.clone();
                let roots = cfg.workspace_roots.clone();
                tokio::task::spawn_blocking(move || {
                    let outcome =
                        crate::gitops::commit_paths(&checkout_path, &message, paths.as_deref());
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
                    .unwrap_or_else(|| crate::config::default_workspace_root(&cfg.server));
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
            ControlToNode::InstallSkill {
                name,
                content,
                sha256,
            } => {
                // Reported, never fatal. A machine where one agent's skills
                // directory is unwritable is still a machine that should keep
                // running sessions — and the operator needs to be told which
                // one it was, not have the node disappear.
                let report = match crate::wizard::skills::install_taught(&name, &content) {
                    Ok(i) => {
                        tracing::info!(
                            skill = %name, sha = %&sha256[..sha256.len().min(8)],
                            agents = i.agents.len(), "learned a skill"
                        );
                        NodeToControl::SkillInstalled {
                            name,
                            agents: i.agents,
                            paths: i.paths,
                            error: None,
                        }
                    }
                    Err(e) => {
                        tracing::warn!(skill = %name, error = %e, "cannot install skill");
                        NodeToControl::SkillInstalled {
                            name,
                            agents: vec![],
                            paths: vec![],
                            error: Some(e.to_string()),
                        }
                    }
                };
                out_tx.send(report).await.ok();
            }
            ControlToNode::InstallRuntimeCredential {
                runtime,
                payload_b64,
            } => {
                // Reported, never fatal (AC-5), like every other install push:
                // a machine whose credential directory is unwritable should
                // keep running sessions, and the operator needs to be told
                // which runtime it was.
                //
                // The payload is decoded here and nowhere else in this node —
                // it goes straight from the frame to the file, and is not
                // logged, because a credential in a log is a credential on
                // disk somewhere nobody chose (AC-4).
                use base64::Engine as _;
                let report = match base64::engine::general_purpose::STANDARD
                    .decode(payload_b64.as_bytes())
                    .map_err(|e| {
                        anyhow::anyhow!("the credential payload was not valid base64: {e}")
                    })
                    .and_then(|payload| crate::runtime_auth::install_credential(&runtime, &payload))
                {
                    Ok(path) => {
                        // A write that the runtime does not accept is a FAILED
                        // delivery. Reporting success on the write alone would
                        // tell an operator the fleet is authorized when it is
                        // not, which is the one wrong answer here.
                        if crate::runtime_auth::is_authorized(&runtime) {
                            tracing::info!(
                                %runtime, path = %path.display(),
                                "installed a runtime credential"
                            );
                            NodeToControl::RuntimeCredentialInstalled {
                                runtime: runtime.clone(),
                                path: path.display().to_string(),
                                error: None,
                            }
                        } else {
                            tracing::warn!(
                                %runtime, path = %path.display(),
                                "credential installed but the runtime still reports not authorized"
                            );
                            NodeToControl::RuntimeCredentialInstalled {
                                runtime: runtime.clone(),
                                path: path.display().to_string(),
                                error: Some(
                                    "the credential was written but the runtime still reports \
                                     not authorized"
                                        .into(),
                                ),
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(%runtime, error = %e, "cannot install runtime credential");
                        NodeToControl::RuntimeCredentialInstalled {
                            runtime: runtime.clone(),
                            path: String::new(),
                            error: Some(e.to_string()),
                        }
                    }
                };
                out_tx.send(report).await.ok();

                // Re-probe and push the fresh set either way (AC-3), the same
                // path an authorize session uses when it ends. On success this
                // is what flips the panel to authorized; on failure it is what
                // stops the panel claiming a state the node does not have.
                let profiles = crate::runtime_auth::probe_all();
                out_tx
                    .send(NodeToControl::RuntimeAuthStatus { profiles })
                    .await
                    .ok();
            }
            ControlToNode::InstallHooks { content, sha256 } => {
                // Reported, never fatal (AC-2). A machine whose settings.json is
                // hand-broken should keep running sessions — the operator is
                // told the merge failed, the node does not disappear.
                let report = match crate::wizard::hooks::apply_pushed(&content) {
                    Ok(a) => {
                        tracing::info!(
                            sha = %&sha256[..sha256.len().min(8)],
                            wrote = a.wrote, path = %a.path.display(),
                            "applied managed hooks"
                        );
                        NodeToControl::HooksInstalled {
                            path: a.path.display().to_string(),
                            error: None,
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "cannot apply managed hooks");
                        NodeToControl::HooksInstalled {
                            path: String::new(),
                            error: Some(e.to_string()),
                        }
                    }
                };
                out_tx.send(report).await.ok();
            }
            ControlToNode::RunLoopJob {
                job_id,
                kind,
                target_task_key,
                repo_url,
                branch,
                seed,
            } => {
                // git + tmux + PTY are all blocking, so the runner lives on a
                // blocking thread with its own cloned sender and config —
                // mirroring the git-op arms above.
                let tx = out_tx.clone();
                let cfg = cfg.clone();
                tokio::task::spawn_blocking(move || {
                    crate::loop_job::run(
                        cfg,
                        tx,
                        crate::loop_job::LoopJob {
                            job_id,
                            kind,
                            target_task_key,
                            repo_url,
                            branch,
                            seed,
                        },
                    );
                });
            }
            ControlToNode::JobMessage { job_id, body } => {
                // MAIN-231: a human steered a run mid-flight. Type it into the
                // job's live session. tmux is blocking, so this goes on a
                // blocking thread like the runner itself; a message that finds
                // no session is reported back on the transcript, so the human
                // never reads "sent" as "the agent saw it".
                let tx = out_tx.clone();
                tokio::task::spawn_blocking(move || {
                    if let Err(e) = crate::loop_job::deliver_message(&job_id, &body) {
                        tracing::warn!(job = %job_id, error = %e, "could not deliver job message");
                        let _ = tx.blocking_send(NodeToControl::JobTranscript {
                            job_id,
                            source: "system".into(),
                            content: format!("message not delivered to the run: {e}"),
                        });
                    }
                });
            }
            ControlToNode::ForgetSkill { name } => {
                match crate::wizard::skills::forget_taught(&name) {
                    Ok(paths) => {
                        tracing::info!(skill = %name, removed = paths.len(), "forgot a skill")
                    }
                    Err(e) => tracing::warn!(skill = %name, error = %e, "cannot forget skill"),
                }
            }
            ControlToNode::InteractionAnswer { request_id, answer } => {
                // MAIN-159: a human answered an interaction this node raised. The waiting
                // `nook interactions ask --wait` process pulls the answer over REST; this
                // push is logged here as the delivery hook a future in-session bridge
                // (MAIN-162) builds on — this slice does not forward it into the PTY (NG).
                tracing::info!(%request_id, answer_len = answer.len(), "interaction answered (pushed)");
            }
        }
    }

    writer.abort();
    cert_check.abort();
    heartbeat.abort();
    discovery_task.abort();
    Ok(())
}

// tokio-tungstenite re-exports http via tungstenite.
mod axum_http {
    pub use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
}

/// Renew if the policy says to, and log why.
///
/// Never fatal: a node that cannot renew right now keeps running on the
/// certificate it has and tries again on the next check. Failing the connection
/// over it would turn a recoverable problem — control plane restarting — into
/// an outage.
async fn maybe_renew(server_fingerprints: &[String]) {
    let held = crate::enroll::held_ca_fingerprints();
    // A node authenticating by token has no certificate to renew. It is not
    // broken; it simply has not enrolled.
    if held.is_empty() && crate::config::load_identity().is_none() {
        return;
    }

    let Some(reason) = crate::certs::should_renew(
        chrono::Utc::now(),
        crate::enroll::expiry(),
        server_fingerprints,
        &held,
    ) else {
        return;
    };

    tracing::info!(reason = reason.why(), "renewing this machine's certificate");
    match crate::enroll::renew_now().await {
        Ok(not_after) => {
            tracing::info!(%not_after, "certificate renewed")
        }
        // Worth a warning rather than an error: the next check retries, and the
        // seven-day window exists precisely so a few failures do not matter.
        Err(e) => tracing::warn!(error = %e, "could not renew — will try again"),
    }
}
