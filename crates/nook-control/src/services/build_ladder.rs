//! What an auto-fired build run's FAILURE costs the card (MAIN-386).
//!
//! A failed build left availability unchanged, so the next sweep fired the
//! same ticket again — forever. A card that cannot build burned Claude quota
//! all night with nobody told. This is the ladder that ends that: back off,
//! hand it back with a brief, then stop.
//!
//! | failures | what happens |
//! |---|---|
//! | 1 | nothing but the hold — [`backoff_for`], 5 min |
//! | 2 | a template comment carrying the failure, and `loop-changes-requested` |
//! | 3 | `needs-human-review`, a final comment naming all three runs, a notification |
//!
//! ## Counted, never stored (AC-1, NG-2)
//!
//! The count is `loop_jobs` read back: the card's run of `failed` build rows
//! since the last one that recorded an outcome, plus the human's own reset —
//! see [`crate::repo::jobs::UNANSWERED_FAILURE`], which is where the exact
//! rule lives. A counter column beside those rows would be a second truth,
//! and MAIN-489's was: it drifted the moment a bump landed and its escalation
//! did not, and it could not survive being read by two replicas at once. The
//! rows cannot drift from themselves.
//!
//! ## No AI on any rung (NG-1)
//!
//! Every comment here is [`format!`] over job fields. The card's next pass
//! reads the rung-2 comment as its brief and repairs from it, which is the
//! only agent involved anywhere in this file — and it is the ordinary build
//! run, not something this raises.
//!
//! ## The ladder gates AUTO-FIRE only (AC-6)
//!
//! Nothing here refuses a person. The hold is applied by
//! [`crate::services::run_reconcile::owed`], which the manual trigger skips
//! for the card it names; `needs-human-review` excludes the card from the pick
//! and the repair lane, and the manual trigger stands that down too — see
//! [`crate::services::jobs::converge_builds`]. Clicking *Build this ticket*
//! works on every rung, including after the stop.

use nook_types::{JobId, LoopJob, TaskId, TenantId};

use crate::error::ApiResult;
use crate::state::AppState;

/// Failures before the card is handed to a human and taken out of auto-fire.
///
/// Three, because two is within the reach of one transient fault (a node
/// restarting mid-run reaches terminal twice) and a fourth costs most of an
/// hour of backoff to prove what the third already did.
pub const MAX_FAILURES: i32 = 3;

/// The first rung's hold, and the base the rest double from.
pub const FIRST_BACKOFF: chrono::Duration = chrono::Duration::minutes(5);

/// The cap (AC-2). An hour is long enough that a repo nobody is fixing costs
/// almost nothing, and short enough that a fix pushed while you were asleep is
/// picked up before you wake.
pub const MAX_BACKOFF: chrono::Duration = chrono::Duration::minutes(60);

/// `5 min × 2^(n-1)`, capped at an hour (AC-2).
///
/// `n = 0` is the flat first window, which is what a REVIEW item gets: it has
/// no ladder, and MAIN-455's five minutes is exactly this function's floor.
pub fn backoff_for(failures: i32) -> chrono::Duration {
    // Shift on the exponent, not on the minutes: `1 << 30` is still an i32,
    // while five minutes doubled thirty times is not a duration at all.
    let doublings = failures.saturating_sub(1).clamp(0, 30) as u32;
    let scaled = FIRST_BACKOFF * 2i32.saturating_pow(doublings);
    scaled.min(MAX_BACKOFF)
}

/// A human lifted the ladder's stop, so the card goes back into auto-fire with
/// a clean count (AC-5).
///
/// Both halves matter and both are here rather than at the call site. Without
/// the clear the count still reads three, and the next single failure
/// re-escalates on the spot — a hair trigger, not another go. Without the
/// nudge the card waits out a sweep interval for something a person just did
/// by hand.
pub async fn on_stop_lifted(state: &AppState, tenant: TenantId, task: TaskId) {
    if let Err(e) = state.tasks.clear_build_ladder(task, tenant).await {
        tracing::warn!(task = %task.0, error = %e, "could not clear a lifted card's ladder");
    }
    crate::services::build_loop::nudge(state, tenant, task, "escalation lifted");
}

/// A build run FAILED. Climb the card's ladder and do what the new rung says.
///
/// Best-effort and never fatal, like the handback it rides beside: this hangs
/// off a state transition, and a card write that fails must not stop a job
/// from finishing. What it cannot do it says in the log.
pub async fn on_run_failed(state: &AppState, tenant: TenantId, job: &LoopJob, task: TaskId) {
    if let Err(e) = climb(state, tenant, job, task).await {
        tracing::error!(
            job = %job.id, task = %task.0, error = ?e,
            "a build run failed and its card's ladder could not be climbed — \
             the run will be re-fired on the ordinary backoff"
        );
    }
}

