//! The per-workspace build-loop switch, and the sweep that fires it (MAIN-385).
//!
//! MAIN-458 made the control plane able to converge build runs from the board;
//! it fired for every workspace of a loops-on tenant, which is the one thing a
//! repo's owner could not say no to. This is that decision: a workspace is
//! enabled by a person, off until they do (NG-4), and everything auto-fired
//! runs AS them.
//!
//! ## Why the enabler is the identity, and not whoever tripped the trigger
//!
//! `select_executor` resolves `person_id_of(requested_by)` and considers only
//! that person's nodes. So `requested_by` is not bookkeeping — it decides
//! whether the job can be placed at all. A run fired because somebody labelled
//! a card would otherwise be requested by *them*, and would sit queued forever
//! on a fleet they own no machine in. The enabler is the one identity that is
//! true of every auto-fire, whatever event produced it, which is why the switch
//! records it (AC-2).
//!
//! ## Two ways in, one evaluation
//!
//! The sweep (AC-5) is the floor: every enabled workspace, every
//! `build_loop.sweep_interval`. The nudges (AC-6) are the ordinary path — a
//! card gaining `agent-ready` or `loop-changes-requested`, or moving into an
//! unstarted column, evaluates that one workspace immediately, so work starts
//! in seconds. Both call [`evaluate`], which re-reads every gate; a nudge is
//! never a second rulebook, only an earlier occasion to apply the one there is.
//!
//! Dedupe, the ceiling and the claim are all `converge_builds`' (AC-7),
//! untouched: two nudges and a sweep arriving together raise what one of them
//! would have.

use std::collections::HashMap;
use std::time::Duration;

use nook_types::{TaskId, TenantId, UserId, Workspace};

use crate::error::ApiResult;
use crate::state::AppState;

/// How often the sweep evaluates a tenant's enabled workspaces. Tenant-scoped
/// like `loops.enabled`, and read on the pass rather than at boot, so a change
/// lands without a restart.
pub const SWEEP_INTERVAL_KEY: &str = "build_loop.sweep_interval";

/// Five minutes (AC-5). The sweep is the FLOOR, not the mechanism: the events
/// in AC-6 are what make ordinary work start in seconds, and this is what
/// catches a workspace whose triggering event nobody was listening for — a
/// label applied while a replica restarted, a card that became eligible
/// because a run ended.
pub const DEFAULT_SWEEP: Duration = Duration::from_secs(300);

/// A tenant cannot ask for a sweep faster than this. The pass claims cards and
/// raises jobs; a one-second interval would be a hot loop against the board
/// with nothing to gain, since every event that matters already nudges.
const MIN_SWEEP: Duration = Duration::from_secs(30);

/// How often the loop wakes to ask whether any tenant is due. Well below the
/// default interval, so a tenant that shortens theirs is honoured promptly;
/// a wake with nothing due is one indexed lookup and no more.
const TICK: Duration = Duration::from_secs(15);

/// Spawn the sweep. Fire-and-forget like the reapers; the process exits on
/// shutdown, taking the task with it.
pub fn start(state: AppState) {
    tokio::spawn(async move {
        tracing::info!("build-loop sweep started");
        run(state).await;
    });
}

async fn run(state: AppState) {
    let mut switch = crate::services::loops::SwitchLog::default();
    let mut due = Schedule::new();
    loop {
        tokio::time::sleep(TICK).await;
        // The master switch first, as every other loop consumer asks it: with
        // every tenant off this wake is a single indexed lookup and nothing
        // else — no workspace read, no board query (AC-5).
        if !switch.observe(
            "build_loop",
            crate::services::loops::any_enabled(&*state.settings).await,
        ) {
            continue;
        }
        if let Err(e) = pass(&state, Some(&mut due)).await {
            tracing::warn!(error = %e, "build-loop sweep pass failed");
        }
    }
}

/// When each tenant is next due a sweep. Held by the loop rather than in a
/// static, so the interval is re-read from the setting every time a tenant
/// comes round instead of being cached for the process's life.
pub type Schedule = HashMap<TenantId, tokio::time::Instant>;

/// One wake: every loops-on tenant whose interval has elapsed.
///
/// `schedule` is `None` for a caller that wants the pass NOW regardless of the
/// interval — which is what a test drives, so the tenant gate a test proves is
/// the same one the loop runs.
pub async fn pass(state: &AppState, schedule: Option<&mut Schedule>) -> ApiResult<()> {
    let now = tokio::time::Instant::now();
    let mut unscheduled = Schedule::new();
    let scheduled = schedule.is_some();
    let due = schedule.unwrap_or(&mut unscheduled);
    for (tenant, value) in state
        .settings
        .tenants_with_value(crate::services::loops::KEY)
        .await?
    {
        // A row can exist reading `false` — the same truthiness the per-tenant
        // gate applies, so on and off mean here what they mean everywhere.
        if !crate::services::loops::truthy(Some(&value)) {
            due.remove(&tenant);
            continue;
        }
        if scheduled {
            if due.get(&tenant).is_some_and(|next| now < *next) {
                continue;
            }
            due.insert(tenant, now + sweep_interval(state, tenant).await);
        }
        sweep_tenant(state, tenant).await;
    }
    Ok(())
}

