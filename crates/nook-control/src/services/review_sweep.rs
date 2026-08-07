//! The board-signal review sweep (MAIN-408 AC-1/AC-4).
//!
//! Review jobs need to be RAISED, not just runnable. This is the automatic
//! half: a periodic scan that turns a board signal into at most one queued
//! review per workspace. The manual half is `POST /api/v1/reviews`, and both
//! go through [`crate::services::jobs::enqueue_review`] — one enqueue path, one
//! dedupe rule (AC-3).
//!
//! **The signal is MAIN-143 AC-3's, not a new one:** a workspace with at least
//! one live card in a `review`-type column, and no review job already in
//! flight. The first half is
//! [`crate::repo::tasks::TaskRepository::workspaces_with_cards_in_review`], the
//! second is
//! [`crate::repo::jobs::LoopJobRepository::active_review_for_workspace`] — the
//! same predicate the manual path uses, which is what makes the two agree by
//! construction rather than by review.
//!
//! **Default OFF**, in the same spirit as `loops.enabled` (MAIN-239) and for
//! the same reason: the failure of "off by default" is a review that waits
//! until somebody notices, and the failure of "on by default" is a fleet of
//! agents reviewing repositories nobody asked them to. Read at runtime on every
//! tick, so flipping it lands within one interval with no restart.
//!
//! **Why it is safe to leave on (AC-4).** The sweep is idempotent by
//! construction: it enqueues only where the dedupe says nothing is in flight,
//! and `queued`, `claimed`, `running` and `waiting_on_human` all count as in
//! flight. So a board that does not change produces no new jobs however often
//! this runs, and a job that is already running is never re-enqueued. Nothing
//! here is time-based, so there is no window in which a second job slips
//! through — the guarantee is the dedupe's, not the interval's.

use std::time::Duration;

use nook_types::{TenantId, UserId, WorkspaceId};

use crate::error::ApiResult;
use crate::state::AppState;

/// The settings key. Tenant-scoped rows only — a `user`-scoped row of the same
/// name is somebody's personal preference and must never gate the fleet, the
/// same rule `loops.enabled` states.
pub const KEY: &str = "reviews.sweep.enabled";

/// How often to scan when nothing overrides it. A review is raised off a human
/// moving a card, so a minute is the right order — responsive enough to feel
/// live, cheap enough that an idle board costs one indexed query per tick.
/// Operators retune it with `NOOK_REVIEW_SWEEP_INTERVAL_SECS`.
const DEFAULT_SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// Is the sweep enabled for this tenant? Absent → `false`.
///
/// Fails **closed** on a database error, exactly like `loops::enabled`: a
/// transient blip must not start raising jobs an operator has said no to.
pub async fn enabled(
    settings: &dyn crate::repo::admin::SettingRepository,
    tenant: TenantId,
) -> bool {
    let raw = settings.tenant_value(tenant, KEY).await.unwrap_or(None);
    crate::services::loops::truthy(raw.as_ref())
}

/// Is ANY tenant sweeping? The cheap gate before doing a pass at all.
pub async fn any_enabled(settings: &dyn crate::repo::admin::SettingRepository) -> bool {
    settings
        .tenant_values_everywhere(KEY)
        .await
        .unwrap_or_default()
        .iter()
        .any(|v| crate::services::loops::truthy(Some(v)))
}

/// Turn the sweep on or off for a tenant. Returns the value now stored.
pub async fn set(
    settings: &dyn crate::repo::admin::SettingRepository,
    tenant: TenantId,
    on: bool,
) -> ApiResult<bool> {
    settings
        .put(crate::repo::admin::SettingWrite {
            tenant,
            scope: "tenant".to_string(),
            user: None,
            key: KEY.to_string(),
            value: serde_json::Value::Bool(on),
        })
        .await?;
    Ok(on)
}

