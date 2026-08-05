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

/// Can this machine actually take that port right now?
///
/// Bind and immediately drop. There IS a race between the drop and the app's
/// own bind, and it is the right trade: the alternative is holding the socket
/// and handing a live fd through tmux to an arbitrary runtime. What this
/// catches is the case that actually happens — something has been sitting on
/// the port for minutes — not a microsecond-wide dead heat.
///
/// `0.0.0.0` rather than a loopback probe: a listener on 127.0.0.1 blocks a
/// later wildcard bind, so checking the wildcard is what answers "will the app
/// get this", and checking loopback alone would call an occupied port free.
fn port_is_free(port: i32) -> bool {
    let Ok(p) = u16::try_from(port) else {
        return false;
    };
    std::net::TcpListener::bind(("0.0.0.0", p)).is_ok()
}
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

/// Where a NEW checkout is written — always a tenant-scoped root, never
/// `workspace_roots[0]` (MAIN-363).
///
/// `workspace_roots` is the SCAN list, and on a node that enrolled before
/// MAIN-347 the first entry is the old flat `~/.nook/workspace`. Cloning there
/// puts the repo at `<owner>/<repo>` with nothing tenant-specific in the path,
/// so two tenants whose repos share an owner and a name land in one directory —
/// and cross-tenant placement (MAIN-353) makes that ordinary rather than exotic.
/// Reading the legacy roots continues, so checkouts already on disk are still
/// discovered where they are; only writing is forced canonical, which is what
/// stops new collisions without moving anything that exists.
///
/// `requested` is the slug the control plane sent for the tenant this checkout
/// is FOR, and it wins: the node's own `tenant_slug` is its HOME tenant, which
/// on a cross-tenant placement is the wrong tree, and on a node that enrolled
/// before MAIN-347 is absent entirely (leaving the control-plane host slug as
/// the last resort — a tenant-shaped path with no tenant in it).
///
/// Takes the slugs rather than the config so that precedence is testable
/// without standing up a `NodeConfig`.
fn checkout_root(requested: Option<&str>, own: Option<&str>, server: &str) -> String {
    crate::config::default_workspace_root(requested.or(own), server)
}

fn new_checkout_root(cfg: &NodeConfig, requested: Option<&str>) -> String {
    checkout_root(requested, cfg.tenant_slug.as_deref(), &cfg.server)
}

/// Make `root` a scan root, persisting it, unless it already is one.
///
/// The clone destination and the SCAN list have to agree, and MAIN-363's
/// tenant-scoping broke that on any node enrolled before MAIN-347. Those nodes
/// scan the flat `~/.nook/workspace`, and `discovery::scan` walks exactly two
/// levels — enough for `<owner>/<repo>`, one short of
/// `<tenant>/<owner>/<repo>`. So the clone landed somewhere discovery could not
/// see: the checkout was reported missing while sitting on disk, and the
/// reconciler kept starting sessions against a path it believed in.
///
/// Registering the tenant root instead of deepening the walk is the fix that
/// matches MAIN-347's model — a root IS tenant-scoped, and `<owner>/<repo>`
/// below it is exactly two levels again. Deepening the walk would also start
/// finding vendored repos nested inside real checkouts.
fn ensure_scan_root(cfg: &NodeConfig, root: &str) -> Vec<String> {
    let mut roots = cfg.workspace_roots.clone();
    let expanded = crate::config::expand_path(root);
    if roots
        .iter()
        .any(|r| crate::config::expand_path(r) == expanded)
    {
        return roots;
    }
    roots.push(root.to_string());
    let mut next = cfg.clone();
    next.workspace_roots = roots.clone();
    match next.save() {
        Ok(()) => tracing::info!(%root, "registered a new workspace scan root"),
        // In-memory still wins for this process, so the scan below is correct
        // even when the write is not possible.
        Err(e) => tracing::warn!(%root, error = %e, "could not persist the new scan root"),
    }
    roots
}

