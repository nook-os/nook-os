use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use sqlx::postgres::PgPoolOptions;
use tracing_subscriber::EnvFilter;

use nook_control::{routes, AppState, Config, MIGRATOR};

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

    let cli = Cli::parse();
    let cfg = Config::from_env()?;

    let db = PgPoolOptions::new()
        .max_connections(10)
        .connect(&cfg.database_url)
        .await?;
    MIGRATOR.run(&db).await?;

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

async fn serve(db: sqlx::PgPool, cfg: Config) -> Result<()> {
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
    // Join the cross-instance bus (LISTEN/NOTIFY): makes N control-plane
    // replicas cooperate. On a single instance it's a no-op fast path.
    state.registry.start_bus(state.db.clone());
    let instance = state.registry.instance_id();
    tracing::info!(%instance, "control plane instance");

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

    // The agent gets its own listener. Bound first so a port clash fails the
    // boot loudly rather than leaving the fleet with nowhere to connect.
    let agent_listener = tokio::net::TcpListener::bind(&agent_bind)
        .await
        .with_context(|| format!("cannot bind the agent port {agent_bind}"))?;
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

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(%bind, "control plane listening");

    let drain_rx = shutdown_rx.clone();
    let serve = axum::serve(listener, router).with_graceful_shutdown(async move {
        wait_for_shutdown(drain_rx).await;
        tracing::info!("shutting down — releasing node leases");
        // Nodes we own reconnect elsewhere and re-lease; mark them offline
        // until they do so schedulers don't route to dead sockets.
        let _ = sqlx::query(
            "UPDATE nodes SET status = 'offline', updated_at = now(),
                owning_instance_id = NULL, lease_expires_at = NULL
             WHERE owning_instance_id = $1",
        )
        .bind(instance)
        .execute(&shutdown_db)
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
    use super::wait_for_shutdown;
    use std::time::Duration;

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
