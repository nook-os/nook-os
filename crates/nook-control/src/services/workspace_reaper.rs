//! The tombstoned-checkout reaper (MAIN-220): a CP-side background scan that
//! hard-deletes `node_workspaces` rows that have been marked missing
//! (`missing_at IS NOT NULL`) for longer than the configured retention window.
//!
//! Reconcile no longer deletes a checkout the moment a node stops reporting it —
//! it tombstones the row so a transient unmount or a moved directory can heal
//! with its id intact (MAIN-220). This is the counterpart that finally reclaims
//! rows whose absence has lasted long enough to be real, and it is the ONLY
//! background path that hard-deletes a checkout row. Every deletion that still
//! has a task pointing at its on-disk path is logged loudly, so a reclaimed
//! checkout is never silent.
//!
//! Like the loop-job reaper, the scan is a plain conditional delete safe to run
//! on every replica: a row is deleted by exactly one of them and the rest see
//! zero rows.

use std::time::Duration;

use nook_db::{params, Db, Postgres, TimeMath};
use nook_types::{NodeId, NodeWorkspaceId};

use crate::error::ApiResult;
use crate::state::AppState;

/// How often to scan. The retention window is measured in days, so an hourly
/// scan reclaims an aged-out row within about an hour of it expiring — far finer
/// than the window itself, and cheap thanks to the partial index over just the
/// tombstoned rows.
const SCAN_INTERVAL: Duration = Duration::from_secs(3600);

/// Spawn the reaper. Fire-and-forget; the process exits on shutdown, taking the
/// task with it.
pub fn start(state: AppState) {
    tokio::spawn(async move {
        let retention = state.cfg.workspace_missing_retention_secs;
        tracing::info!(
            retention_secs = retention,
            "tombstoned-checkout reaper started"
        );
        run(state, retention).await;
    });
}

async fn run(state: AppState, retention_secs: u64) {
    loop {
        tokio::time::sleep(SCAN_INTERVAL).await;
        match reap_missing_checkouts(&state, retention_secs).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(reaped = n, "reaped tombstoned checkouts past retention"),
            Err(e) => tracing::warn!(error = %e, "tombstoned-checkout reaper scan failed"),
        }
    }
}

/// Hard-delete every checkout tombstoned longer than `retention_secs`, logging
/// any task that still referenced each deleted row's on-disk path. Returns how
/// many rows were reclaimed. Idempotent and multi-instance safe.
pub async fn reap_missing_checkouts(state: &AppState, retention_secs: u64) -> ApiResult<u64> {
    // Delete-and-return in one statement: race-free (a row healed between a
    // separate select and delete could otherwise be wrongly reclaimed) and it
    // still hands back node_id + path so the reference log can be built after
    // the row is gone — tasks reference the path directly, not by FK.
    let reaped: Vec<(NodeWorkspaceId, NodeId, String)> = state
        .db
        .query_all(
            &format!(
                "DELETE FROM node_workspaces
                 WHERE missing_at IS NOT NULL AND missing_at < {}
                 RETURNING id, node_id, path",
                Postgres.now_minus_scaled("$1", "1 second")
            ),
            params![retention_secs as i64],
        )
        .await?;

    for (nw_id, node_id, path) in &reaped {
        // Match tasks the same way `migrate_paths` does — node + worktree_path,
        // the only durable on-disk reference. A reclaimed checkout that still
        // has task worktrees pointing at it is a warning, not a silent drop.
        let refs: Vec<(Option<String>, Option<i32>)> = state
            .db
            .query_all(
                "SELECT b.key, t.number
                   FROM tasks t JOIN boards b ON b.id = t.board_id
                  WHERE t.worktree_node_id = $1 AND t.worktree_path = $2",
                params![node_id, path],
            )
            .await
            .unwrap_or_default();
        let task_keys: Vec<String> = refs
            .into_iter()
            .map(|(key, number)| format!("{}-{}", key.unwrap_or_default(), number.unwrap_or(0)))
            .collect();
        if task_keys.is_empty() {
            tracing::info!(%node_id, %nw_id, %path, "reclaimed tombstoned checkout past retention");
        } else {
            tracing::warn!(
                %node_id, %nw_id, %path, tasks = ?task_keys,
                "reclaimed tombstoned checkout past retention — it still had task worktree references"
            );
        }
    }
    Ok(reaped.len() as u64)
}
