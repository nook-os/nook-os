//! Converge managed agent RUNS against whatever work exists (MAIN-455).
//!
//! One live headless run per work item, raised only when the item has changed
//! since the last completed run for it. That rule is what replaced the
//! five-minute sweep: a repo nobody has pushed to costs nothing, and a push is
//! answered within a poll interval rather than within the sweep's window.
//!
//! Nothing here knows what a pull request is. The work comes from a
//! [`WorkSource`], so a builder loop lands as a second source and a skill name
//! rather than as a second copy of this file.

use crate::services::work_source::{WorkItem, WorkSource};
use crate::state::AppState;
use nook_types::{TenantId, UserId, WorkspaceId};

/// What the reconciler decided for one workspace, so the caller can log it once
/// rather than this deciding how loud it is.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Converged {
    /// Runs raised this pass.
    pub raised: usize,
    /// Items with work owing that the ceiling would not let us start yet. Not a
    /// failure — the declaration is doing its job — but the number people look
    /// for when reviews feel slow.
    pub withheld: usize,
    /// Items already being run right now.
    pub live: usize,
}

/// Which items are owed a run, given what is already running or finished.
///
/// Split out from the IO so the rule is testable as a function of its inputs —
/// it is the whole of the wakeup policy, and it is the thing most likely to be
/// got subtly wrong.
///
/// An item is owed a run when nothing is live for it AND its fingerprint is not
/// what the last completed run recorded. A never-run item is owed one, because
/// `None` is not equal to a fingerprint.
pub fn owed<'a>(
    items: &'a [WorkItem],
    heads: &[crate::repo::jobs::ReviewRunHeads],
    ceiling: usize,
) -> (Vec<&'a WorkItem>, usize, usize) {
    let live = heads.iter().filter(|h| h.live_head.is_some()).count();
    let mut owed: Vec<&WorkItem> = items
        .iter()
        .filter(|item| {
            let head = heads.iter().find(|h| h.review_pr_number == item.key);
            match head {
                // Already being run: never two runs for one item, whatever the
                // fingerprint says. The new head is picked up when this finishes.
                Some(h) if h.live_head.is_some() => false,
                Some(h) => h.done_head.as_deref() != Some(item.fingerprint.as_str()),
                None => true,
            }
        })
        .collect();

    // Deterministic: two control-plane replicas planning the same instant must
    // raise the same runs, and "whichever the map iterated first" is not that.
    owed.sort_by_key(|i| i.key);

    let room = ceiling.saturating_sub(live);
    let withheld = owed.len().saturating_sub(room);
    owed.truncate(room);
    (owed, withheld, live)
}

/// Raise the runs one workspace is owed.
pub async fn converge(
    state: &AppState,
    source: &dyn WorkSource,
    tenant: TenantId,
    requested_by: UserId,
    workspace: WorkspaceId,
    remote: Option<&str>,
    ceiling: usize,
) -> crate::error::ApiResult<Converged> {
    // `None` is UNKNOWN, never "no work" — an outage must not read as a clean
    // repo. Holding is right: whatever is live keeps running, and nothing new is
    // raised on a guess.
    let Some(items) = source.items(workspace, remote).await else {
        return Ok(Converged::default());
    };
    let heads = state.jobs.review_run_heads(tenant, workspace).await?;
    let (owed, withheld, live) = owed(&items, &heads, ceiling);

    let mut raised = 0;
    for item in owed {
        match crate::services::jobs::raise_run(
            state,
            tenant,
            requested_by,
            workspace,
            source.kind(),
            item,
        )
        .await
        {
            Ok(Some(_)) => raised += 1,
            // Lost the race to another replica: the unique index refused the
            // second row, which is exactly what it is for.
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(%workspace, item = %item.label, error = %e, "could not raise run")
            }
        }
    }
    Ok(Converged {
        raised,
        withheld,
        live,
    })
}
