//! The loop-job executor-selection consumer (MAIN-160).
//!
//! A background task that drains `loop.job` items off the durable queue and
//! runs [`jobs::select_executor`] on each. The queue's receive is the first
//! layer of the atomic claim (AC-2) — one consumer, even across control-plane
//! replicas, gets a given item — and `select_executor`'s conditional UPDATE is
//! the second, so a job is placed on exactly one executor.
//!
//! The `nook-worker` process deliberately links only `nook-infra`, so it cannot
//! host this: selection needs the node/job domain (capabilities parsing, the
//! job service). It therefore runs here, where those types live. The worker
//! never claims `loop.job` — it only receives its registered work types — so
//! there is no contention between the two.
//!
//! When a job has no eligible executor yet (no node online / runtime
//! unauthorized / no operator), its work item is acked and a fresh one is
//! re-enqueued after a delay, so the job is re-evaluated as nodes come online
//! (AC-3) rather than dead-lettering. The re-enqueue stops naturally once the
//! job leaves `queued` (claimed, canceled): `select_executor` returns a
//! non-queued state and the loop acks without re-enqueuing.
//!
//! "Canceled" was an assumption with nothing behind it until MAIN-496: a job
//! nothing could place was re-enqueued here forever, because the only thing
//! that ever canceled one was a human. The two queued-job endings in
//! [`crate::services::job_reaper`] are what make this sentence true.

use std::time::Duration;

use nook_types::{JobId, TenantId};

use crate::queue::{Nack, NewWork};
use crate::services::{jobs, loops};
use crate::state::AppState;

/// Items claimed per receive.
const BATCH: usize = 10;
/// How long a claimed item stays invisible while we place it — placement is a
/// couple of quick queries, so this is generous.
const VISIBILITY: Duration = Duration::from_secs(30);
/// Idle wait after an empty receive.
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// How long to hold a still-unplaceable job before re-evaluating it.
const RETRY_DELAY: Duration = Duration::from_secs(30);

/// Spawn the consumer. Fire-and-forget, like the chat bus listener: the process
/// exits on shutdown, taking the task with it.
pub fn start(state: AppState) {
    tokio::spawn(async move {
        tracing::info!("loop-job executor-selection consumer started");
        run(state).await;
    });
}

async fn run(state: AppState) {
    let types = [jobs::WORK_TYPE.to_string()];
    let mut switch = loops::SwitchLog::default();
    loop {
        // The master switch (MAIN-239), re-read every tick so a flip takes
        // effect within one poll interval without a restart. With every tenant
        // off this is a single indexed lookup and we never touch the queue —
        // which is what "off is genuinely quiet" means: no claiming, no
        // dispatch, and queued jobs simply keep waiting.
        if !switch.observe("job_dispatch", loops::any_enabled(&*state.settings).await) {
            tokio::time::sleep(POLL_INTERVAL).await;
            continue;
        }
        match state.queue.receive(&types, BATCH, VISIBILITY).await {
            Ok(items) if items.is_empty() => tokio::time::sleep(POLL_INTERVAL).await,
            Ok(items) => {
                for item in items {
                    handle(&state, &item).await;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "loop-job consumer receive failed");
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        }
    }
}

async fn handle(state: &AppState, item: &crate::queue::WorkEnvelope) {
    let tenant = TenantId(item.tenant_id);
    let Ok(job_id) = serde_json::from_slice::<JobId>(&item.payload) else {
        // A malformed payload will never become valid — drop it, don't retry.
        tracing::warn!(id = %item.id, "loop.job item has an unreadable payload; dead-lettering");
        let _ = state
            .queue
            .nack(item.id, Nack::Dead("unreadable payload".into()))
            .await;
        return;
    };

    // Per-tenant gate. `any_enabled` above got us into this pass, but the item
    // may belong to a tenant whose loops are off — so re-arm it exactly as an
    // unplaceable job is re-armed and place nothing. The job keeps its queued
    // state and runs when the switch flips (AC-3: off loses no work).
    if !loops::enabled(&*state.settings, tenant).await {
        let _ = state.queue.ack(item.id).await;
        let _ = state
            .queue
            .enqueue(
                NewWork::new(tenant.0, jobs::WORK_TYPE, item.payload.clone()).delay(RETRY_DELAY),
            )
            .await;
        return;
    }

    match jobs::select_executor(state, tenant, job_id).await {
        Ok(job) if job.state == "queued" => {
            // Still no eligible executor. Ack this delivery and re-arm a delayed
            // one so the job is re-evaluated later (AC-3) without dead-lettering.
            let _ = state.queue.ack(item.id).await;
            let _ = state
                .queue
                .enqueue(
                    NewWork::new(tenant.0, jobs::WORK_TYPE, item.payload.clone())
                        .delay(RETRY_DELAY),
                )
                .await;
        }
        // Freshly placed on an executor: hand it to that node to run (MAIN-161).
        // `dispatch_to_node` is idempotent — a re-delivery of an already-running
        // job (state no longer `claimed`) is a no-op — and fails the job itself
        // if there is nowhere to run it, so we always ack.
        Ok(job) if job.state == "claimed" => {
            if let Err(e) = jobs::dispatch_to_node(state, tenant, &job).await {
                tracing::warn!(job = %job_id, error = %e, "dispatching job to node failed");
            }
            let _ = state.queue.ack(item.id).await;
        }
        // Already running / gone terminal (e.g. canceled while queued): nothing
        // more to do — let the item go.
        Ok(_) => {
            let _ = state.queue.ack(item.id).await;
        }
        Err(e) => {
            // A transient failure (DB blip): let it redeliver and try again.
            tracing::warn!(job = %job_id, error = %e, "executor selection failed; will retry");
            let _ = state.queue.nack(item.id, Nack::Requeue).await;
        }
    }
}
