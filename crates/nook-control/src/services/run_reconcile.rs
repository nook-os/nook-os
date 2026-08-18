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
#[derive(Debug, Default)]
pub struct Converged {
    /// The runs raised this pass, one per owed item.
    pub jobs: Vec<nook_types::LoopJob>,
    /// `jobs.len()`, kept for the log line.
    pub raised: usize,
    /// Items with work owing that the ceiling would not let us start yet. Not a
    /// failure — the declaration is doing its job — but the number people look
    /// for when reviews feel slow.
    pub withheld: usize,
    /// Items already being run right now.
    pub live: usize,
}

/// How long a run that concluded nothing holds its item back — a failure, or a
/// zero-exit pass with no verdict (checks pending, environment broken).
///
/// Neither may count as a review — one bad run would silence a pull request
/// until somebody happened to push again. But retrying on the next pass is a
/// run every ten seconds, which is what the first end-to-end produced: fifteen
/// identical failures in two and a half minutes, each one a clone attempt on a
/// shared machine. Held, not forgotten: long enough that a broken repo is not
/// a hot loop, short enough that a transient fault heals unwatched.
///
/// This is the FIRST hold, not the only one: a BUILD item that keeps failing
/// doubles it up to an hour (MAIN-386 AC-2), because five minutes forever is
/// a ticket that cannot build spending a night's quota proving it. A review
/// item carries no streak, so this stays its flat window.
pub const FAILURE_BACKOFF: chrono::Duration = crate::services::build_ladder::FIRST_BACKOFF;