async fn climb(state: &AppState, tenant: TenantId, job: &LoopJob, task: TaskId) -> ApiResult<()> {
    let runs = state.jobs.build_failure_streak(tenant, task).await?;
    let failures = runs.len() as i32;
    // Below the second rung there is only the hold, which `owed` applies from
    // the rows themselves — nothing to write, and nothing to say that the
    // handback's own comment has not already said.
    if failures < 2 {
        return Ok(());
    }
    let ids: Vec<JobId> = runs.iter().map(|r| r.id).collect();
    let why = last_line(state, job).await;

    if failures >= MAX_FAILURES {
        // The label FIRST, before the comment: the other order has a hole —
        // a comment that lands while the label write fails reads as a card
        // taken out of auto-fire that is still in it, and the next failure
        // says the same thing again. Failing towards the stop is the safe way
        // to fail. Idempotent, and applied whether or not it was already
        // there: a card somebody else escalated for their own reason still
        // has to lose `loop-changes-requested` on reaching three, or the two
        // labels sit on it saying opposite things.
        state
            .tasks
            .attach_label(tenant, task, ESCALATION_LABEL)
            .await?;
        // Its opposite comes off in the same breath: the card is not owed a
        // repair pass any more, it is owed a person, and leaving both on says
        // both things at once.
        state.tasks.detach_label(tenant, task, REPAIR_LABEL).await?;
        // Said ONCE per run of failures, and the count is what makes that
        // true: the streak reaches exactly `MAX_FAILURES` on one failure and
        // never again until something resets it, so this cannot repeat on the
        // fourth. Keyed on the count rather than on "did I just add the
        // label", which skipped the whole announcement for a card that was
        // already carrying it.
        if failures == MAX_FAILURES {
            comment(state, tenant, task, stopped(&ids, why.as_deref())).await?;
            notify_stopped(state, tenant, task, failures).await;
        }
        tracing::warn!(
            job = %job.id, task = %task.0, failures,
            "build runs failed {MAX_FAILURES} times running — card handed to a human"
        );
    } else {
        state.tasks.attach_label(tenant, task, REPAIR_LABEL).await?;
        comment(state, tenant, task, brief(job.id, failures, why.as_deref())).await?;
    }

    state
        .registry
        .publish(tenant, nook_proto::UiEvent::TaskChanged { task_id: task });
    Ok(())
}

/// AC-3's label: the board's existing "this needs another pass, and here is
/// what for" signal, so the next run is an ordinary repair rather than a kind
/// of run that only this ladder can produce.
const REPAIR_LABEL: &str = "loop-changes-requested";

/// AC-4's stop. The same label the claim reaper and the merge sweep raise, and
/// the same one every loop skill already treats as "a human must look" — which
/// is why it is this name and not a new one.
///
/// Public because taking it OFF is AC-5's reset, and the one place that
/// happens — [`crate::services::tasks::detach_label`] — must key on this
/// definition rather than on a second copy of the string.
pub const ESCALATION_LABEL: &str = "needs-human-review";

/// The rung-2 comment (AC-3): the run, why it failed, and what the next pass
/// is being asked to do about it. A fixed template — nothing here is
/// generated, so two identical failures produce two identical comments, and a
/// human who reads one has read them all.
fn brief(run: JobId, failures: i32, why: Option<&str>) -> String {
    let mut body = format!(
        "Build run `{run}` failed. That is {failures} in a row for this card, so the loop is \
         asking for a repair pass rather than another attempt at the same thing.\n\n\
         **The next run's brief is this failure.** Find out why the build did not complete \
         before building the card's acceptance criteria again — the same attempt will fail the \
         same way."
    );
    if let Some(why) = why {
        body.push_str(&format!("\n\nLast transcript line:\n\n```\n{why}\n```"));
    }
    body.push_str(&format!(
        "\n\nOne more failure and this card leaves the loop for a human ({}).",
        stop_of(MAX_FAILURES)
    ));
    body
}

