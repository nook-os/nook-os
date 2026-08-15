//! The loop-job reaper (MAIN-164): a CP-side background scan that fails jobs
//! whose executor node has gone dark, so a crashed or upgraded operator
//! container never strands work in `claimed`/`running` forever.
//!
//! Liveness is derived from `nodes.last_seen_at`, which the node connection
//! already maintains (NG-1 — no new heartbeat protocol). The reap itself is an
//! atomic, multi-instance-safe conditional UPDATE in
//! [`jobs::reap_stale_executors`], so every control-plane replica may run this
//! harmlessly — a job is failed by exactly one of them.
//!
//! Node liveness is not the only way a run is stranded, though, and MAIN-506 is
//! the case it cannot see: an executor AGENT that restarts leaves its streaming
//! child running with nobody reading it, while the node itself reconnects and
//! keeps heartbeating — so the cutoff above never trips. [`jobs::reap_stalled_jobs`]
//! scans the same in-flight set on the job's OWN progress instead, and is global
//! and atomic for the same reasons.
//!
//! Silence is not always failure, either: a run that already RECORDED its
//! conclusion has concluded, and losing the completion signal that should have
//! followed does not un-conclude it (MAIN-607). That scan therefore completes
//! such a job instead of failing it — so it stops holding loop capacity — and
//! leaves the handback alone, because the card it would give back is finished,
//! reviewed and carrying its PR.
//!
//! It also ends jobs that never got that far (MAIN-496). The reap above needs
//! an `executor_node_id`, which a `queued` job does not have, so a job nothing
//! could place had no exit at all: one scan cancels a queued job whose card has
//! been closed under it, the other escalates one whose `queued_reason` has not
//! moved in `job_starve_secs`. Both live here rather than in a fourth loop —
//! same cadence, same atomic-UPDATE property, same loops-off gate.
//!
//! Those two are asked PER TENANT — `any_enabled` then `enabled`, the pair
//! `loops`' own module doc prescribes — while the reap above stays global. The
//! difference is what they write: a reap changes job state, but these mutate a
//! board a human reads (a `blocked` label, a comment, a notification), and
//! doing that to a tenant with loops OFF because a DIFFERENT tenant has them on
//! would break MAIN-239's promise that such a job simply waits for the switch.

use std::time::Duration;

use crate::services::{jobs, loops};
use crate::state::AppState;

/// How often to scan. Independent of the grace window: a short scan interval over
/// a long grace just means a dead executor's jobs fail within about one interval
/// of the grace expiring.
const SCAN_INTERVAL: Duration = Duration::from_secs(30);

/// Spawn the reaper. Fire-and-forget like the dispatch consumer; the process
/// exits on shutdown, taking the task with it.
pub fn start(state: AppState) {
    tokio::spawn(async move {
        let (grace, starve, stall) = (
            state.cfg.job_reap_grace_secs,
            state.cfg.job_starve_secs,
            state.cfg.job_stall_secs,
        );
        tracing::info!(
            grace_secs = grace,
            starve_secs = starve,
            stall_secs = stall,
            "loop-job reaper started"
        );
        run(state, grace, starve, stall).await;
    });
}

async fn run(state: AppState, grace_secs: u64, starve_secs: u64, stall_secs: u64) {
    let mut switch = loops::SwitchLog::default();
    loop {
        tokio::time::sleep(SCAN_INTERVAL).await;
        // No loops, nothing to reap: there are no runs to strand (MAIN-239).
        if !switch.observe("job_reaper", loops::any_enabled(&*state.settings).await) {
            continue;
        }
        match jobs::reap_stale_executors(&state, grace_secs).await {
            Ok(0) => {}
            Ok(n) => tracing::warn!(reaped = n, "reaped loop jobs whose executor went offline"),
            Err(e) => tracing::warn!(error = %e, "loop-job reaper scan failed"),
        }
        // After the liveness reap, never before: a job whose node genuinely went
        // away is also silent, and the offline node is the more specific — and
        // more useful — explanation to put on its transcript.
        match jobs::scan_stalled_jobs(&state, stall_secs).await {
            Ok(r) if r.failed == 0 && r.concluded == 0 => {}
            Ok(r) => tracing::warn!(
                failed = r.failed,
                concluded = r.concluded,
                "reaped loop jobs that stopped making progress"
            ),
            Err(e) => tracing::warn!(error = %e, "stalled-job scan failed"),
        }
        for tenant in loops::enabled_tenants(&*state.settings).await {
            // Doomed before starved: a job whose card just closed is canceled on
            // the card's evidence, so it never reaches the escalation that would
            // comment on a card nobody is working any more.
            match jobs::cancel_queued_on_finished_cards(&state, tenant).await {
                Ok(0) => {}
                Ok(n) => {
                    tracing::warn!(%tenant, canceled = n, "canceled queued jobs whose card is finished")
                }
                Err(e) => tracing::warn!(%tenant, error = %e, "finished-card queued scan failed"),
            }
            match jobs::escalate_starved_queued(&state, tenant, starve_secs).await {
                Ok(0) => {}
                Ok(n) => {
                    tracing::warn!(%tenant, escalated = n, "starved queued jobs escalated to a human")
                }
                Err(e) => tracing::warn!(%tenant, error = %e, "starved-job scan failed"),
            }
        }
    }
}