/// Spawn the sweep. Fire-and-forget; the process exits on shutdown, taking the
/// task with it.
pub fn start(state: AppState) {
    tokio::spawn(async move {
        let interval = match state.cfg.review_sweep_interval_secs {
            0 => DEFAULT_SWEEP_INTERVAL,
            n => Duration::from_secs(n),
        };
        tracing::info!(
            interval_secs = interval.as_secs(),
            "review sweep started (gated on `{KEY}`, default off)"
        );
        run(state, interval).await;
    });
}

async fn run(state: AppState, interval: Duration) {
    let mut switch = SwitchLog::default();
    loop {
        tokio::time::sleep(interval).await;
        if !switch.observe(any_enabled(&*state.settings).await) {
            continue;
        }
        match sweep(&state).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(enqueued = n, "review sweep raised review jobs"),
            Err(e) => tracing::warn!(error = %e, "review sweep failed"),
        }
    }
}

/// One pass: enqueue a review for every signalling workspace whose tenant has
/// the sweep on and which has no review in flight. Returns how many were
/// actually raised — deduped workspaces are not counted, which is what makes
/// "no growth when nothing changes" observable as a zero.
///
/// `pub` so a test can run exactly one pass rather than racing the poller.
pub async fn sweep(state: &AppState) -> ApiResult<u64> {
    let signalling = state.tasks.workspaces_with_cards_in_review().await?;
    let mut raised = 0u64;
    // Per-tenant, so one tenant's setting can never raise another's jobs. The
    // cross-tenant `any_enabled` above is only a "do a pass at all" gate.
    let mut allowed: std::collections::HashMap<TenantId, bool> = std::collections::HashMap::new();
    for (tenant, workspace) in signalling {
        let on = match allowed.get(&tenant) {
            Some(v) => *v,
            None => {
                let v = enabled(&*state.settings, tenant).await;
                allowed.insert(tenant, v);
                v
            }
        };
        if !on {
            continue;
        }
        if enqueue_for(state, tenant, workspace).await? {
            raised += 1;
        }
    }
    Ok(raised)
}

/// Raise one workspace's review, if the shared dedupe lets it. `false` means
/// already covered.
async fn enqueue_for(
    state: &AppState,
    tenant: TenantId,
    workspace: WorkspaceId,
) -> ApiResult<bool> {
    // The sweep has no human requester, so the job is attributed to the
    // workspace's tenant owner — the same identity `nook operator` acts as.
    // Without one there is nobody to attribute the run to, and a job with a
    // dangling `requested_by` cannot mint an executor token; skip rather than
    // invent an id.
    let Some(requested_by) = sweep_requester(state, tenant).await? else {
        tracing::warn!(
            tenant = %tenant.0,
            "review sweep: tenant has no owner to attribute a job to — skipped"
        );
        return Ok(false);
    };
    Ok(
        crate::services::jobs::enqueue_review(state, tenant, requested_by, workspace, None)
            .await?
            .is_some(),
    )
}

/// Who a sweep-raised job is attributed to: the tenant's owner. Deliberately
/// not "whoever moved the card" — the sweep reads a board state, not an action,
/// and there is no actor to read.
async fn sweep_requester(state: &AppState, tenant: TenantId) -> ApiResult<Option<UserId>> {
    Ok(state
        .identity
        .tenant_owner_user_id(tenant.0)
        .await?
        .map(UserId))
}

/// Log the switch only when it CHANGES, so a per-minute poll does not fill the
/// log forever. Same shape as `loops::SwitchLog`, with this sweep's own message
/// naming its own setting — a shared logger would have to say "loops", which is
/// the wrong switch to tell an operator to flip.
#[derive(Default)]
struct SwitchLog {
    last: Option<bool>,
}

impl SwitchLog {
    fn observe(&mut self, on: bool) -> bool {
        if self.last != Some(on) {
            if on {
                tracing::info!("review sweep enabled — resuming");
            } else {
                tracing::info!(
                    "review sweep disabled — idle. Enable with `nook reviews sweep on`."
                );
            }
            self.last = Some(on);
        }
        on
    }
}
