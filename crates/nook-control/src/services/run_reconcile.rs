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
/// How long a failed run holds its item back.
///
/// A failure must not count as a review — one bad run would silence a pull
/// request until somebody happened to push again. But retrying it on the next
/// pass is a run every ten seconds forever, which is what the first end-to-end
/// produced: fifteen identical failures in two and a half minutes, each one a
/// clone attempt on a shared machine.
///
/// So a failure is held, not forgotten. Long enough that a broken repo is not a
/// hot loop, short enough that a transient fault heals without anyone watching.
pub const FAILURE_BACKOFF: chrono::Duration = chrono::Duration::minutes(5);

pub fn owed<'a>(
    items: &'a [WorkItem],
    heads: &[crate::repo::jobs::ReviewRunHeads],
    ceiling: usize,
    now: chrono::DateTime<chrono::Utc>,
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
                Some(h) => {
                    // Reviewed at this exact head: nothing owing until it moves.
                    if h.done_head.as_deref() == Some(item.fingerprint.as_str()) {
                        return false;
                    }
                    // Failed at this exact head, recently: hold. A push changes
                    // the fingerprint and clears the hold by itself, so a real
                    // fix is never waiting on a timer.
                    match (h.failed_head.as_deref(), h.failed_at) {
                        (Some(f), Some(at))
                            if f == item.fingerprint && now - at < FAILURE_BACKOFF =>
                        {
                            false
                        }
                        _ => true,
                    }
                }
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
    let (owed, withheld, live) = owed(&items, &heads, ceiling, chrono::Utc::now());

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::jobs::ReviewRunHeads;

    fn item(key: i64, fingerprint: &str) -> WorkItem {
        WorkItem {
            key,
            fingerprint: fingerprint.to_string(),
            label: format!("PR #{key}"),
        }
    }

    fn heads(key: i64) -> ReviewRunHeads {
        ReviewRunHeads {
            review_pr_number: key,
            live_head: None,
            done_head: None,
            failed_head: None,
            failed_at: None,
        }
    }

    fn now() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp(1_800_000_000, 0).expect("a fixed instant")
    }

    #[test]
    fn a_pull_request_nobody_has_run_is_owed_a_run() {
        let items = [item(341, "aaa")];
        let (owed, withheld, live) = owed(&items, &[], 2, now());
        assert_eq!(owed.len(), 1);
        assert_eq!((withheld, live), (0, 0));
    }

    /// The rule that replaced the sweep. A repo nobody has pushed to costs
    /// nothing at all — which is the whole reason there is no timer.
    #[test]
    fn a_head_that_has_not_moved_is_owed_nothing() {
        let items = [item(341, "aaa")];
        let done = ReviewRunHeads {
            done_head: Some("aaa".into()),
            ..heads(341)
        };
        assert!(owed(&items, &[done], 2, now()).0.is_empty());
    }

    #[test]
    fn a_push_earns_a_fresh_run() {
        let items = [item(341, "bbb")];
        let done = ReviewRunHeads {
            done_head: Some("aaa".into()),
            ..heads(341)
        };
        assert_eq!(owed(&items, &[done], 2, now()).0.len(), 1);
    }

    /// Never two runs for one pull request, whatever the fingerprint says. The
    /// new head is picked up when the live one finishes.
    #[test]
    fn a_live_run_blocks_a_second_one_even_after_a_push() {
        let items = [item(341, "bbb")];
        let live = ReviewRunHeads {
            live_head: Some("aaa".into()),
            ..heads(341)
        };
        assert!(owed(&items, &[live], 2, now()).0.is_empty());
    }

    /// The defect the first end-to-end found: a failed run re-raised on every
    /// pass, fifteen times in two and a half minutes.
    #[test]
    fn a_recent_failure_at_this_head_is_held_rather_than_retried() {
        let items = [item(341, "aaa")];
        let failed = ReviewRunHeads {
            failed_head: Some("aaa".into()),
            failed_at: Some(now() - chrono::Duration::seconds(30)),
            ..heads(341)
        };
        assert!(owed(&items, &[failed], 2, now()).0.is_empty());
    }

    #[test]
    fn the_hold_expires_so_a_transient_fault_heals_itself() {
        let items = [item(341, "aaa")];
        let failed = ReviewRunHeads {
            failed_head: Some("aaa".into()),
            failed_at: Some(now() - FAILURE_BACKOFF - chrono::Duration::seconds(1)),
            ..heads(341)
        };
        assert_eq!(owed(&items, &[failed], 2, now()).0.len(), 1);
    }

    /// A fix is never waiting on a timer: the push changes the fingerprint, so
    /// the hold does not apply to it at all.
    #[test]
    fn a_push_clears_a_failure_hold_immediately() {
        let items = [item(341, "bbb")];
        let failed = ReviewRunHeads {
            failed_head: Some("aaa".into()),
            failed_at: Some(now()),
            ..heads(341)
        };
        assert_eq!(owed(&items, &[failed], 2, now()).0.len(), 1);
    }

    /// A failure is not a review. Holding it must never look like "done", or a
    /// PR that failed once would go unreviewed until somebody pushed.
    #[test]
    fn a_failure_does_not_count_as_having_reviewed_the_head() {
        let items = [item(341, "aaa")];
        let failed = ReviewRunHeads {
            failed_head: Some("aaa".into()),
            failed_at: Some(now() - FAILURE_BACKOFF * 2),
            ..heads(341)
        };
        assert_eq!(
            owed(&items, &[failed], 2, now()).0.len(),
            1,
            "once the hold expires the PR is owed a run again"
        );
    }

    /// The declaration still rules (AC-6): the ceiling caps how many run at
    /// once, and the rest are reported rather than dropped.
    #[test]
    fn the_ceiling_caps_concurrency_and_the_remainder_is_reported() {
        let items = [item(1, "a"), item(2, "b"), item(3, "c"), item(4, "d")];
        let (owed, withheld, live) = owed(&items, &[], 2, now());
        assert_eq!(owed.len(), 2);
        assert_eq!(withheld, 2);
        assert_eq!(live, 0);
    }

    #[test]
    fn a_live_run_uses_up_a_slot_in_the_ceiling() {
        let items = [item(1, "a"), item(2, "b")];
        let live = ReviewRunHeads {
            live_head: Some("a".into()),
            ..heads(1)
        };
        let (owed, _, live_count) = owed(&items, &[live], 2, now());
        assert_eq!(live_count, 1);
        assert_eq!(owed.len(), 1, "one slot left, so only one more is raised");
    }

    /// Two control-plane replicas planning the same instant must raise the same
    /// runs. "Whichever the map iterated first" is not that.
    #[test]
    fn the_choice_is_deterministic_whatever_order_the_items_arrive_in() {
        let a = [item(9, "a"), item(3, "b"), item(7, "c")];
        let b = [item(7, "c"), item(9, "a"), item(3, "b")];
        let keys = |items: &[WorkItem]| -> Vec<i64> {
            owed(items, &[], 2, now()).0.iter().map(|i| i.key).collect()
        };
        assert_eq!(keys(&a), keys(&b));
        assert_eq!(keys(&a), vec![3, 7]);
    }
}
