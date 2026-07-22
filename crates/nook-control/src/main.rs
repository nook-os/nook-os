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
    tracing::info!(bind = %agent_bind, "agent listener");
    let agent_router = routes::build_agent_router(state);
    tokio::spawn(async move {
        if let Err(e) = axum::serve(agent_listener, agent_router).await {
            tracing::error!(error = %e, "agent listener stopped");
        }
    });

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