/// Which items are owed a run, given what already ran or is running.
///
/// Split out from the IO so the rule is testable as a function of its inputs —
/// it is the whole of the wakeup policy, and it is the thing most likely to be
/// got subtly wrong.
///
/// An item is owed a run when nothing is live for it, its fingerprint is not
/// what the last VERDICTED run recorded (a never-run item qualifies, because
/// `None` equals no fingerprint), and it is not inside a concluded-nothing
/// hold — see [`FAILURE_BACKOFF`], whose length grows with the item's run of
/// failures. A push changes the fingerprint and clears the hold immediately,
/// so a real fix never waits on the timer.
///
/// There are two exceptions to the fingerprint rule, and both are facts about
/// the item that no fingerprint of it can hold: a human's ruling ([`overruled`])
/// and a pull request that conflicts with its base branch ([`rebase_owed`]).
pub fn owed<'a>(
    items: &'a [WorkItem],
    heads: &[crate::repo::jobs::RunHeads],
    ceiling: usize,
    now: chrono::DateTime<chrono::Utc>,
) -> (Vec<&'a WorkItem>, usize, usize) {
    let live = heads.iter().filter(|h| h.live_head.is_some()).count();
    let mut owed: Vec<&WorkItem> = items
        .iter()
        .filter(|item| {
            let head = heads.iter().find(|h| h.item_key == item.key);
            match head {
                // Already being run: never two runs for one item, whatever the
                // fingerprint says. The new head is picked up when this finishes.
                Some(h) if h.live_head.is_some() => false,
                Some(h) => {
                    // Reviewed at this exact head: nothing owing until it moves
                    // — unless a human has ruled since, which no fingerprint
                    // can express.
                    if h.done_head.as_deref() == Some(item.fingerprint.as_str())
                        && !overruled(item, h)
                        && !rebase_owed(item, h, now)
                    {
                        return false;
                    }
                    // Attempted at this exact head recently — failed, or ended
                    // without a verdict (checks pending, environment broken):
                    // hold. A push changes the fingerprint and clears the hold
                    // by itself, so a real fix is never waiting on a timer.
                    let hold = crate::services::build_ladder::backoff_for(h.failure_streak);
                    !matches!(
                        (h.attempted_head.as_deref(), h.attempted_at),
                        (Some(f), Some(at)) if f == item.fingerprint && now - at < hold
                    )
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

/// Has a human overruled the run that concluded this item (MAIN-584 AC-2)?
///
/// A `blocked` outcome IS a recorded outcome, so a card the loop handed back
/// carries a `done_head` equal to its own fingerprint — and the fingerprint is
/// title and description only, which no ruling moves. Comment, label and
/// `agent-ready` all change, and the card is still never picked. The stamp is
/// what says the last conclusion is spent; comparing it against the run's own
/// time is what stops the stamp disabling the dedupe for that card forever,
/// because the next run to conclude concludes after it.
fn overruled(item: &WorkItem, heads: &crate::repo::jobs::RunHeads) -> bool {
    match (item.unblocked_at, heads.done_at) {
        (Some(unblocked), Some(done)) => done < unblocked,
        // No stamp is the ordinary case; a `done_head` with no time is a row
        // written before the column existed, and re-running every one of those
        // on sight would be a stampede at deploy.
        _ => false,
    }
}

/// Is this item's pull request conflicting with its base branch RIGHT NOW
/// (MAIN-627 AC-1)?
///
/// A base-branch move changes nothing an item is fingerprinted on — not the
/// card's content, not the head a repair was rejected at — so the dedupe above
/// says the last run still speaks for the item, and it does not: it concluded
/// against a base that has since moved. Left there, a pull request that goes
/// conflicting while it waits is owed nothing by anyone, forever, and only a
/// hand rebase ends it. Which is most pull requests, on a board landing several
/// a day.
///
/// PACED by the same window a failed attempt is held for, and for the same
/// reason (AC-6). A repair that concluded and left the conflict standing
/// answered nothing, and the conflict is still true one second later — so
/// without a hold this is a run every sweep, forever, which is the hot loop
/// [`FAILURE_BACKOFF`] exists to stop. The window is the ladder's, so a card
/// whose rebases keep failing backs off and escalates exactly as any other
/// build does.
///
/// A `done_head` with no `done_at` is a row written before the column existed
/// (`overruled` meets the same shape). It is read as owed here rather than as
/// held, because the population is bounded by "conflicting right now" — a
/// handful of pull requests — where a ruling's is every card the loop ever
/// concluded.
fn rebase_owed(
    item: &WorkItem,
    heads: &crate::repo::jobs::RunHeads,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    if item.conflict_base.is_none() {
        return false;
    }
    let hold = crate::services::build_ladder::backoff_for(heads.failure_streak);
    heads.done_at.is_none_or(|at| now - at >= hold)
}

/// Raise the runs one workspace is owed.
#[allow(clippy::too_many_arguments)]
pub async fn converge(
    state: &AppState,
    source: &dyn WorkSource,
    tenant: TenantId,
    requested_by: UserId,
    workspace: WorkspaceId,
    remote: Option<&str>,
    ceiling: usize,
    // A human's brief for the runs raised by THIS call (the manual path);
    // `None` on the reconciler's own passes.
    note: Option<&str>,
) -> crate::error::ApiResult<Converged> {
    // `None` is UNKNOWN, never "no work" — an outage must not read as a clean
    // repo. Holding is right: whatever is live keeps running, and nothing new is
    // raised on a guess.
    let Some(items) = source.items(workspace, remote).await else {
        return Ok(Converged::default());
    };
    let heads = state.jobs.review_run_heads(tenant, workspace).await?;
    let (owed, withheld, live) = owed(&items, &heads, ceiling, chrono::Utc::now());

    let mut jobs = Vec::new();
    for item in owed {
        match crate::services::jobs::raise_run(
            state,
            tenant,
            requested_by,
            workspace,
            source.kind(),
            item,
            note,
            false,
        )
        .await
        {
            Ok(Some(job)) => jobs.push(job),
            // Lost the race to another replica: the unique index refused the
            // second row, which is exactly what it is for.
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(%workspace, item = %item.label, error = %e, "could not raise run")
            }
        }
    }
    Ok(Converged {
        raised: jobs.len(),
        jobs,
        withheld,
        live,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::jobs::RunHeads;

    fn item(key: i64, fingerprint: &str) -> WorkItem {
        WorkItem {
            key,
            fingerprint: fingerprint.to_string(),
            label: format!("PR #{key}"),
            target_task_id: None,
            claim_first: false,
            unblocked_at: None,
            conflict_base: None,
        }
    }

    /// The same item, with its pull request conflicting against `main`.
    fn conflicting(item: WorkItem) -> WorkItem {
        WorkItem {
            conflict_base: Some("main".into()),
            ..item
        }
    }

    fn heads(key: i64) -> RunHeads {
        RunHeads {
            item_key: key,
            ..Default::default()
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
        let done = RunHeads {
            done_head: Some("aaa".into()),
            ..heads(341)
        };
        assert!(owed(&items, &[done], 2, now()).0.is_empty());
    }

    /// MAIN-584 AC-2, and the defect the whole card turns on: a `blocked`
    /// outcome records a `done_head`, and the fingerprint is title+description
    /// — so a ruling on the card moves nothing the dedupe reads, and the card
    /// stays unpickable however many labels come off it.
    #[test]
    fn a_human_ruling_defeats_the_dedupe_at_an_unchanged_fingerprint() {
        let done = RunHeads {
            done_head: Some("aaa".into()),
            done_at: Some(now() - chrono::Duration::hours(1)),
            ..heads(341)
        };
        let blocked = [item(341, "aaa")];
        assert!(
            owed(&blocked, std::slice::from_ref(&done), 2, now())
                .0
                .is_empty(),
            "the card the loop handed back is not owed a run on its own"
        );

        let ruled = [WorkItem {
            unblocked_at: Some(now() - chrono::Duration::minutes(1)),
            ..item(341, "aaa")
        }];
        assert_eq!(
            owed(&ruled, &[done], 2, now()).0.len(),
            1,
            "a ruling re-arms it with the fingerprint still identical"
        );
    }

    /// So the stamp does not permanently disable the dedupe for that card: the
    /// run the ruling asked for concludes after it, and quiets the card again.
    #[test]
    fn a_run_that_concluded_after_the_ruling_suppresses_the_item_again() {
        let items = [WorkItem {
            unblocked_at: Some(now() - chrono::Duration::hours(1)),
            ..item(341, "aaa")
        }];
        let done = RunHeads {
            done_head: Some("aaa".into()),
            done_at: Some(now() - chrono::Duration::minutes(1)),
            ..heads(341)
        };
        assert!(owed(&items, &[done], 2, now()).0.is_empty());
    }

    /// MAIN-627 AC-1, and the whole card. A conflicting pull request is owed a
    /// run at a fingerprint nothing has moved — because nothing CAN move it: a
    /// base-branch merge is not a change to the card's content and not a change
    /// to the head the repair was rejected at. Observed on MAIN-331 / PR #493,
    /// stranded two days with an approved review and no command able to force
    /// it.
    #[test]
    fn a_conflicting_pull_request_is_owed_a_run_at_an_unchanged_fingerprint() {
        let done = RunHeads {
            done_head: Some("aaa".into()),
            done_at: Some(now() - FAILURE_BACKOFF - chrono::Duration::seconds(1)),
            ..heads(341)
        };
        let clean = [item(341, "aaa")];
        assert!(
            owed(&clean, std::slice::from_ref(&done), 2, now())
                .0
                .is_empty(),
            "AC-3/NG-3: a clean pull request at an unchanged fingerprint is still owed nothing"
        );
        assert_eq!(
            owed(&[conflicting(item(341, "aaa"))], &[done], 2, now())
                .0
                .len(),
            1,
            "a conflict defeats the dedupe with the fingerprint identical"
        );
    }

    /// AC-3: convergence must not become a loop. The repair lands, the conflict
    /// clears, and the very next sweep is owed nothing again — with no push, no
    /// review and no human in between.
    #[test]
    fn a_repaired_conflict_is_not_re_raised() {
        let items = [item(341, "aaa")];
        let done = RunHeads {
            done_head: Some("aaa".into()),
            done_at: Some(now() - chrono::Duration::hours(4)),
            ..heads(341)
        };
        assert!(owed(&items, &[done], 2, now()).0.is_empty());
    }

    /// A repair that concluded and left the conflict standing is a run that
    /// answered nothing, and the conflict is still true a second later — so the
    /// re-raise waits out the same window a failed attempt does, instead of
    /// firing every sweep forever.
    #[test]
    fn a_conflict_re_raise_waits_out_the_hold() {
        let items = [conflicting(item(341, "aaa"))];
        let fresh = RunHeads {
            done_head: Some("aaa".into()),
            done_at: Some(now() - chrono::Duration::minutes(1)),
            ..heads(341)
        };
        assert!(
            owed(&items, std::slice::from_ref(&fresh), 2, now())
                .0
                .is_empty(),
            "held inside the window"
        );
        let expired = RunHeads {
            done_at: Some(now() - FAILURE_BACKOFF - chrono::Duration::seconds(1)),
            ..fresh
        };
        assert_eq!(
            owed(&items, &[expired], 2, now()).0.len(),
            1,
            "owed after it"
        );
    }

    /// AC-6: the ladder applies to a conflict repair exactly as to any other
    /// build. Each failure widens the hold, and the third takes the card out of
    /// auto-fire altogether — `needs-human-review` excludes it from the repair
    /// lane's own query, so a rebase that cannot be made to work reaches a
    /// person instead of retrying all night.
    #[test]
    fn consecutive_failed_conflict_repairs_climb_the_ladder() {
        let items = [conflicting(item(341, "aaa"))];
        for (streak, held_at) in [(1, 4), (2, 9), (3, 19)] {
            let failed = RunHeads {
                attempted_head: Some("aaa".into()),
                attempted_at: Some(now() - chrono::Duration::minutes(held_at)),
                failure_streak: streak,
                ..heads(341)
            };
            assert!(
                owed(&items, std::slice::from_ref(&failed), 2, now())
                    .0
                    .is_empty(),
                "a conflict does not shorten the hold after {streak} failure(s)"
            );
            let expired = RunHeads {
                attempted_at: Some(now() - crate::services::build_ladder::backoff_for(streak)),
                ..failed
            };
            assert_eq!(
                owed(&items, &[expired], 2, now()).0.len(),
                1,
                "and the hold does expire"
            );
        }
    }

    /// A conflict is not a licence to run two at once — the live rule outranks
    /// it, as it outranks a push.
    #[test]
    fn a_live_run_still_blocks_a_conflicting_item() {
        let items = [conflicting(item(341, "aaa"))];
        let live = RunHeads {
            live_head: Some("aaa".into()),
            ..heads(341)
        };
        assert!(owed(&items, &[live], 2, now()).0.is_empty());
    }

    #[test]
    fn a_push_earns_a_fresh_run() {
        let items = [item(341, "bbb")];
        let done = RunHeads {
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
        let live = RunHeads {
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
        let failed = RunHeads {
            attempted_head: Some("aaa".into()),
            attempted_at: Some(now() - chrono::Duration::seconds(30)),
            ..heads(341)
        };
        assert!(owed(&items, &[failed], 2, now()).0.is_empty());
    }

    #[test]
    fn the_hold_expires_so_a_transient_fault_heals_itself() {
        let items = [item(341, "aaa")];
        let failed = RunHeads {
            attempted_head: Some("aaa".into()),
            attempted_at: Some(now() - FAILURE_BACKOFF - chrono::Duration::seconds(1)),
            ..heads(341)
        };
        assert_eq!(owed(&items, &[failed], 2, now()).0.len(), 1);
    }

    /// A fix is never waiting on a timer: the push changes the fingerprint, so
    /// the hold does not apply to it at all.
    #[test]
    fn a_push_clears_a_failure_hold_immediately() {
        let items = [item(341, "bbb")];
        let failed = RunHeads {
            attempted_head: Some("aaa".into()),
            attempted_at: Some(now()),
            ..heads(341)
        };
        assert_eq!(owed(&items, &[failed], 2, now()).0.len(), 1);
    }

    /// The other way to conclude nothing: exit ZERO without a verdict — checks
    /// pending, environment broken, any polite early return. Before the verdict
    /// column, that consumed the head and the PR went silent until the next
    /// push; gating done on the verdict alone would instead retry it every
    /// poll interval for the length of a CI cycle. Held, like a failure,
    /// because to the wakeup rule they are the same fact.
    #[test]
    fn a_verdictless_completion_is_held_then_owed_like_a_failure() {
        let items = [item(341, "aaa")];
        let hold = RunHeads {
            attempted_head: Some("aaa".into()),
            attempted_at: Some(now() - chrono::Duration::seconds(30)),
            ..heads(341)
        };
        assert!(
            owed(&items, &[hold], 2, now()).0.is_empty(),
            "held inside the window"
        );
        let expired = RunHeads {
            attempted_head: Some("aaa".into()),
            attempted_at: Some(now() - FAILURE_BACKOFF - chrono::Duration::seconds(1)),
            ..heads(341)
        };
        assert_eq!(
            owed(&items, &[expired], 2, now()).0.len(),
            1,
            "owed after it"
        );
    }

    /// A failure is not a review. Holding it must never look like "done", or a
    /// PR that failed once would go unreviewed until somebody pushed.
    #[test]
    fn a_failure_does_not_count_as_having_reviewed_the_head() {
        let items = [item(341, "aaa")];
        let failed = RunHeads {
            attempted_head: Some("aaa".into()),
            attempted_at: Some(now() - FAILURE_BACKOFF * 2),
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
        let live = RunHeads {
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
