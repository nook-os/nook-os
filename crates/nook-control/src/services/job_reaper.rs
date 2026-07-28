//! The loop-job reaper (MAIN-164): a CP-side background scan that fails jobs
//! whose executor node has gone dark, so a crashed or upgraded operator
//! container never strands work in `claimed`/`running` forever.
//!
//! Liveness is derived from `nodes.last_seen_at`, which the node connection
//! already maintains (NG-1 — no new heartbeat protocol). The reap itself is an
//! atomic, multi-instance-safe conditional UPDATE in
//! [`jobs::reap_stale_executors`], so every control-plane replica may run this
//! harmlessly — a job is failed by exactly one of them.

use std::time::Duration;

use crate::services::jobs;
use crate::state::AppState;

/// How often to scan. Independent of the grace window: a short scan interval over
/// a long grace just means a dead executor's jobs fail within about one interval
/// of the grace expiring.
const SCAN_INTERVAL: Duration = Duration::from_secs(30);

/// Spawn the reaper. Fire-and-forget like the dispatch consumer; the process
/// exits on shutdown, taking the task with it.
pub fn start(state: AppState) {
    tokio::spawn(async move {
        let grace = state.cfg.job_reap_grace_secs;
        tracing::info!(grace_secs = grace, "loop-job reaper started");
        run(state, grace).await;
    });
}

async fn run(state: AppState, grace_secs: u64) {
    loop {
        tokio::time::sleep(SCAN_INTERVAL).await;
        match jobs::reap_stale_executors(&state, grace_secs).await {
            Ok(0) => {}
            Ok(n) => tracing::warn!(reaped = n, "reaped loop jobs whose executor went offline"),
            Err(e) => tracing::warn!(error = %e, "loop-job reaper scan failed"),
        }
    }
}