/// Evaluate every enabled workspace of one tenant. Public so a test can drive a
/// pass without waiting on the timer.
pub async fn sweep_tenant(state: &AppState, tenant: TenantId) {
    let workspaces = match state.workspaces.build_loop_enabled(tenant).await {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(%tenant, error = %e, "could not list build-loop workspaces");
            return;
        }
    };
    for ws in workspaces {
        match evaluate(state, tenant, &ws).await {
            Ok(Some(c)) if c.raised > 0 || c.withheld > 0 => tracing::info!(
                workspace = %ws.id,
                raised = c.raised,
                live = c.live,
                withheld = c.withheld,
                "build loop raised runs"
            ),
            Ok(_) => {}
            Err(e) => tracing::warn!(workspace = %ws.id, error = %e, "build loop pass failed"),
        }
    }
}

/// The gate and the convergence for ONE workspace — what both the sweep and
/// every nudge run.
///
/// `Ok(None)` is "this workspace is not the loop's business": the switch is
/// off, or it is on with no recorded enabler, which is a row a hand-written
/// UPDATE could produce and which nothing may fire from — a job requested by
/// nobody has no person, so no node is eligible and it would queue forever.
pub async fn evaluate(
    state: &AppState,
    tenant: TenantId,
    ws: &Workspace,
) -> ApiResult<Option<crate::services::run_reconcile::Converged>> {
    let Some(requested_by) = auto_fire_identity(ws) else {
        return Ok(None);
    };
    Ok(Some(
        crate::services::jobs::converge_builds(state, tenant, requested_by, ws.id, None).await?,
    ))
}

/// Who an auto-fired run of this workspace is requested by (AC-2), or `None`
/// when nothing may be fired.
pub fn auto_fire_identity(ws: &Workspace) -> Option<UserId> {
    if !ws.build_loop_enabled {
        return None;
    }
    if ws.build_loop_enabled_by.is_none() {
        tracing::warn!(
            workspace = %ws.id,
            "build loop is enabled with no recorded enabler — nothing fired; \
             re-enable it so runs carry an identity that owns nodes"
        );
    }
    ws.build_loop_enabled_by
}

/// AC-6: something happened to a card that could make work available — evaluate
/// its workspace now instead of at the next sweep.
///
/// Spawned, so the write that triggered it never waits on a convergence, and
/// gated on the tenant switch exactly as the sweep is. Silent for a card with
/// no workspace, a workspace whose loop is off, or a tenant with loops off:
/// all three are the ordinary case, not a fault.
pub fn nudge(state: &AppState, tenant: TenantId, task: TaskId, why: &'static str) {
    let state = state.clone();
    tokio::spawn(async move {
        if !crate::services::loops::enabled(&*state.settings, tenant).await {
            return;
        }
        let Ok(Some(row)) = state.tasks.get_row(tenant, task).await else {
            return;
        };
        let Some(ws_id) = row.workspace_id else {
            return;
        };
        let Ok(Some(ws)) = state.workspaces.get(tenant, ws_id).await else {
            return;
        };
        match evaluate(&state, tenant, &ws).await {
            Ok(Some(c)) if c.raised > 0 => {
                tracing::info!(workspace = %ws_id, raised = c.raised, why, "build loop nudged")
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(workspace = %ws_id, error = %e, why, "build loop nudge failed")
            }
        }
    });
}

/// The tenant's sweep interval, floored at [`MIN_SWEEP`]. Anything the setting
/// cannot be read as a number of seconds is the default — a malformed value is
/// not a reason to stop sweeping.
async fn sweep_interval(state: &AppState, tenant: TenantId) -> Duration {
    let raw = state
        .settings
        .tenant_value(tenant, SWEEP_INTERVAL_KEY)
        .await
        .unwrap_or(None);
    interval_of(raw.as_ref())
}

/// Parse the stored value: a JSON number of seconds, or a string holding one —
/// the settings endpoint takes arbitrary JSON, so a hand-`PUT` string is
/// entirely possible.
fn interval_of(v: Option<&serde_json::Value>) -> Duration {
    let secs = match v {
        Some(serde_json::Value::Number(n)) => n.as_u64(),
        Some(serde_json::Value::String(s)) => s.trim().parse::<u64>().ok(),
        _ => None,
    };
    secs.map(Duration::from_secs)
        .unwrap_or(DEFAULT_SWEEP)
        .max(MIN_SWEEP)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_unset_or_unreadable_interval_is_the_default() {
        assert_eq!(interval_of(None), DEFAULT_SWEEP);
        assert_eq!(interval_of(Some(&json!(null))), DEFAULT_SWEEP);
        assert_eq!(interval_of(Some(&json!("soon"))), DEFAULT_SWEEP);
        assert_eq!(interval_of(Some(&json!(true))), DEFAULT_SWEEP);
    }

    #[test]
    fn a_number_of_seconds_is_honoured_but_never_below_the_floor() {
        assert_eq!(interval_of(Some(&json!(600))), Duration::from_secs(600));
        assert_eq!(interval_of(Some(&json!("600"))), Duration::from_secs(600));
        assert_eq!(interval_of(Some(&json!(1))), MIN_SWEEP);
        assert_eq!(interval_of(Some(&json!(0))), MIN_SWEEP);
    }
}
