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
    let state = AppState::new(db, cfg, oidc).await;
    // Join the cross-instance bus (LISTEN/NOTIFY): makes N control-plane
    // replicas cooperate. On a single instance it's a no-op fast path.
    state.registry.start_bus(state.db.clone());
    let instance = state.registry.instance_id();
    tracing::info!(%instance, "control plane instance");

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
            tokio::spawn(nook_control::agent_tls::serve(
                agent_listener,
                agent_router,
                tls,
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
            tokio::spawn(async move {
                if let Err(e) = axum::serve(agent_listener, agent_router).await {
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
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
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
        })
        .await?;
    Ok(())
}
