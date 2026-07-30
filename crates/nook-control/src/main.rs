use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use nook_db::{params, Db, Postgres, TypeMapping};
use tracing_subscriber::EnvFilter;

use nook_control::{routes, AppState, Config, MIGRATOR, SQUASH_MANIFEST};

#[derive(Parser)]
#[command(name = "nook-control", about = "NookOS control plane")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the control plane server (default).
    Serve,
    /// Seed the database with dev fixtures (idempotent).
    Seed,
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    // After the subscriber, so the hook's chained default and the layer's
    // structured record both land in the configured log (MAIN-273).
    nook_errors::install_panic_hook();

    let cli = Cli::parse();
    let cfg = Config::from_env()?;

    // One control plane per SQLite file (MAIN-197). Taken BEFORE the pool,
    // because `create_if_missing` means connecting is already a write, and the
    // value of this check is entirely in refusing before the first one.
    //
    // The binding is named on purpose. The lock lives on an open descriptor, so
    // `let _ = …` would drop it here and enforce nothing while looking correct;
    // holding it in `main`'s scope is what makes it last as long as the process.
    // Postgres takes no lock and multi-instance is unaffected.
    let _instance_lock = nook_db::acquire_single_instance_lock(&cfg.database_url)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("this control plane cannot use that database")?;
    if let Some(lock) = &_instance_lock {
        tracing::info!(lock = %lock.path().display(), "holding the SQLite single-instance lock");
    }

    // Select the engine from the DATABASE_URL scheme and refuse an unknown one
    // here, at boot, with a pointed message (MAIN-195). Postgres connects exactly
    // as before; the pool type is unchanged.
    let db = nook_db::connect(&cfg.database_url, 10)
        .await
        .context("opening the database")?;
    // The migration set follows the engine (MAIN-196): the Postgres track, or
    // the SQLite one for a `sqlite://` URL. On Postgres a pre-squash ledger
    // collapses to the canonical row first (MAIN-235) — the image that carries
    // a squash carries its own re-stamp, never the two-step ordering that
    // caused the documented prod near-miss — and dev tolerates a ledger ahead
    // of this checkout while production stays strictly fatal (MAIN-224).
    nook_db::migrate::run_boot_migrations_for(
        &db,
        cfg.is_production(),
        &MIGRATOR,
        &nook_control::MIGRATOR_SQLITE,
        SQUASH_MANIFEST,
    )
    .await?;

    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => {
            // Always seed: built-in themes ship with every install, so a fresh
            // production instance has real choice out of the box. `seed::run`
            // stops there in production — demo tenants and dev join tokens
            // stay dev-only.
            nook_control::seed::run(&db, &cfg).await?;
            serve(db, cfg).await
        }
        Command::Seed => nook_control::seed::run(&db, &cfg).await,
    }
}

