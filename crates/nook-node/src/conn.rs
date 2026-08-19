//! The node's single outbound connection to the control plane, with
//! jittered exponential backoff reconnect. No inbound ports, no SSH.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

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
/// How often a deferred agent update reconsiders whether the machine is quiet
/// (MAIN-505). Frequent enough that a finished run is followed promptly by the
/// restart it was blocking, cheap enough to be a lock and a set length.
const DRAIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

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
/// How often to tell the control plane which build worktrees and compose stacks
/// this node holds (MAIN-507, MAIN-537). Ten minutes: it costs one
/// `docker compose ls` and one directory listing, and the things it catches — a
/// stack whose card finished, a merged card's checkout — are measured in hours
/// of idle RAM and days of disk.
const INVENTORY_REPORT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(600);
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
///
/// `deferred_ctl` is set only for a DEFERRED update (MAIN-505), and it is what
/// keeps the cordon honest. This function is deliberately non-fatal — a failed
/// download or an unwritable binary logs and leaves the process running — and a
/// deferred update stays cordoned across the install, so that failure has to
/// LIFT the cordon and say so. Nothing else would: the drain tick has no work
/// left to do, and both other clear-paths ride a `RegisterAck` that only
/// arrives on connect, so the node would take no loop work until the socket
/// happened to drop.
fn spawn_selfupdate(
    reason: &'static str,
    updating: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    deferred_ctl: Option<mpsc::Sender<NodeToControl>>,
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
            // Only reached when the install failed: on success this process is
            // already gone.
            if let Some(tx) = deferred_ctl {
                if crate::cordon::install_failed() {
                    tracing::warn!(
                        "lifting the update cordon — the install failed and this node is still running"
                    );
                    tx.send(NodeToControl::CordonChanged { cordon: None })
                        .await
                        .ok();
                }
            }
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
            capabilities: Box::new(capabilities::detect()),
            live_tmux_sessions: tmux::list_nook_sessions(),
        })
        .await
        .ok();
    // ...and, immediately after it, this node's cordon — including the clear
    // `None` (MAIN-505). Unconditional because the ASSERTION is the point: a
    // node that restarted into the new agent holds nothing, and only saying so
    // clears the cordon its previous process left behind.
    ctl_tx
        .send(NodeToControl::CordonChanged {
            cordon: crate::cordon::current(),
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
    // crash or a node restart (MAIN-161 AC-4), and reclaim the job containers,
    // networks, volumes and firewall rules the same crash stranded (MAIN-617).
    // Blocking git and docker, non-fatal.
    //
    // Connect is where the crash case is actually caught: a node that has just
    // started is running no jobs, so every labelled object on the machine is by
    // definition an orphan.
    //
    // The build stacks left on the HOST daemon by runs that predate the sandbox
    // are collected here too (MAIN-630). Same occasion, different argument:
    // those are not a crash's leavings but an upgrade's, and every one of them
    // is holding host ports its card still leases — so the card's own next run
    // cannot publish them until it goes.
    {
        let cfg = cfg.clone();
        tokio::task::spawn_blocking(move || {
            crate::loop_job::reconcile(&cfg);
            crate::loop_job::sweep_job_sandboxes();
            crate::compose::reconcile_pre_sandbox_stacks();
        });
    }

    // What this node holds for finished cards, on a timer as well as on connect.
    //
    // Build worktrees are exempt from the sweep above and REPORTED instead:
    // whether one is still wanted is a card fact this process cannot see, so the
    // control plane answers with a `RemoveWorktree` for each it no longer
    // records (MAIN-480 AC-1) and for each whose card is over (MAIN-537 AC-4).
    // The compose stacks those worktrees boot are reported the same way
    // (MAIN-507 AC-5).
    //
    // **On a timer, and the worktree half is why this is one task rather than
    // two.** The stack report got the timer when it landed, with the reason
    // written on it: a node that stays up for days accumulates the leavings of
    // every card that finished in between, and only the control plane can say
    // which of them belong to a card that is over. That is exactly as true of
    // worktrees, which had only the connect-time report — so on a node that
    // never restarts, a merged card's tree was collected by nothing at all
    // (MAIN-537). One task reports both, so neither can be given a schedule the
    // other lacks again.
    let inventory_cfg = cfg.clone();
    let inventory_tx = ctl_tx.clone();
    let inventory_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(INVENTORY_REPORT_INTERVAL);
        loop {
            interval.tick().await; // the first tick is immediate: the connect-time report
            let cfg = inventory_cfg.clone();
            let held = tokio::task::spawn_blocking(move || {
                // The orphaned-sandbox sweep rides the same timer for the same
                // reason the reports do (MAIN-617 AC-4): a node that stays up
                // for days accumulates the leavings of every job that died in
                // between, and a container nobody removes holds a whole nested
                // image cache. No new timer — this one already runs every ten
                // minutes and already costs a `docker compose ls`.
                crate::loop_job::sweep_job_sandboxes();
                (
                    crate::loop_job::build_worktrees_held(&cfg),
                    crate::compose::build_stacks_held(),
                )
            })
            .await
            .unwrap_or_default();
            if inventory_tx
                .send(NodeToControl::LoopWorktreesHeld { paths: held.0 })
                .await
                .is_err()
            {
                break;
            }
            if inventory_tx
                .send(NodeToControl::BuildStacksHeld { projects: held.1 })
                .await
                .is_err()
            {
                break;
            }
        }
    });

    let updating = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    // A deferred agent update, reconsidered on a timer (MAIN-505). Per
    // connection, and aborted with the rest below, so a reconnect replaces this
    // watcher rather than adding a second one — the deferral itself is process
    // state and survives both.
    let drain_tx = ctl_tx.clone();
    let drain_updating = updating.clone();
    let drain = tokio::spawn(async move {
        let mut interval = tokio::time::interval(DRAIN_INTERVAL);
        loop {
            interval.tick().await;
            match crate::cordon::tick(crate::loop_job::in_flight()) {
                crate::cordon::Tick::Idle => {}
                crate::cordon::Tick::Waiting { cordon, changed } => {
                    if changed {
                        if cordon.overdue {
                            tracing::warn!(reason = %cordon.reason, "agent update still blocked");
                        } else {
                            tracing::info!(reason = %cordon.reason, "agent update deferred");
                        }
                        drain_tx
                            .send(NodeToControl::CordonChanged {
                                cordon: Some(cordon),
                            })
                            .await
                            .ok();
                    }
                }
                crate::cordon::Tick::Proceed { cordon } => {
                    tracing::info!("loop jobs finished — installing the deferred agent update");
                    // Reported before the install rather than after: the node
                    // is still cordoned (nothing may start while the process is
                    // about to exit), and this is what the surfaces show if the
                    // install takes a while.
                    drain_tx
                        .send(NodeToControl::CordonChanged {
                            cordon: Some(cordon),
                        })
                        .await
                        .ok();
                    spawn_selfupdate(
                        "deferred update — loop jobs finished",
                        &drain_updating,
                        Some(drain_tx.clone()),
                    );
                }
            }
        }
    });

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

    // Tunnelled WebSockets this node is holding open, by request id (MAIN-10).
    // Scoped to the CONNECTION: the control plane closes every tunnel on a
    // node it loses (MAIN-404 AC-4), so a socket that outlived the reconnect
    // would be an upstream connection nothing on either side still refers to.
    // Dropping this table drops the senders, and every pump ends.
    let ws_tunnels: Arc<Mutex<HashMap<uuid::Uuid, mpsc::Sender<ControlToNode>>>> =
        Arc::new(Mutex::new(HashMap::new()));

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
                spawn_selfupdate("asked by the control plane", &updating, None);
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

                // A deploy rolled back to what we already run: whatever we were
                // holding work back for is no longer wanted (MAIN-505). Said
                // here rather than left to the drain tick, because a deferral
                // nothing ever cancels is a node that quietly goes dark.
                if !behind && crate::cordon::current().is_some() {
                    tracing::info!("control plane expects the version we run — lifting the cordon");
                    crate::cordon::clear();
                    ctl_tx
                        .send(NodeToControl::CordonChanged { cordon: None })
                        .await
                        .ok();
                }

                if crate::selfupdate::should_update(expected, cfg) {
                    tracing::info!(
                        expected = %expected.unwrap_or_default(),
                        running = env!("CARGO_PKG_VERSION"),
                        "control plane expects a different agent version"
                    );
                    // Updating ends this process, and for a streaming loop job
                    // this process IS the buffer (MAIN-505 / MAIN-240). So a
                    // busy node cordons and waits instead; the drain tick above
                    // installs it the moment the last run concludes.
                    let in_flight = crate::loop_job::in_flight();
                    if in_flight > 0 {
                        let cordon =
                            crate::cordon::defer_update(expected.unwrap_or_default(), in_flight);
                        tracing::warn!(
                            reason = %cordon.reason,
                            "deferring the agent update — loop jobs are running here"
                        );
                        ctl_tx
                            .send(NodeToControl::CordonChanged {
                                cordon: Some(cordon),
                            })
                            .await
                            .ok();
                    } else {
                        spawn_selfupdate("version differs from the control plane", &updating, None);
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
                workspace_id,
                tenant_id,
                cols,
                rows,
                ports,
                unsatisfied,
                attempt,
                // The wire still carries both — the control plane records a
                // purpose on the row — but a session no longer does anything
                // different because of them (MAIN-455).
                managed_purpose: _,
                shard: _,
                interface,
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
                    // Terminal or chat (MAIN-502). The whole fork is here: the
                    // ports, the checkout and the ids are resolved identically
                    // either way, and only the machinery that runs in that
                    // directory differs. `cols`/`rows` are a PTY's, so a chat
                    // simply has none.
                    let _ = match interface {
                        nook_types::SessionInterface::Chat => {
                            session_tx.send(sessions::Cmd::StartChat {
                                workspace_id: workspace_id.map(|w| w.0.to_string()),
                                tenant_id: tenant_id.map(|t| t.0.to_string()),
                                session_id,
                                runtime,
                                cwd,
                                ports,
                                unsatisfied,
                            })
                        }
                        nook_types::SessionInterface::Terminal => {
                            session_tx.send(sessions::Cmd::Start {
                                workspace_id: workspace_id.map(|w| w.0.to_string()),
                                tenant_id: tenant_id.map(|t| t.0.to_string()),
                                session_id,
                                runtime,
                                cwd,
                                cols,
                                rows,
                                ports,
                                unsatisfied,
                            })
                        }
                    };
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
            ControlToNode::ChatMessage { session_id, text } => {
                let _ = session_tx.send(sessions::Cmd::ChatMessage { session_id, text });
            }
            ControlToNode::ChatPermissionDecision {
                session_id,
                request_id,
                allow,
                remember,
            } => {
                let _ = session_tx.send(sessions::Cmd::ChatPermission {
                    session_id,
                    request_id,
                    allow,
                    remember,
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
            // Dial a WebSocket on a local port and pump it both ways (MAIN-10
            // AC-1). Spawned for the same reason the HTTP proxy is: this is the
            // read loop, and a socket that lives for hours cannot live in it.
            ControlToNode::TunnelUpgrade {
                version,
                request_id,
                port,
                path,
                headers,
            } => {
                let tx = ctl_tx.clone();
                if version > nook_proto::TUNNEL_PROTOCOL_VERSION {
                    let _ = tx
                        .send(NodeToControl::TunnelFailed {
                            request_id,
                            message: format!(
                                "tunnel frame v{version} is newer than this node's \
                                 v{}; upgrade the node",
                                nook_proto::TUNNEL_PROTOCOL_VERSION
                            ),
                        })
                        .await;
                    continue;
                }
                // Bounded, like every other lane to a pump on this node. A
                // full one is not a frame to drop — dropping one corrupts the
                // stream — so `forward_ws` ends the socket instead.
                let (up_tx, up_rx) = mpsc::channel::<ControlToNode>(256);
                ws_tunnels.lock().unwrap().insert(request_id, up_tx);
                // WEAK, deliberately: the table is what ends every pump when
                // the connection drops, and a pump holding it strongly would
                // wait for a table waiting for the pump.
                let table = Arc::downgrade(&ws_tunnels);
                tokio::spawn(async move {
                    tunnel_ws(request_id, port, path, headers, up_rx, tx).await;
                    if let Some(table) = table.upgrade() {
                        table.lock().unwrap().remove(&request_id);
                    }
                });
            }
            frame @ (ControlToNode::TunnelWsData { .. } | ControlToNode::TunnelWsClose { .. }) => {
                forward_ws(&ws_tunnels, frame);
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
            ControlToNode::ReapBuildStacks {
                request_id,
                projects,
            } => {
                let tx = ctl_tx.clone();
                tokio::task::spawn_blocking(move || {
                    let reaped = crate::compose::reap_projects(&projects);
                    let _ = tx.blocking_send(NodeToControl::OpResult {
                        request_id,
                        ok: reaped.ok,
                        // What actually came down, so the card can say so
                        // (AC-7); `None` when nothing was running.
                        path: (!reaped.projects.is_empty()).then(|| reaped.projects.join(", ")),
                        message: reaped.message,
                    });
                });
            }
            ControlToNode::RemoveWorktree {
                request_id,
                worktree_path,
                delete_branch,
            } => {
                let tx = ctl_tx.clone();
                let roots = cfg.workspace_roots.clone();
                tokio::task::spawn_blocking(move || {
                    // The stack goes first (MAIN-507 AC-3) — see the function.
                    let outcome = crate::compose::reap_then_remove_worktree(
                        &worktree_path,
                        if delete_branch {
                            crate::gitops::remove_build_worktree
                        } else {
                            crate::gitops::remove_worktree
                        },
                    );
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
            ControlToNode::InitProject {
                request_id,
                name,
                description,
            } => {
                let tx = ctl_tx.clone();
                // No tenant on the wire for this one yet, so it uses the node's
                // own — correct for the ordinary case, and still better than
                // `workspace_roots[0]`.
                let root = new_checkout_root(cfg, None);
                // Registered BEFORE the init, exactly as the clone arm above
                // does it and for the same reason (MAIN-363): the project lands
                // under the TENANT root, which on a node enrolled before that
                // card is not a scan root — so the rescan below walked
                // `~/.nook/workspace` and never saw it. Discovery then never
                // surfaced the workspace, and the flow that waits for it to
                // open a session waited forever (MAIN-619 AC-9).
                let roots = ensure_scan_root(cfg, &root);
                tokio::task::spawn_blocking(move || {
                    let outcome = crate::gitops::init_project(&root, &name, description.as_deref());
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
                review_pr_number,
                review_forced,
                gh_token,
                server_url,
                target_task_key,
                repo_url,
                branch,
                seed,
                ports,
                unsatisfied_ports,
            } => {
                // The cordon is enforced HERE, not only where work is placed
                // (MAIN-505 AC-2). The control plane's view of it is a push old,
                // and a run that lands in that window is one more thing the
                // deferral has to wait for — refused, the card goes straight
                // back instead.
                if let Some(cordon) = crate::cordon::current() {
                    ctl_tx
                        .send(NodeToControl::JobRefused {
                            job_id,
                            reason: format!(
                                "node {} is cordoned: {}",
                                cfg.node_name, cordon.reason
                            ),
                        })
                        .await
                        .ok();
                    continue;
                }
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
                            review_pr_number,
                            review_forced,
                            gh_token,
                            server_url,
                            target_task_key,
                            repo_url,
                            branch,
                            seed,
                            ports,
                            unsatisfied_ports,
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
    drain.abort();
    discovery_task.abort();
    inventory_task.abort();
    Ok(())
}

/// Hand a frame to the pump holding that socket, or end the socket.
///
/// A full lane is the case worth naming: a WebSocket frame is not a chunk of a
/// body, so skipping one does not shorten a stream, it corrupts it. Dropping
/// the table entry drops the last sender, and the pump closes both ends —
/// a socket that dies is recoverable, one that silently lies is not.
fn forward_ws(
    table: &Arc<Mutex<HashMap<uuid::Uuid, mpsc::Sender<ControlToNode>>>>,
    frame: ControlToNode,
) {
    let request_id = match &frame {
        ControlToNode::TunnelWsData { request_id, .. }
        | ControlToNode::TunnelWsClose { request_id, .. } => *request_id,
        _ => return,
    };
    let sender = table.lock().unwrap().get(&request_id).cloned();
    let Some(sender) = sender else { return };
    if sender.try_send(frame).is_err() {
        tracing::warn!(%request_id, "tunnelled socket is not keeping up — closing it");
        table.lock().unwrap().remove(&request_id);
    }
}

/// Headers of the visitor's handshake this node's own client owns.
///
/// The key and the version are answered by the upstream against OUR handshake,
/// not the visitor's, so forwarding theirs would make the accept-hash we verify
/// the wrong one. Extensions are refused for a blunter reason: an upstream that
/// selected `permessage-deflate` would frame its payloads in a way this client
/// does not read, and never offering it is what keeps AC-2's bytes intact.
const CLIENT_OWNED_HANDSHAKE: &[&str] = &[
    "sec-websocket-key",
    "sec-websocket-version",
    "sec-websocket-extensions",
    "connection",
    "upgrade",
];

/// Dial the upstream WebSocket and carry frames until one end stops (MAIN-10).
///
/// Loopback only, exactly like the HTTP proxy beside it: a tunnel reaches
/// something running on THIS machine, and letting the frame name a host would
/// make every node an open proxy into its own network.
async fn tunnel_ws(
    request_id: uuid::Uuid,
    port: u16,
    path: String,
    headers: Vec<nook_proto::TunnelHeader>,
    mut inbound: mpsc::Receiver<ControlToNode>,
    tx: mpsc::Sender<NodeToControl>,
) {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD;
    let fail = |message: String| NodeToControl::TunnelFailed {
        request_id,
        message,
    };

    let mut request = match format!("ws://127.0.0.1:{port}{path}").into_client_request() {
        Ok(r) => r,
        Err(e) => {
            let _ = tx.send(fail(format!("bad upgrade target: {e}"))).await;
            return;
        }
    };
    for (name, value) in &headers {
        if CLIENT_OWNED_HANDSHAKE
            .iter()
            .any(|owned| name.eq_ignore_ascii_case(owned))
        {
            continue;
        }
        let (Ok(name), Ok(value)) = (
            tokio_tungstenite::tungstenite::http::HeaderName::from_bytes(name.as_bytes()),
            tokio_tungstenite::tungstenite::http::HeaderValue::from_str(value),
        ) else {
            continue;
        };
        request.headers_mut().insert(name, value);
    }

    let (upstream, response) = match connect_async(request).await {
        Ok(pair) => pair,
        Err(e) => {
            let _ = tx
                .send(fail(format!("upstream websocket on port {port}: {e}")))
                .await;
            return;
        }
    };
    // Only what the control plane can act on: the subprotocol it must echo in
    // its own `101`. The rest of a handshake response describes THIS
    // connection, and none of it is true of the visitor's.
    let out_headers: Vec<nook_proto::TunnelHeader> = response
        .headers()
        .get_all("sec-websocket-protocol")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(|v| ("sec-websocket-protocol".to_string(), v.to_string()))
        .collect();
    if tx
        .send(NodeToControl::TunnelUpgraded {
            request_id,
            version: nook_proto::TUNNEL_PROTOCOL_VERSION,
            headers: out_headers,
        })
        .await
        .is_err()
    {
        return;
    }

    let (mut sink, mut stream) = upstream.split();
    loop {
        tokio::select! {
            frame = inbound.recv() => match frame {
                Some(ControlToNode::TunnelWsData { data_b64, binary, .. }) => {
                    let Ok(bytes) = b64.decode(&data_b64) else { break };
                    let message = if binary {
                        Message::Binary(bytes.into())
                    } else {
                        match String::from_utf8(bytes) {
                            Ok(text) => Message::Text(text.into()),
                            Err(_) => break,
                        }
                    };
                    if sink.send(message).await.is_err() {
                        break;
                    }
                }
                Some(ControlToNode::TunnelWsClose { .. }) | None => {
                    // The visitor closed, or the tunnel they rode on is gone
                    // (AC-4, AC-5). Either way this connection is released —
                    // which is the whole of "the node does not leave a socket
                    // held open".
                    let _ = sink.close().await;
                    return;
                }
                Some(_) => continue,
            },
            message = stream.next() => match message {
                Some(Ok(Message::Text(text))) => {
                    if tx.send(NodeToControl::TunnelWsData {
                        request_id,
                        data_b64: b64.encode(text.as_bytes()),
                        binary: false,
                    }).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Binary(bytes))) => {
                    if tx.send(NodeToControl::TunnelWsData {
                        request_id,
                        data_b64: b64.encode(&bytes),
                        binary: true,
                    }).await.is_err() {
                        break;
                    }
                }
                // tungstenite answers a ping itself; a raw frame is the
                // unmasked view of something already delivered above.
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => continue,
                Some(Ok(Message::Close(frame))) => {
                    let (code, reason) = match frame {
                        Some(f) => (Some(u16::from(f.code)), Some(f.reason.to_string())),
                        None => (None, None),
                    };
                    let _ = tx
                        .send(NodeToControl::TunnelWsClose { request_id, code, reason })
                        .await;
                    // Answer the closing handshake rather than just dropping
                    // the socket, so the app sees a close and not a reset.
                    let _ = sink.close().await;
                    return;
                }
                Some(Err(e)) => {
                    let _ = tx.send(fail(format!("upstream socket on port {port}: {e}"))).await;
                    return;
                }
                None => break,
            },
        }
    }
    // Fell out without the upstream saying so: tell the control plane the
    // socket is over, since a stream that ends by going quiet is
    // indistinguishable from one still waiting.
    let _ = tx
        .send(NodeToControl::TunnelWsClose {
            request_id,
            code: None,
            reason: None,
        })
        .await;
    let _ = sink.close().await;
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
    use super::{checkout_root, next_outbound, tunnel_ws};
    use nook_proto::{ControlToNode, NodeToControl};
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

    /// Every arm that WRITES a checkout must register where it writes as a scan
    /// root, or discovery walks a tree the file is not in (MAIN-363). Clone got
    /// this; `InitProject` did not, and the cost was invisible — the repo was
    /// created correctly and then never surfaced, so the flow waiting on
    /// discovery to open a session waited forever (MAIN-619 AC-9).
    ///
    /// Read off the source because the invariant is about the CALL, and both
    /// alternatives are worse: `ensure_scan_root` persists the config, so
    /// exercising it needs a config file, and the two arms are inside one
    /// several-hundred-line `match` in a connection loop that needs a live
    /// control plane to reach.
    #[test]
    fn every_arm_that_creates_a_checkout_registers_it_as_a_scan_root() {
        let src = include_str!("conn.rs");
        for arm in ["let root = new_checkout_root(cfg,"] {
            let mut from = 0;
            let mut seen = 0;
            while let Some(at) = src[from..].find(arm) {
                let start = from + at;
                let after = &src[start..(start + 700).min(src.len())];
                assert!(
                    after.contains("ensure_scan_root(cfg, &root)"),
                    "a checkout destination is computed without being registered \
                     as a scan root — discovery will not see what lands there:\n{}",
                    &after[..after.len().min(300)]
                );
                seen += 1;
                from = start + arm.len();
            }
            assert!(seen >= 2, "expected the clone and init arms, found {seen}");
        }
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

    /// A WebSocket echo server on a loopback port, for the node's half of a
    /// tunnelled upgrade. Returns the port and a handle that finishes when the
    /// one connection it accepts has closed — which is how AC-4's "the node
    /// releases the upstream connection" is actually observed rather than
    /// assumed.
    // The handshake callback's `Err` type is tungstenite's own HTTP response,
    // which is over clippy's threshold and not ours to box.
    #[allow(clippy::result_large_err)]
    async fn echo_upstream() -> (u16, tokio::task::JoinHandle<()>) {
        use futures_util::{SinkExt, StreamExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_hdr_async(
                stream,
                |req: &tokio_tungstenite::tungstenite::handshake::server::Request,
                 mut res: tokio_tungstenite::tungstenite::handshake::server::Response| {
                    // Answer the subprotocol, exactly as a Vite dev server does
                    // — and assert the visitor's `Host` survived the hop, since
                    // an app that vets it would otherwise refuse the handshake.
                    assert_eq!(
                        req.headers().get("host").unwrap().to_str().unwrap(),
                        "hmr.tunnels.test"
                    );
                    if let Some(p) = req.headers().get("sec-websocket-protocol").cloned() {
                        res.headers_mut().insert("sec-websocket-protocol", p);
                    }
                    Ok(res)
                },
            )
            .await
            .unwrap();
            while let Some(Ok(msg)) = ws.next().await {
                match msg {
                    tokio_tungstenite::tungstenite::Message::Text(_)
                    | tokio_tungstenite::tungstenite::Message::Binary(_) => {
                        ws.send(msg).await.unwrap()
                    }
                    tokio_tungstenite::tungstenite::Message::Close(_) => break,
                    _ => {}
                }
            }
        });
        (port, handle)
    }

    /// MAIN-10 AC-1 and AC-2, on the node: the upgrade reaches a real upstream,
    /// its subprotocol comes back, and a payload that is not valid UTF-8 makes
    /// the round trip byte for byte.
    #[tokio::test]
    async fn a_tunnelled_socket_carries_text_and_non_utf8_bytes_unchanged() {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD;
        let (port, upstream) = echo_upstream().await;
        let (in_tx, in_rx) = mpsc::channel::<ControlToNode>(16);
        let (out_tx, mut out_rx) = mpsc::channel::<NodeToControl>(16);
        let request_id = uuid::Uuid::now_v7();

        let pump = tokio::spawn(tunnel_ws(
            request_id,
            port,
            "/hmr".into(),
            vec![
                ("host".into(), "hmr.tunnels.test".into()),
                ("sec-websocket-protocol".into(), "vite-hmr".into()),
                // The visitor's own handshake headers, which this node's client
                // owns and must not copy — forwarding the key would make the
                // accept-hash it verifies the wrong one.
                ("sec-websocket-key".into(), "ZmFrZWtleWZha2VrZXk=".into()),
                ("sec-websocket-version".into(), "13".into()),
            ],
            in_rx,
            out_tx,
        ));

        let NodeToControl::TunnelUpgraded { headers, .. } = out_rx.recv().await.unwrap() else {
            panic!("the upstream accepted, so the node reports an upgrade");
        };
        assert_eq!(
            headers,
            vec![("sec-websocket-protocol".to_string(), "vite-hmr".to_string())],
            "the upstream's subprotocol is what the control plane must echo"
        );

        // Not valid UTF-8 anywhere in it: a lone continuation byte, a NUL and
        // an unpaired surrogate's lead byte.
        let raw: &[u8] = &[0xff, 0x00, 0xfe, 0x80, 0xed];
        in_tx
            .send(ControlToNode::TunnelWsData {
                request_id,
                data_b64: b64.encode(raw),
                binary: true,
            })
            .await
            .unwrap();
        let NodeToControl::TunnelWsData {
            data_b64, binary, ..
        } = out_rx.recv().await.unwrap()
        else {
            panic!("the echo comes back as data");
        };
        assert!(binary, "a binary frame stays binary across the hop");
        assert_eq!(b64.decode(&data_b64).unwrap(), raw);

        in_tx
            .send(ControlToNode::TunnelWsData {
                request_id,
                data_b64: b64.encode("hello"),
                binary: false,
            })
            .await
            .unwrap();
        let NodeToControl::TunnelWsData {
            data_b64, binary, ..
        } = out_rx.recv().await.unwrap()
        else {
            panic!("the echo comes back as data");
        };
        assert!(!binary);
        assert_eq!(b64.decode(&data_b64).unwrap(), b"hello");

        // AC-4: the visitor's end going away releases the upstream. The echo
        // server's task finishing is the proof — it only ends on a close.
        in_tx
            .send(ControlToNode::TunnelWsClose {
                request_id,
                code: Some(1000),
                reason: None,
            })
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), upstream)
            .await
            .expect("the node releases the upstream connection")
            .unwrap();
        pump.await.unwrap();
    }

    /// AC-5's node half: the control plane dropping the lane — which is what a
    /// stopped, swept or session-less tunnel does — closes the upstream too.
    #[tokio::test]
    async fn losing_the_control_plane_releases_the_upstream() {
        let (port, upstream) = echo_upstream().await;
        let (in_tx, in_rx) = mpsc::channel::<ControlToNode>(4);
        let (out_tx, mut out_rx) = mpsc::channel::<NodeToControl>(4);
        let request_id = uuid::Uuid::now_v7();

        let pump = tokio::spawn(tunnel_ws(
            request_id,
            port,
            "/hmr".into(),
            vec![("host".into(), "hmr.tunnels.test".into())],
            in_rx,
            out_tx,
        ));
        assert!(matches!(
            out_rx.recv().await,
            Some(NodeToControl::TunnelUpgraded { .. })
        ));

        drop(in_tx);
        tokio::time::timeout(std::time::Duration::from_secs(5), upstream)
            .await
            .expect("the upstream is released when the socket's lane closes")
            .unwrap();
        pump.await.unwrap();
    }

    /// An upstream that is not there is a named failure, not a hang: the
    /// control plane turns this into the tunnel's 502 rather than leaving a
    /// browser waiting on a handshake nobody will finish.
    #[tokio::test]
    async fn nothing_listening_fails_the_upgrade_by_name() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let (_in_tx, in_rx) = mpsc::channel::<ControlToNode>(4);
        let (out_tx, mut out_rx) = mpsc::channel::<NodeToControl>(4);
        let request_id = uuid::Uuid::now_v7();

        tunnel_ws(request_id, port, "/".into(), Vec::new(), in_rx, out_tx).await;

        let Some(NodeToControl::TunnelFailed { message, .. }) = out_rx.recv().await else {
            panic!("a dead port is reported, not waited on");
        };
        assert!(message.contains(&port.to_string()), "{message}");
    }
}
