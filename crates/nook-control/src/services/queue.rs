//! CP-internal enqueue surface for the durable work queue (MAIN-147).
//!
//! A thin wrapper over `AppState::queue` so control-plane code enqueues durable
//! work without reaching into the provider directly, and so the one place that
//! samples the depth gauge lives here. This is the **producer** half only:
//! there is no HTTP route and no worker draining the queue yet (NG-2).

use anyhow::Result;
use uuid::Uuid;

use crate::mailer::{EmailJob, EMAIL_WORK_TYPE};
use crate::queue::{NewWork, QueueStats};
use crate::state::AppState;

/// Enqueue a rendered email as an `email.send` job for `tenant_id` (MAIN-149).
/// The control plane renders the message; the worker drives the mail provider,
/// so the request no longer blocks on SMTP and a failed send retries there.
pub async fn enqueue_email(state: &AppState, tenant_id: Uuid, job: &EmailJob) -> Result<()> {
    let payload = serde_json::to_vec(job)?;
    enqueue(state, tenant_id, EMAIL_WORK_TYPE, payload).await?;
    Ok(())
}

/// Enqueue a unit of durable work for `tenant_id`. `payload` is the caller's
/// serialized job (JSON by convention); the queue treats it as opaque bytes.
/// Returns the message id — the idempotency key a handler will dedupe on.
pub async fn enqueue(
    state: &AppState,
    tenant_id: Uuid,
    work_type: impl Into<String>,
    payload: Vec<u8>,
) -> Result<Uuid> {
    state
        .queue
        .enqueue(NewWork::new(tenant_id, work_type, payload))
        .await
}

/// Sample the queue and emit the `queue.depth` gauge for logs / later
/// autoscaling. `ready` is the drain-able depth; `dead` the dead-letter count.
/// Returns the full stats for callers that want to act on them.
pub async fn record_depth(state: &AppState) -> Result<QueueStats> {
    let stats = state.queue.describe().await?;
    tracing::info!(
        target: "queue.depth",
        backend = %stats.backend,
        ready = stats.ready,
        in_flight = stats.in_flight,
        dead = stats.dead,
        "queue depth",
    );
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::time::Duration;

    async fn pool() -> Option<nook_db::DbPool> {
        if std::env::var("NOOK_REQUIRE_DB").ok().as_deref() != Some("1") {
            return None;
        }
        let url = std::env::var("DATABASE_URL").ok()?;
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .ok()
            .map(nook_db::EnginePool::from_pg)
    }

    #[tokio::test]
    async fn enqueue_lands_work_reachable_through_state_and_depth_reports_database() {
        let Some(db) = pool().await else {
            eprintln!("skipping enqueue_lands_work — no DATABASE_URL");
            return;
        };
        let state = AppState::new(db, Config::for_test(), None).await;

        // Scoped to a unique type + fresh tenant so the shared dev DB's other
        // rows are invisible to the receive below.
        let ty = format!("test.svc.{}", Uuid::now_v7());
        let tenant = Uuid::now_v7();
        let id = enqueue(&state, tenant, ty.clone(), b"{\"job\":true}".to_vec())
            .await
            .unwrap();

        let got = state
            .queue
            .receive(std::slice::from_ref(&ty), 10, Duration::from_secs(30))
            .await
            .unwrap();
        assert_eq!(got.len(), 1, "the enqueued job is drain-able through state");
        assert_eq!(got[0].id, id);
        assert_eq!(got[0].tenant_id, tenant);
        state.queue.ack(id).await.unwrap();

        // The gauge is available and names the configured backend.
        let stats = record_depth(&state).await.unwrap();
        assert_eq!(stats.backend, "database");
        assert!(stats.ready >= 0 && stats.dead >= 0);
    }
}