async fn serve(db: nook_db::DbPool, cfg: Config) -> Result<()> {
    // Pick the TLS backend explicitly. Several crates in the tree pull rustls
    // with different providers (the AWS SDK among them), which leaves the
    // process-wide default ambiguous — and rustls panics rather than guessing.
    // Installing it here makes the choice ours and the failure impossible.
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Discover the IdP once at startup. Failure is non-fatal so the stack
    // boots without the IdP reachable (dev-login still works).
    let oidc = match cfg.oidc_issuer_url.as_deref() {
        Some(issuer) if cfg.oidc_configured() => {
            match nook_control::auth::OidcContext::discover(issuer).await {
                Ok(ctx) => {
                    tracing::info!(issuer, "OIDC discovery complete");
                    Some(ctx)
                }
                Err(e) => {
                    tracing::warn!(issuer, error = %e, "OIDC discovery failed — IdP login disabled");
                    None
                }
            }
        }
        _ => {
            tracing::warn!("OIDC not configured — IdP login disabled");
            None
        }
    };

    let bind = cfg.bind.clone();
    let agent_bind = cfg.agent_bind.clone();
    let agent_tls_cert = cfg.agent_tls_cert.clone();
    let agent_tls_key = cfg.agent_tls_key.clone();
    let is_production = cfg.is_production();
    let grace = std::time::Duration::from_secs(cfg.shutdown_grace_secs);
    let state = AppState::new(db, cfg, oidc).await;

    // If OIDC is configured but the boot-time discovery failed (the IdP was
    // unreachable), keep retrying in the background with backoff so login
    // recovers on its own — no container restart (MAIN-169 AC-1). A successful
    // login also triggers an immediate on-demand attempt; this task is the
    // unattended path. We do not re-discover after success (NG-4).
    if state.oidc.degraded() {
        let oidc = state.oidc.clone();
        tokio::spawn(async move {
            let mut backoff = std::time::Duration::from_secs(2);
            let max = std::time::Duration::from_secs(30);
            while oidc.degraded() {
                tokio::time::sleep(backoff).await;
                match oidc.discover_now().await {
                    // Success logs INFO with the issuer inside `discover_now`.
                    Ok(_) => break,
                    Err(e) => {
                        tracing::debug!(error = %e, "OIDC discovery retry failed — will retry");
                        backoff = (backoff * 2).min(max);
                    }
                }
            }
        });
    }

    // Join the cross-instance bus (LISTEN/NOTIFY): makes N control-plane
    // replicas cooperate. On a single instance it's a no-op fast path.
    // Cross-instance fan-out is Postgres LISTEN/NOTIFY. A SQLite deployment is
    // one process against one file — there is no second replica to coordinate
    // with — so the bus is simply not started, which the registry already
    // supports as its documented fast path ("without start_bus … identical to
    // the original in-memory registry"). Starting a Postgres listener on a
    // SQLite pool would panic, and faking a bus would be pretending to solve a
    // problem that cannot arise (MAIN-196).
    if state.db.engine() == nook_db::Engine::Postgres {
        state.registry.start_bus(state.db.clone());
    } else {
        tracing::info!(
            "single-instance engine — cross-instance event bus not started (in-memory fan-out only)"
        );
    }
    let instance = state.registry.instance_id();
    tracing::info!(%instance, "control plane instance");

    // Drain queued loop jobs onto eligible executors (MAIN-160). Every replica
    // runs it; the queue's atomic receive keeps them from double-claiming.
    nook_control::services::job_dispatch::start(state.clone());

    // Reap jobs whose executor node went dark, so a crashed/upgraded operator
    // never strands work (MAIN-164). Every replica runs it; the reap's atomic
    // conditional UPDATE keeps them from double-failing a job.
    nook_control::services::job_reaper::start(state.clone());

    // Reclaim checkouts that have stayed tombstoned past the retention window
    // (MAIN-220): reconcile now marks a vanished checkout missing instead of
    // deleting it, so this is the only background path that hard-deletes the row
    // — and it logs any task still pointing at the reclaimed path.
    nook_control::services::workspace_reaper::start(state.clone());

    // One signal, every listener. A single task watches for SIGTERM/SIGINT and
    // flips a watch channel; the browser door, the agent door, and the grace
    // timer each hold a receiver, so a rolling update drains all of them at once
    // rather than only whichever `axum::serve` happened to own the signal.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = shutdown_tx.send(true);
    });

    let shutdown_db = state.db.clone();
    let router = routes::build_router(state.clone());

    // BOTH doors are bound before EITHER is served, and the browser door goes
    // first (MAIN-285).
    //
    // The agent door used to be bound *and served* before the browser port was
    // even attempted. A boot that was going to fail on the browser port
    // therefore still opened the agent door for the ~1s it took to get there —
    // long enough for every node to complete a WebSocket handshake and be cut
    // off when the process exited. Under a restart loop that is not a blip: it
    // is a permanent connect/close flap in which no node ever holds a session
    // long enough to finish a capability push, so the control plane serves
    // stale capabilities (a runtime reported `not_authorized` long after it was
    // authorized) and the node is undispatchable.
    //
    // Binding both first makes a doomed boot fail with the fleet's door still
    // shut: nodes see a refused connection, back off, and reconnect once a boot
    // actually succeeds. Binding the browser port first is what guarantees it
    // for the case that actually happens — the browser port is the contended
    // one, since it is the port everything else in a deployment also talks to.
    let (listener, agent_listener) = bind_doors(&bind, &agent_bind).await?;
    let agent_router = routes::build_agent_router(state);
    match (
        agent_tls_cert.as_deref().filter(|s| !s.is_empty()),
        agent_tls_key.as_deref().filter(|s| !s.is_empty()),
    ) {
        (Some(cert), Some(key)) => {
            // TLS terminates HERE, not at the proxy: only the control plane can
            // judge a client certificate against the right tenant's CA.
            let tls = nook_control::agent_tls::acceptor(cert, key)?;
            tracing::info!(bind = %agent_bind, "agent listener (mTLS)");
            let agent_shutdown = wait_for_shutdown(shutdown_rx.clone());
            tokio::spawn(nook_control::agent_tls::serve(
                agent_listener,
                agent_router,
                tls,
                agent_shutdown,
            ));
        }
        (None, None) if is_production => {
            // The agent port carries enrolment and every node's connection.
            // Serving it in the clear in production would put join tokens and
            // CSRs on the wire, and the warning below is too easy to miss in a
            // log — this is a misconfiguration that should stop the process.
            anyhow::bail!(
                "the agent listener on {agent_bind} would be PLAINTEXT: set \
                 NOOK_AGENT_TLS_CERT and NOOK_AGENT_TLS_KEY (see \
                 deploy/enable-agent-mtls.sh)"
            );
        }
        (None, None) => {
            tracing::warn!(
                bind = %agent_bind,
                "agent listener is PLAINTEXT — set NOOK_AGENT_TLS_CERT and \
                 NOOK_AGENT_TLS_KEY so node connections are mutually authenticated"
            );
            let agent_shutdown = wait_for_shutdown(shutdown_rx.clone());
            tokio::spawn(async move {
                if let Err(e) = axum::serve(agent_listener, agent_router)
                    .with_graceful_shutdown(agent_shutdown)
                    .await
                {
                    tracing::error!(error = %e, "agent listener stopped");
                }
            });
        }
        // Half-configured is a mistake worth failing on rather than quietly
        // serving plaintext to a fleet the operator believes is encrypted.
        _ => anyhow::bail!("NOOK_AGENT_TLS_CERT and NOOK_AGENT_TLS_KEY must be set together"),
    }

    tracing::info!(%bind, "control plane listening");

    let drain_rx = shutdown_rx.clone();
    // `into_make_service_with_connect_info` exposes the peer socket address to
    // handlers via `ConnectInfo<SocketAddr>` — the invite-preview rate limiter's
    // client-IP resolver needs the real peer to decide whether to believe XFF.
    let serve = axum::serve(
        listener,
        router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        wait_for_shutdown(drain_rx).await;
        tracing::info!("shutting down — releasing node leases");
        // Nodes we own reconnect elsewhere and re-lease; mark them offline
        // until they do so schedulers don't route to dead sockets.
        let _ = shutdown_db
            .exec(
                &format!(
                    "UPDATE nodes SET status = 'offline', updated_at = {now},
                owning_instance_id = NULL, lease_expires_at = NULL
             WHERE owning_instance_id = $1",
                    now = Postgres.now()
                ),
                params![instance],
            )
            .await;
    });

    // Bound the drain. `with_graceful_shutdown` waits for every in-flight
    // request with no ceiling; one hung handler would otherwise hold the process
    // until Kubernetes' own grace period expires and SIGKILLs it. Race the drain
    // against a timer that starts when the signal fires: whichever finishes
    // first, we exit 0 — either drained cleanly, or forced out after the grace.
    let timer_rx = shutdown_rx.clone();
    tokio::select! {
        res = serve => res?,
        _ = async move {
            wait_for_shutdown(timer_rx).await;
            tokio::time::sleep(grace).await;
        } => {
            tracing::warn!(grace_secs = grace.as_secs(), "grace period elapsed — forcing shutdown");
        }
    }
    Ok(())
}

