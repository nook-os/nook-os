//! The loop-job executor-selection consumer (MAIN-160).
//!
//! A background task that drains `loop.job` items off the durable queue and
//! runs a dispatch pass for the tenant each one belongs to. The queue's receive
//! is the first layer of the atomic claim (AC-2) — one consumer, even across
//! control-plane replicas, gets a given item — and `select_executor`'s
//! conditional UPDATE is the second, so a job is placed on exactly one
//! executor.
//!
//! **A delivery is an OCCASION to dispatch, not an instruction to place the job
//! it names (MAIN-509).** It used to be the latter, and that made placement
//! accidental LIFO: an unplaceable job is re-armed after `RETRY_DELAY` while a
//! freshly raised one is delivered immediately, so whichever item happened to
//! be in the queue when an executor freed won — systematically the newest,
//! because a run concluding is exactly what raises the next card. Urgent jobs
//! sat for hours while newer, lower-priority ones went straight through.
//!
//! So an item now only says "a pass is owed", and
//! [`jobs::place_queued_in_order`] decides who gets the executor, from the
//! whole queued set in one defined order. That is what makes a 30s-delayed item
//! compete fairly with a 0s one: the new job's early delivery hands the freed
//! executor to the job that most deserves it, which is usually the old one.
//! Deliveries received together are ONE occasion per tenant — ten jobs re-armed
//! at the same moment are ten reminders that a pass is owed, not ten passes.
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
//! job leaves `queued` (claimed, canceled): the settle step reads a non-queued
//! state and acks without re-enqueuing — including when the pass this very
//! delivery triggered is what placed it.
//!
//! "Canceled" was an assumption with nothing behind it until MAIN-496: a job
//! nothing could place was re-enqueued here forever, because the only thing
//! that ever canceled one was a human. The two queued-job endings in
//! [`crate::services::job_reaper`] are what make this sentence true.

use std::collections::HashSet;
use std::time::Duration;

use nook_types::{JobId, TenantId};
use uuid::Uuid;

use crate::error::ApiResult;
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
            Ok(items) => handle_batch(&state, &items).await,
            Err(e) => {
                tracing::warn!(error = %e, "loop-job consumer receive failed");
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        }
    }
}

/// One receive: run the pass each represented tenant is owed, then settle every
/// delivery against the state that pass left behind.
///
/// The two halves are deliberately separate. A delivery no longer decides
/// anything about its own job — the pass does, and it may well have placed a
/// different one — so settling can only be honest if it reads the job AFTER the
/// pass rather than from what placing it returned.
async fn handle_batch(state: &AppState, items: &[crate::queue::WorkEnvelope]) {
    let mut swept: HashSet<Uuid> = HashSet::new();
    let mut failed: HashSet<Uuid> = HashSet::new();
    for item in items {
        let tenant = TenantId(item.tenant_id);
        // Per-tenant gate. `any_enabled` in `run` got us into this receive, but
        // an item may belong to a tenant whose loops are off — place nothing
        // for it. The job keeps its queued state and runs when the switch flips
        // (AC-3: off loses no work); `settle` re-arms the item below.
        if !loops::enabled(&*state.settings, tenant).await {
            continue;
        }
        if !swept.insert(item.tenant_id) {
            continue;
        }
        if let Err(e) = dispatch_pass(state, tenant).await {
            tracing::warn!(tenant = %item.tenant_id, error = %e, "dispatch pass failed; will retry");
            failed.insert(item.tenant_id);
        }
    }
    for item in items {
        settle(state, item, failed.contains(&item.tenant_id)).await;
    }
}

/// Place what this tenant's queued jobs deserve, then hand each placement to
/// its node to run (MAIN-161).
async fn dispatch_pass(state: &AppState, tenant: TenantId) -> ApiResult<()> {
    for job in jobs::place_queued_in_order(state, tenant).await? {
        if let Err(e) = jobs::dispatch_to_node(state, tenant, &job).await {
            tracing::warn!(job = %job.id, error = %e, "dispatching job to node failed");
        }
    }
    Ok(())
}

/// Retire one delivery according to where its job now stands.
async fn settle(state: &AppState, item: &crate::queue::WorkEnvelope, pass_failed: bool) {
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

    if pass_failed {
        // A transient failure (DB blip): let it redeliver and try again.
        let _ = state.queue.nack(item.id, Nack::Requeue).await;
        return;
    }

    let still_queued = match state.jobs.get(tenant, job_id).await {
        // Loops off for this tenant, or nothing could place it yet: re-arm.
        Ok(Some(job)) => job.state == "queued",
        // The job is gone — nothing to re-arm for.
        Ok(None) => false,
        Err(e) => {
            tracing::warn!(job = %job_id, error = %e, "reading a dispatched job failed; will retry");
            let _ = state.queue.nack(item.id, Nack::Requeue).await;
            return;
        }
    };

    let _ = state.queue.ack(item.id).await;
    if still_queued {
        // Nothing could place it. Re-arm a delayed delivery so the job is
        // re-evaluated later (AC-3) rather than dead-lettering.
        let _ = state
            .queue
            .enqueue(
                NewWork::new(tenant.0, jobs::WORK_TYPE, item.payload.clone()).delay(RETRY_DELAY),
            )
            .await;
    }
}
