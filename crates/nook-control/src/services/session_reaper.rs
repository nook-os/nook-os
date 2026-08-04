//! The terminated-session reaper: a background scan that deletes `sessions`
//! rows which ended longer ago than the tenant's retention window.
//!
//! Nothing ever removed them. A prod tenant reached 58 session rows with 7 live,
//! and every `nook get sessions` listed all 58 — the dead ones outnumbering the
//! useful ones eight to one, and growing with every reconcile.
//!
//! **kubectl's shape, minus the reason to keep them.** A terminated pod stays
//! listed because `kubectl logs` still works on it — the kubelet holds the
//! container's output, so the row has residual value, and the controller
//! manager reaps it at a threshold rather than immediately. A terminated nook
//! session has no such value: tmux IS the buffer of record and it died with the
//! process, so the row is a name and a timestamp for something nobody can ever
//! read again.
//!
//! **`exited` and `error` only.** `detached` looks dead and is not — tmux is
//! still holding it and a browser can reattach, which is the whole point of the
//! detach/attach split. Reaping those would destroy live work that merely had
//! nobody watching it.
//!
//! Deleting a session row is safe by construction, and the schema already says
//! so: `events`, `feedback` and `tasks` reference it `ON DELETE SET NULL`, so
//! history survives with its pointer cleared, while `session_port_leases` is
//! `ON DELETE CASCADE` — so reaping a session also hands its ports back, which
//! the lazy reclaim in the allocator otherwise only did when somebody needed one.
//!
//! Like the other reapers, the scan is a plain conditional delete safe to run on
//! every replica: a row is deleted by exactly one of them and the rest see zero.

use std::time::Duration;

use crate::error::ApiResult;
use crate::state::AppState;

/// The settings key. Tenant-scoped, beside `loops.enabled`, so an operator sets
/// it per org rather than per deployment — the retention somebody wants for a
/// busy shared tenant is not the one they want for a personal one.
pub const KEY: &str = "sessions.retention_days";

/// What a tenant that has never set it gets. A week: long enough that "what was
/// I running on Monday" still answers on Friday, short enough that a reconciling
/// workspace does not accumulate a year of corpses.
pub const DEFAULT_RETENTION_DAYS: i64 = 7;

/// Nothing shorter is honoured. A retention of zero would delete a session the
/// instant it exits, which reads as sessions vanishing mid-glance — and one
/// minute is already far below any window a person would set on purpose.
const MIN_RETENTION_DAYS: i64 = 1;

/// How often to scan. The window is measured in days, so hourly reclaims an
/// aged-out row within an hour of it expiring — much finer than the window, and
/// cheap: the delete is indexed on status and `ended_at`.
const SCAN_INTERVAL: Duration = Duration::from_secs(3600);

/// The retention window for a tenant, in days. Absent or unusable → the default.
///
/// Fails **safe**, and the direction matters: a database blip or a garbled value
/// must not read as "retain nothing" and delete the fleet's history. Anything
/// this cannot make sense of keeps the default, and says so once.
pub async fn retention_days(
    settings: &dyn crate::repo::admin::SettingRepository,
    tenant: nook_types::TenantId,
) -> i64 {
    let raw = settings.tenant_value(tenant, KEY).await.unwrap_or(None);
    let Some(v) = raw else {
        return DEFAULT_RETENTION_DAYS;
    };
    // A number, or a string holding one: the UI sends a number, a hand-written
    // setting or a curl is as likely to send "14".
    let parsed = v
        .as_i64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<i64>().ok()));
    match parsed {
        Some(d) if d >= MIN_RETENTION_DAYS => d,
        other => {
            tracing::warn!(
                %tenant,
                value = %v,
                parsed = ?other,
                default = DEFAULT_RETENTION_DAYS,
                "unusable {KEY} — keeping the default rather than reaping to it"
            );
            DEFAULT_RETENTION_DAYS
        }
    }
}

/// Spawn the reaper. Fire-and-forget; the process exits on shutdown, taking the
/// task with it.
pub fn start(state: AppState) {
    tokio::spawn(async move {
        tracing::info!(
            default_retention_days = DEFAULT_RETENTION_DAYS,
            "terminated-session reaper started"
        );
        run(state).await;
    });
}

async fn run(state: AppState) {
    loop {
        tokio::time::sleep(SCAN_INTERVAL).await;
        match reap_terminated(&state).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(reaped = n, "reaped terminated sessions past retention"),
            Err(e) => tracing::warn!(error = %e, "terminated-session reaper scan failed"),
        }
    }
}

/// Delete every `exited`/`error` session whose tenant's window has passed.
///
/// Per tenant, because the window is per tenant — one sweep with one number
/// would quietly apply somebody else's policy. Deliberately NOT gated on
/// `loops.enabled`, unlike the checkout reaper: sessions accumulate from ordinary
/// human use as much as from loops, so an operator who turned loops off would
/// otherwise keep collecting rows forever with no way to stop it.
pub async fn reap_terminated(state: &AppState) -> ApiResult<u64> {
    let tenants = state.sessions.tenants_with_terminated().await?;
    let mut total = 0u64;
    for tenant in tenants {
        let days = retention_days(&*state.settings, tenant).await;
        match state.sessions.reap_terminated(tenant, days).await {
            Ok(0) => {}
            Ok(n) => {
                total += n;
                tracing::info!(%tenant, reaped = n, retention_days = days, "reaped sessions");
            }
            // One tenant's failure must not strand the others — the next hour
            // retries it, and the rest are already reclaimed.
            Err(e) => tracing::warn!(%tenant, error = %e, "could not reap this tenant's sessions"),
        }
    }
    Ok(total)
}
