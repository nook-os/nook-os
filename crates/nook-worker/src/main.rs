//! The `nook-worker` binary: connect to the queue and DB, then drain until a
//! termination signal, finishing in-flight work on the way out (MAIN-148).

use anyhow::Result;
use sqlx::postgres::PgPoolOptions;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

use nook_infra::{queue, Config};
use nook_worker::{resolve_work_types, run, Registry};

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    // Same env surface as the control plane (AC-1): one `Config`, one set of
    // env names. The worker uses the DB URL and the queue provider; it validates
    // the rest so a misconfiguration fails the same way here as there.
    let cfg = Config::from_env()?;

    let db = PgPoolOptions::new()
        .max_connections(5)
        .connect(&cfg.database_url)
        .await?;

    // The worker does NOT run migrations — the control plane owns them
    // (MAIN-146 NG-2). It just connects to the schema they produced.
    let queue: Arc<dyn queue::Queue> = Arc::from(queue::from_config(&cfg, db));
    let registry = Registry::with_builtins();
    let types = resolve_work_types(&registry);

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = shutdown_tx.send(true);
    });

    run(queue, registry, types, shutdown_rx).await
}

/// Resolve on the first termination signal. Kubernetes sends **SIGTERM** on
/// shutdown; both it and SIGINT map to the same graceful drain.
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