/// Bind the browser door and the agent door, in that order, before either is
/// served (MAIN-285).
///
/// Returning both listeners together is the point: a caller cannot serve one
/// door and then discover the other is unavailable, which is the state that
/// makes a fleet flap rather than back off. If the second bind fails, the first
/// listener is dropped on the error path and its socket closes with it.
///
/// The browser door is first because it is the contended one — everything in a
/// deployment talks to it, so it is the port a stray process is holding.
async fn bind_doors(
    bind: &str,
    agent_bind: &str,
) -> anyhow::Result<(tokio::net::TcpListener, tokio::net::TcpListener)> {
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        // Without this the failure reads only `Address already in use (os error
        // 98)`: no port, no door, nothing an operator can act on. That is
        // exactly how a week-long flap went undiagnosed.
        .with_context(|| format!("cannot bind the control plane port {bind}"))?;
    let agent_listener = tokio::net::TcpListener::bind(agent_bind)
        .await
        .with_context(|| format!("cannot bind the agent port {agent_bind}"))?;
    Ok((listener, agent_listener))
}

/// Resolve on the first termination signal. Kubernetes sends **SIGTERM** on pod
/// shutdown, so listening only for SIGINT (Ctrl-C) would leave a pod hanging
/// until the kill-grace timeout turned into a SIGKILL mid-request. Both map to
/// the same graceful drain here.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
        tokio::select! {
            _ = term.recv() => tracing::info!("received SIGTERM"),
            _ = int.recv() => tracing::info!("received SIGINT"),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// A future that completes when the shutdown watch flips true. A dropped sender
/// (should not happen before shutdown) also completes it — fail toward draining.
async fn wait_for_shutdown(mut rx: tokio::sync::watch::Receiver<bool>) {
    if *rx.borrow_and_update() {
        return;
    }
    let _ = rx.wait_for(|flagged| *flagged).await;
}

#[cfg(test)]
mod tests {
    use super::{bind_doors, wait_for_shutdown};
    use std::time::Duration;

    /// A free port, released before the call under test uses it. Racy in
    /// principle, fine in practice and far better than a hard-coded port that
    /// collides with whatever else the suite is running.
    async fn free_port() -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        drop(l);
        addr.to_string()
    }

    /// MAIN-285: the flap. When the control plane cannot take its browser port,
    /// it must NOT have opened the agent door on the way there — a node that
    /// completes a handshake against a process which is about to exit is a node
    /// that reconnects a second later, forever, and never holds a session long
    /// enough to push its capabilities.
    #[tokio::test]
    async fn a_boot_that_cannot_take_the_browser_port_never_opens_the_agent_door() {
        // Something else already owns the browser port — a previous server that
        // outlived its restart, which is exactly the dev-loop case.
        let squatter = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let taken = squatter.local_addr().unwrap().to_string();
        let agent = free_port().await;

        let err = bind_doors(&taken, &agent)
            .await
            .expect_err("the browser port is taken, so the boot must fail");

        // The failure names the port. `Address already in use (os error 98)`
        // alone is what made this take a week to find.
        let chain = err
            .chain()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(": ");
        assert!(
            chain.contains(&taken),
            "the error must name the contended port, got: {chain}"
        );

        // The agent door is still shut: binding it now succeeds, which it could
        // not do if the failed boot had left a listener on it.
        tokio::net::TcpListener::bind(&agent)
            .await
            .expect("the agent door must be free — a doomed boot must not open it");
    }

    /// The other order still fails loudly, and leaves nothing behind.
    #[tokio::test]
    async fn a_contended_agent_port_fails_the_boot_and_releases_the_browser_port() {
        let browser = free_port().await;
        let squatter = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let taken_agent = squatter.local_addr().unwrap().to_string();

        let err = bind_doors(&browser, &taken_agent)
            .await
            .expect_err("the agent port is taken, so the boot must fail");
        assert!(err.to_string().contains(&taken_agent));

        // The browser listener bound first must have been dropped with the
        // error, not leaked.
        tokio::net::TcpListener::bind(&browser)
            .await
            .expect("the browser port must be released when the agent bind fails");
    }

    /// The happy path hands back both doors, so the caller can serve them
    /// together rather than one at a time.
    #[tokio::test]
    async fn both_doors_come_back_together() {
        let browser = free_port().await;
        let agent = free_port().await;
        let (a, b) = bind_doors(&browser, &agent).await.expect("both bind");
        assert_eq!(a.local_addr().unwrap().to_string(), browser);
        assert_eq!(b.local_addr().unwrap().to_string(), agent);
    }

    #[tokio::test]
    async fn wait_for_shutdown_resolves_when_the_flag_flips() {
        let (tx, rx) = tokio::sync::watch::channel(false);
        let waiter = tokio::spawn(wait_for_shutdown(rx));
        tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("shutdown future resolves once the flag flips")
            .unwrap();
    }

    #[tokio::test]
    async fn wait_for_shutdown_returns_at_once_if_already_flagged() {
        // A listener that clones the receiver AFTER the signal already fired
        // must still drain immediately, not hang waiting for a change.
        let (_tx, rx) = tokio::sync::watch::channel(true);
        tokio::time::timeout(Duration::from_millis(100), wait_for_shutdown(rx))
            .await
            .expect("an already-set shutdown returns without waiting");
    }

    // The mechanism SIGTERM drives: an axum server wired to the watch stops
    // accepting and its `serve` future returns once the flag flips. This is the
    // behavioural half of AC-3 (graceful drain) without raising a real signal
    // into the shared test process.
    #[tokio::test]
    async fn a_wired_server_drains_and_stops_accepting_on_shutdown() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::watch::channel(false);
        let router = axum::Router::new().route(
            "/livez",
            axum::routing::get(nook_control::routes::health::livez),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(wait_for_shutdown(rx))
                .await
                .unwrap();
        });

        // Accepting before the signal.
        tokio::net::TcpStream::connect(addr)
            .await
            .expect("server accepts before shutdown");

        // After the signal, the serve future completes on its own.
        tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("server drains and returns after shutdown is signalled")
            .unwrap();
    }
}