/// Download and install a new agent, OFF the read loop (MAIN-371).
///
/// The read loop is the only thing that delivers a keystroke to a PTY, so a
/// download awaited there freezes every terminal on the machine for as long as
/// it takes — and `RegisterAck` carries this on every single reconnect, which
/// is exactly when a deploy has just made the download large. The flag is not
/// belt-and-braces: `UpdateAgent` and `RegisterAck` can both fire within a
/// reconnect, and two installers unpacking over the same binary is worse than
/// either of them alone.
fn spawn_selfupdate(
    reason: &'static str,
    updating: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering;
    if updating.swap(true, Ordering::SeqCst) {
        tracing::debug!(reason, "an agent update is already running");
        return;
    }
    let updating = updating.clone();
    tokio::spawn(async move {
        if let Err(e) = crate::selfupdate::run(reason).await {
            tracing::warn!(error = %e, "cannot update this agent");
        }
        updating.store(false, Ordering::SeqCst);
    });
}

/// The next frame to write, control lane first (MAIN-371).
///
/// A busy session can hold a thousand output frames — several megabytes — in
/// `out` at once. On ONE queue a `GitStatusResult` or an `OpResult` behind them
/// arrives megabytes late, and the control plane gives up after ten seconds
/// with "node did not answer in time" while the answer sits in this buffer.
/// Control replies are small, rare and latency-critical; terminal output is
/// bulky, constant, and tolerant of a few milliseconds. Two queues and a bias
/// is the whole fix.
///
/// Starving `out` is not a risk worth guarding against: the control lane is
/// heartbeats and answers to questions somebody asked, never a stream.
///
/// None when BOTH lanes are closed, which is the connection ending.
async fn next_outbound(
    ctl: &mut mpsc::Receiver<NodeToControl>,
    out: &mut mpsc::Receiver<NodeToControl>,
) -> Option<NodeToControl> {
    tokio::select! {
        biased;
        Some(msg) = ctl.recv() => Some(msg),
        Some(msg) = out.recv() => Some(msg),
        else => None,
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

    // Two lanes to the control plane, not one (MAIN-371). `out_tx` carries
    // terminal output and nothing else; `ctl_tx` carries everything a human or
    // an API call is waiting on. See the writer below for why.
    let (out_tx, mut out_rx) = mpsc::channel::<NodeToControl>(1024);
    let (ctl_tx, mut ctl_rx) = mpsc::channel::<NodeToControl>(256);

    // Writer: everything → socket.
    let writer = tokio::spawn(async move {
        while let Some(msg) = next_outbound(&mut ctl_rx, &mut out_rx).await {
            let Ok(json) = serde_json::to_string(&msg) else {
                continue;
            };
            if sink.send(Message::Text(json.into())).await.is_err() {
                break;
            }
        }
    });

    // Register: idempotent full resync on every connect.
    ctl_tx
        .send(NodeToControl::Register {
            capabilities: capabilities::detect(),
            live_tmux_sessions: tmux::list_nook_sessions(),
        })
        .await
        .ok();
    {
        // Off the loop: a full scan is one `git status` per checkout, which on
        // a bind-mounted filesystem can take seconds.
        let tx = ctl_tx.clone();
        let roots = cfg.workspace_roots.clone();
        tokio::task::spawn_blocking(move || {
            let _ = tx.blocking_send(NodeToControl::WorkspacesDiscovered {
                workspaces: discovery::scan(&roots),
            });
        });
    }

    // Best-effort: on every (re)connect, prune loop-job worktrees orphaned by a
    // crash or a node restart (MAIN-161 AC-4). Blocking git, non-fatal.
    {
        let cfg = cfg.clone();
        tokio::task::spawn_blocking(move || crate::loop_job::reconcile(&cfg));
    }

    // Heartbeat carries a live resource sample so triage/humans can see which
    // machine can take the work.
    let hb_tx = ctl_tx.clone();
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
    let disc_tx = ctl_tx.clone();
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

    // The session engine lives on its own thread (see sessions.rs) — the read
    // loop only ever forwards commands, so a slow tmux spawn can never sit
    // between a keystroke arriving and it reaching the PTY.
    let session_tx = sessions::Manager::spawn(out_tx.clone(), ctl_tx.clone());

    let updating = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

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
                spawn_selfupdate("asked by the control plane", &updating);
            }
            ControlToNode::Ping => {
                ctl_tx.send(NodeToControl::Pong).await.ok();
            }
            ControlToNode::TrustChanged { ca_fingerprints } => {
                // A CA was staged. Renew now so the operator can promote it
                // without waiting up to thirty days for this node's
                // certificate to expire on its own.
                tokio::spawn(async move { maybe_renew(&ca_fingerprints).await });
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
                tokio::spawn(async move { maybe_renew(&ca_fingerprints).await });

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
                    spawn_selfupdate("version differs from the control plane", &updating);
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
                workspace_id,
                tenant_id,
                cols,
                rows,
                ports,
                unsatisfied,
                attempt,
                managed_purpose,
            } => {
                // THE authoritative check (MAIN-301 follow-on). Everything
                // upstream of here is a belief: the range is a promise that
                // nothing else listens in it, the exclusion list is what an
                // operator already knew, and neither can see a container that
                // came up thirty seconds ago. Only bind() knows, and only here.
                //
                // Checked BEFORE tmux exists, so a clash costs a re-lease
                // rather than a session whose app dies on start with an
                // EADDRINUSE nobody reads.
                let taken: Vec<i32> = ports
                    .iter()
                    .filter(|p| !port_is_free(p.port))
                    .map(|p| p.port)
                    .collect();
                if !taken.is_empty() {
                    tracing::warn!(
                        %session_id,
                        ?taken,
                        attempt,
                        "leased ports are already in use here — not starting, asking for others"
                    );
                    let _ = ctl_tx
                        .send(NodeToControl::PortsUnavailable {
                            session_id,
                            ports: taken,
                            attempt,
                        })
                        .await;
                } else {
                    // An empty path is the control plane's signal for an ad-hoc
                    // terminal: a shell with no workspace, run in this machine's
                    // home directory.
                    let cwd = if workspace_path.is_empty() {
                        std::env::var("HOME").unwrap_or_else(|_| "/".into())
                    } else {
                        workspace_path
                    };
                    let _ = session_tx.send(sessions::Cmd::Start {
                        workspace_id: workspace_id.map(|w| w.0.to_string()),
                        tenant_id: tenant_id.map(|t| t.0.to_string()),
                        session_id,
                        runtime,
                        cwd,
                        cols,
                        rows,
                        ports,
                        unsatisfied,
                        managed_purpose,
                    });
                }
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
                let _ = session_tx.send(sessions::Cmd::StartAuth {
                    session_id,
                    runtime,
                    cols,
                    rows,
                });
            }
            ControlToNode::AttachSession {
                session_id,
                tmux_session,
            } => {
                let _ = session_tx.send(sessions::Cmd::Attach {
                    session_id,
                    tmux_session,
                });
            }
            ControlToNode::SessionInput {
                session_id,
                data_b64,
            } => {
                let _ = session_tx.send(sessions::Cmd::Input {
                    session_id,
                    data_b64,
                });
            }
            ControlToNode::ResizeSession {
                session_id,
                cols,
                rows,
            } => {
                let _ = session_tx.send(sessions::Cmd::Resize {
                    session_id,
                    cols,
                    rows,
                });
            }
            ControlToNode::KillSession { session_id } => {
                let _ = session_tx.send(sessions::Cmd::Kill { session_id });
            }
            ControlToNode::DetachSession { session_id } => {
                let _ = session_tx.send(sessions::Cmd::Detach { session_id });
            }
            // Proxy one HTTP request to a local port and STREAM the answer back
            // (MAIN-402 AC-2).
            //
            // Spawned, never awaited here: this is the socket's read loop, and
            // awaiting a whole HTTP exchange in it would freeze every terminal
            // on this machine until the upstream answered — the failure mode
            // MAIN-362 spent a card removing.
            ControlToNode::TunnelRequest {
                version,
                request_id,
                port,
                method,
                path,
                headers,
                body_b64,
            } => {
                let tx = ctl_tx.clone();
                tokio::spawn(async move {
                    use base64::Engine;
                    let fail = |m: String| NodeToControl::TunnelFailed {
                        request_id,
                        message: m,
                    };

                    // A KNOWN frame carrying a meaning this build does not
                    // share. Refusing by name is the "degrade rather than
                    // misparse" half of AC-1 — obeying it would be worse than
                    // not understanding it.
                    if version > nook_proto::TUNNEL_PROTOCOL_VERSION {
                        let _ = tx
                            .send(fail(format!(
                                "tunnel frame v{version} is newer than this node's \
                                 v{}; upgrade the node",
                                nook_proto::TUNNEL_PROTOCOL_VERSION
                            )))
                            .await;
                        return;
                    }

                    let body = match base64::engine::general_purpose::STANDARD.decode(&body_b64) {
                        Ok(b) => b,
                        Err(e) => {
                            let _ = tx.send(fail(format!("undecodable body: {e}"))).await;
                            return;
                        }
                    };

                    // Loopback only. A tunnel exists to reach something running
                    // on THIS machine; letting the URL name any host would make
                    // every node an open proxy into its own network.
                    let url = format!("http://127.0.0.1:{port}{path}");
                    let method = match reqwest::Method::from_bytes(method.as_bytes()) {
                        Ok(m) => m,
                        Err(_) => {
                            let _ = tx.send(fail(format!("bad method: {method}"))).await;
                            return;
                        }
                    };
                    let client = reqwest::Client::new();
                    let mut req = client.request(method, &url).body(body);
                    for (k, v) in &headers {
                        req = req.header(k, v);
                    }
                    let mut resp = match req.send().await {
                        Ok(r) => r,
                        Err(e) => {
                            let _ = tx.send(fail(format!("upstream on port {port}: {e}"))).await;
                            return;
                        }
                    };

                    let status = resp.status().as_u16();
                    let out_headers: Vec<(String, String)> = resp
                        .headers()
                        .iter()
                        .map(|(k, v)| {
                            (
                                k.as_str().to_string(),
                                v.to_str().unwrap_or_default().to_string(),
                            )
                        })
                        .collect();
                    if tx
                        .send(NodeToControl::TunnelResponse {
                            request_id,
                            version: nook_proto::TUNNEL_PROTOCOL_VERSION,
                            status,
                            headers: out_headers,
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }

                    // The body, a chunk at a time. `chunk()` rather than
                    // reading to end: a tunnel that buffers the response is a
                    // memory bug waiting for somebody to curl a large file
                    // through it, which is the whole of AC-2.
                    let mut seq = 0u64;
                    loop {
                        match resp.chunk().await {
                            Ok(Some(bytes)) => {
                                let frame = NodeToControl::TunnelChunk {
                                    request_id,
                                    seq,
                                    data_b64: base64::engine::general_purpose::STANDARD
                                        .encode(&bytes),
                                    last: false,
                                };
                                seq += 1;
                                if tx.send(frame).await.is_err() {
                                    return;
                                }
                            }
                            Ok(None) => break,
                            Err(e) => {
                                // Mid-stream death. The receiver has already
                                // seen a status, so this is the only way to
                                // tell it the body is short.
                                let _ = tx.send(fail(format!("upstream ended early: {e}"))).await;
                                return;
                            }
                        }
                    }
                    // Explicit end. A stream that finishes by going quiet is
                    // indistinguishable from one that died.
                    let _ = tx
                        .send(NodeToControl::TunnelChunk {
                            request_id,
                            seq,
                            data_b64: String::new(),
                            last: true,
                        })
                        .await;
                });
            }
            ControlToNode::GetGitStatus {
                request_id,
                workspace_path,
            } => {
                let tx = ctl_tx.clone();
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
                tenant_slug,
            } => {
                let tx = ctl_tx.clone();
                let root = new_checkout_root(cfg, tenant_slug.as_deref());
                // Registered BEFORE the clone, so the rescan that follows can
                // actually see what we are about to write there.
                let roots = ensure_scan_root(cfg, &root);
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
                let tx = ctl_tx.clone();
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
                let tx = ctl_tx.clone();
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
                let tx = ctl_tx.clone();
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
                let tx = ctl_tx.clone();
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
                let tx = ctl_tx.clone();
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
                let tx = ctl_tx.clone();
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
                let tx = ctl_tx.clone();
                // No tenant on the wire for this one yet, so it uses the node's
                // own — correct for the ordinary case, and still better than
                // `workspace_roots[0]`.
                let root = new_checkout_root(cfg, None);
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
                let tx = ctl_tx.clone();
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
                let tx = ctl_tx.clone();
                tokio::task::spawn_blocking(move || {
                    use base64::Engine;
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(content_b64.as_bytes())
                        .unwrap_or_default();
                    if let Err(e) =
                        crate::gitops::write_workspace_file(&checkout_path, &name, &bytes)
                    {
                        let _ = tx.blocking_send(NodeToControl::Error {
                            context: "write_workspace_file".into(),
                            message: e,
                        });
                    } else {
                        tracing::info!(checkout = %checkout_path, file = %name, "workspace file synced");
                    }
                });
            }
            ControlToNode::ReadWorkspaceFile {
                request_id,
                checkout_path,
                name,
            } => {
                let tx = ctl_tx.clone();
                tokio::task::spawn_blocking(move || {
                    use base64::Engine;
                    let (ok, message) =
                        match crate::gitops::read_workspace_file(&checkout_path, &name) {
                            Ok(bytes) => (
                                true,
                                base64::engine::general_purpose::STANDARD.encode(&bytes),
                            ),
                            Err(e) => (false, e),
                        };
                    let _ = tx.blocking_send(NodeToControl::OpResult {
                        request_id,
                        ok,
                        path: Some(checkout_path),
                        message,
                    });
                });
            }
            ControlToNode::RescanWorkspaces => {
                // spawn_blocking like every other git arm — this one ran the
                // scan INLINE and stalled the read loop (frozen keystrokes)
                // for one `git status` per checkout.
                let tx = ctl_tx.clone();
                let roots = cfg.workspace_roots.clone();
                tokio::task::spawn_blocking(move || {
                    let _ = tx.blocking_send(NodeToControl::WorkspacesDiscovered {
                        workspaces: discovery::scan(&roots),
                    });
                });
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
                ctl_tx.send(report).await.ok();
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
                ctl_tx.send(report).await.ok();

                // Re-probe and push the fresh set either way (AC-3), the same
                // path an authorize session uses when it ends. On success this
                // is what flips the panel to authorized; on failure it is what
                // stops the panel claiming a state the node does not have.
                let profiles = crate::runtime_auth::probe_all();
                ctl_tx
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
                ctl_tx.send(report).await.ok();
            }
            ControlToNode::RunLoopJob {
                workspace_id,
                ssh_key,
                nook_token,
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
                let tx = ctl_tx.clone();
                let cfg = cfg.clone();
                tokio::task::spawn_blocking(move || {
                    crate::loop_job::run(
                        cfg,
                        tx,
                        crate::loop_job::LoopJob {
                            workspace_id: workspace_id.map(|w| w.0.to_string()),
                            ssh_key,
                            nook_token,
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
                let tx = ctl_tx.clone();
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

#[cfg(test)]
mod tests {
    use super::{checkout_root, next_outbound};
    use nook_proto::NodeToControl;
    use nook_types::SessionId;
    use tokio::sync::mpsc;

    const SERVER: &str = "https://nook.hein.network";

    /// A git answer must not wait behind a terminal that is mid-`cat`.
    ///
    /// The bug this pins: one queue for both, a session flooding it, and the
    /// control plane timing out at ten seconds on a reply the node had already
    /// produced — "node did not answer in time", every terminal on the machine
    /// frozen with it.
    #[tokio::test]
    async fn control_replies_overtake_a_backlog_of_terminal_output() {
        let (out_tx, mut out_rx) = mpsc::channel::<NodeToControl>(1024);
        let (ctl_tx, mut ctl_rx) = mpsc::channel::<NodeToControl>(256);

        for _ in 0..1024 {
            out_tx
                .try_send(NodeToControl::SessionOutput {
                    session_id: SessionId(uuid::Uuid::now_v7()),
                    data_b64: "x".repeat(5500),
                })
                .expect("output lane holds a full backlog");
        }
        ctl_tx
            .try_send(NodeToControl::Pong)
            .expect("control lane is empty");

        assert!(
            matches!(
                next_outbound(&mut ctl_rx, &mut out_rx).await,
                Some(NodeToControl::Pong)
            ),
            "the control lane must be drained first, not after 1024 output frames"
        );
    }

    /// Nothing is stranded on the bulk lane once control is quiet.
    #[tokio::test]
    async fn terminal_output_still_flows_when_control_is_idle() {
        let (out_tx, mut out_rx) = mpsc::channel::<NodeToControl>(8);
        let (_ctl_tx, mut ctl_rx) = mpsc::channel::<NodeToControl>(8);
        out_tx
            .try_send(NodeToControl::SessionOutput {
                session_id: SessionId(uuid::Uuid::now_v7()),
                data_b64: "y".into(),
            })
            .unwrap();
        assert!(matches!(
            next_outbound(&mut ctl_rx, &mut out_rx).await,
            Some(NodeToControl::SessionOutput { .. })
        ));
    }

    /// Both lanes closed is the connection ending — the writer must stop, not spin.
    #[tokio::test]
    async fn both_lanes_closed_ends_the_writer() {
        let (out_tx, mut out_rx) = mpsc::channel::<NodeToControl>(1);
        let (ctl_tx, mut ctl_rx) = mpsc::channel::<NodeToControl>(1);
        drop(out_tx);
        drop(ctl_tx);
        assert!(next_outbound(&mut ctl_rx, &mut out_rx).await.is_none());
    }

    /// The tenant that ASKED wins over the node's home tenant. Cross-tenant
    /// placement (MAIN-353) makes this the ordinary case, not the exception:
    /// a node homed in one tenant routinely clones for another, and the
    /// checkout belongs in the requesting tenant's tree.
    #[test]
    fn the_requesting_tenant_beats_the_nodes_own() {
        assert_eq!(
            checkout_root(Some("engineering-team"), Some("hein"), SERVER),
            "~/.nook/workspace/engineering-team"
        );
    }

    /// The prod case that exposed this: a node enrolled before MAIN-347 has no
    /// slug of its own at all, so without one on the wire the path falls back
    /// to the control-plane host — tenant-shaped with no tenant in it.
    #[test]
    fn a_node_with_no_slug_of_its_own_still_lands_in_the_right_tree() {
        assert_eq!(
            checkout_root(Some("engineering-team"), None, SERVER),
            "~/.nook/workspace/engineering-team"
        );
        assert_eq!(
            checkout_root(None, None, SERVER),
            "~/.nook/workspace/nook.hein.network",
            "no slug anywhere: the host is the last resort, not a bare root"
        );
    }

    /// An older control plane sends nothing, and the node keeps its previous
    /// behaviour rather than regressing to a rootless path.
    #[test]
    fn without_a_requested_slug_the_nodes_own_is_used() {
        assert_eq!(
            checkout_root(None, Some("hein"), SERVER),
            "~/.nook/workspace/hein"
        );
    }
}