/// The rung-3 comment (AC-4): every run in the streak, by id, and what has
/// been done about the card.
fn stopped(runs: &[JobId], why: Option<&str>) -> String {
    let listed = runs
        .iter()
        .rev()
        .map(|r| format!("- `{r}`"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut body = format!(
        "{} build runs for this card have failed in a row:\n\n{listed}\n\n\
         The loop has stopped picking it up: it is labelled `{ESCALATION_LABEL}` and \
         `{REPAIR_LABEL}` has been removed. Nothing about this card will auto-fire again \
         until a person removes `{ESCALATION_LABEL}`, which also starts the count over.",
        runs.len()
    );
    if let Some(why) = why {
        body.push_str(&format!(
            "\n\nLast transcript line of the last run:\n\n```\n{why}\n```"
        ));
    }
    body.push_str("\n\nTo force one run without lifting the label, use `nook builds enqueue`.");
    body
}

fn stop_of(n: i32) -> String {
    format!("{n} failures in a row")
}

async fn comment(state: &AppState, tenant: TenantId, task: TaskId, body: String) -> ApiResult<()> {
    state
        .tasks
        .create_comment(crate::repo::tasks::NewComment {
            tenant,
            task,
            author_type: "system".into(),
            author_id: None,
            author_name: "nook-build loop".into(),
            body_md: body,
        })
        .await?;
    Ok(())
}

/// AC-4's notification. Warning level and deep-linked, matching the claim
/// reaper's escalation exactly — the card's TITLE is deliberately not
/// interpolated, so an escalation on a private card leaks nothing to the
/// tenant-wide bell (MAIN-76).
async fn notify_stopped(state: &AppState, tenant: TenantId, task: TaskId, failures: i32) {
    let key = state
        .tasks
        .task_ref(task)
        .await
        .ok()
        .flatten()
        .map(|(key, number, _title)| match key {
            Some(k) => format!("{k}-{number}"),
            None => format!("#{number}"),
        })
        .unwrap_or_else(|| task.to_string());
    let base = state.cfg.public_base_url.trim_end_matches('/');

    crate::services::notify::raise(
        state,
        tenant,
        crate::services::notify::Draft::new(format!("Build loop gave up: {key}"))
            .level("warning")
            .kind("task.build_ladder")
            .body(format!(
                "{} — the loop will not pick this card up again until \
                 `{ESCALATION_LABEL}` is removed.",
                stop_of(failures)
            ))
            .link(format!("{base}/board?task={task}"))
            .payload(serde_json::json!({ "task_id": task, "key": key })),
    )
    .await;
}

/// The run's own last word — what a failed preflight or a dead executor left
/// in the transcript — so the card explains itself without anybody opening the
/// job. `None` for a run that died with nothing to say.
async fn last_line(state: &AppState, job: &LoopJob) -> Option<String> {
    let lines = state.jobs.transcript(job.id).await.ok()?;
    let last = lines
        .iter()
        .rev()
        .find(|l| !l.content.trim().is_empty())?
        .content
        .trim();
    Some(last.chars().take(400).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AC-2 exactly: five minutes doubling to a one-hour cap. The 0 case is
    /// the review path, which has no ladder and keeps MAIN-455's flat window.
    #[test]
    fn the_backoff_doubles_from_five_minutes_and_stops_at_an_hour() {
        assert_eq!(backoff_for(0), FIRST_BACKOFF);
        assert_eq!(backoff_for(1), chrono::Duration::minutes(5));
        assert_eq!(backoff_for(2), chrono::Duration::minutes(10));
        assert_eq!(backoff_for(3), chrono::Duration::minutes(20));
        assert_eq!(backoff_for(4), chrono::Duration::minutes(40));
        assert_eq!(backoff_for(5), MAX_BACKOFF, "80 minutes is capped");
        assert_eq!(backoff_for(6), MAX_BACKOFF);
    }

    /// A card that somehow accrued an absurd streak must still produce a
    /// duration. Doubling minutes would have overflowed long before here.
    #[test]
    fn an_absurd_streak_is_still_the_cap_and_not_an_overflow() {
        assert_eq!(backoff_for(1_000), MAX_BACKOFF);
        assert_eq!(backoff_for(i32::MAX), MAX_BACKOFF);
        assert_eq!(
            backoff_for(-1),
            FIRST_BACKOFF,
            "nonsense reads as the floor"
        );
    }

    /// AC-3/NG-1: the rung-2 comment is a template over job fields — the same
    /// failure renders the same words, every time, with no agent involved.
    #[test]
    fn the_repair_brief_names_the_run_and_the_reason() {
        let run = JobId::new();
        let body = brief(run, 2, Some("gh auth status: not logged in"));
        assert!(body.contains(&run.to_string()), "the job id: {body}");
        assert!(body.contains("2 in a row"));
        assert!(body.contains("gh auth status: not logged in"));
        assert!(body.contains("One more failure"));
        assert_eq!(
            body,
            brief(run, 2, Some("gh auth status: not logged in")),
            "NG-1: rendered, not written — identical run to run"
        );

        // A run that died with nothing to say still explains the card.
        let silent = brief(run, 2, None);
        assert!(silent.contains("repair pass") && !silent.contains("Last transcript line"));
    }

    /// AC-4: the final comment names every run in the streak, and says both
    /// what happened to the labels and how to override it.
    #[test]
    fn the_stop_names_every_run_and_both_ways_back() {
        let runs: Vec<JobId> = (0..MAX_FAILURES).map(|_| JobId::new()).collect();
        let body = stopped(&runs, Some("no gh on the executor"));
        for r in &runs {
            assert!(body.contains(&r.to_string()), "run {r} is missing: {body}");
        }
        assert!(body.contains("3 build runs"));
        assert!(body.contains("`needs-human-review`"));
        assert!(body.contains("`loop-changes-requested` has been removed"));
        assert!(
            body.contains("nook builds enqueue"),
            "AC-6: the manual path is still open: {body}"
        );
    }
}
