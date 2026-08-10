//! What happens to a card when its build run concludes NOTHING (MAIN-489).
//!
//! The three outcome paths (`pr_opened`, `blocked`, `nothing_to_do`) each say
//! what the card becomes. A run that simply ends — a failed preflight, a dead
//! executor, a cancel — used to say nothing at all, and because the pick reads
//! `unassigned_only`, the still-claimed card dropped out of the work source
//! entirely: not even the failure backoff applied to it, because there was no
//! item left to hold back. It sat in In Progress with nothing running until the
//! claim reaper's four-hour cap escalated it to a human. Twice on 2026-08-09.
//!
//! So a terminal run with no outcome hands the card back here instead: the
//! loop's own claim released, the card returned to the unstarted column, and a
//! comment saying why — after which the ordinary
//! [`FAILURE_BACKOFF`](crate::services::run_reconcile::FAILURE_BACKOFF) governs
//! the retry, exactly as it governs any other attempt that concluded nothing.
//!
//! Retrying forever is the other failure, so the handback counts: three
//! consecutive FAILED runs label the card `blocked`, which is already the pick
//! contract's stop. The count is spent by a recorded outcome, and by either
//! human nudge that brings an escalated card back — see
//! [`crate::repo::tasks::TaskRepository::clear_build_failures`].
//!
//! `failed` alone, because MAIN-496 chose `canceled` for a queued run nothing
//! could place precisely so that it would not read as one: a canceled run never
//! happened. It still gets its card back — that is AC-1, and it is about
//! reaching a terminal state, not about which one.

use nook_types::{JobId, LoopJob, TaskId, TenantId};

use crate::error::ApiResult;
use crate::state::AppState;

/// Consecutive concluded-nothing runs before the card is handed to a human.
///
/// Three, because two is within the reach of one transient fault (a node
/// restarting mid-run reaches terminal twice) and anything larger is most of an
/// hour of backoff spent proving what the second failure already suggested.
pub const MAX_STRIKES: i32 = 3;

/// A build run reached a terminal state. If it recorded no outcome, hand its
/// card back to the board.
///
/// Best-effort and never fatal: this rides the state transition, and a card
/// write that fails must not stop a job from finishing. What it cannot do it
/// says in the log, where the claim reaper is still the backstop.
pub async fn on_run_concluded(state: &AppState, tenant: TenantId, job: &LoopJob) {
    if job.kind != crate::services::jobs::BUILD_KIND || job.build_outcome.is_some() {
        return;
    }
    // A repair run's card was never claimed by the loop and is parked in In
    // Review under a human's eye (NG-2): there is no claim of ours to give
    // back, and moving it would drag the board around.
    if job
        .build_fingerprint
        .as_deref()
        .is_some_and(|f| f.starts_with("repair:"))
    {
        return;
    }
    let Some(task) = job.target_task_id else {
        return;
    };
    if let Err(e) = hand_back(state, tenant, job, task).await {
        tracing::error!(
            job = %job.id, task = %task.0, error = ?e,
            "a build run concluded nothing and its card could not be handed back — \
             the claim reaper is the remaining backstop"
        );
    }
}

async fn hand_back(
    state: &AppState,
    tenant: TenantId,
    job: &LoopJob,
    task: TaskId,
) -> ApiResult<()> {
    // The release and the column move are `give_card_back`'s pair (MAIN-482),
    // and `held_by` is the AC-7 fence: a card a human took over, or one they
    // dragged into progress unleased, is not ours to hand back. `false` means
    // exactly that, and nothing below runs — the strikes are a record of the
    // loop failing on work it was actually holding.
    if !crate::services::jobs::give_card_back(state, tenant, task, Some(job.requested_by)).await? {
        return Ok(());
    }

    // Only a FAILED run moves the ladder, which is MAIN-496's rule and not this
    // card's to bend: `failed` means the run ran and lost, while `canceled`
    // means it never happened — a human stopping it, or the queued-job reaper
    // ending one nothing could place. The card still comes back either way
    // (AC-1 is about reaching a terminal state, not about how), it just does
    // not move towards `blocked` on an attempt nobody made.
    let strikes = if job.state == "failed" {
        // AC-4's label goes on BEFORE the count reaches three, not after. The
        // other order has a hole: a bump that lands while the `attach` fails
        // leaves the card pickable at a full strike set, and the next claim's
        // reset — whose whole premise is "only a human's hand gets a card
        // here" — then zeroes it, and the cycle runs forever. Failing towards
        // the label is the safe way to fail; a strike lost to the same error
        // costs one extra attempt.
        //
        // DO NOT collapse the read and the bump into one atomic statement to
        // save a round trip: the ordering IS the safety, and the atomic form
        // can only decide after it has already written. What the split costs
        // is a race two handbacks on one card would have to win — 0050's
        // per-card unique index makes that near-impossible — and its whole
        // effect is a comment reading `attempt 4 of 3`, which is left honest
        // rather than clamped, because a count past the maximum is exactly the
        // evidence that two of these ran at once.
        let reaching = state.tasks.build_failures(task, tenant).await? + 1;
        if reaching >= MAX_STRIKES {
            state.tasks.attach_label(tenant, task, "blocked").await?;
            tracing::warn!(
                job = %job.id, task = %task.0, strikes = reaching,
                "build runs concluded nothing three times running — card blocked for a human"
            );
        }
        // The bump's own answer is the one the card is told, so what a human
        // reads is the count that was actually written.
        Some(state.tasks.bump_build_failures(task, tenant).await?)
    } else {
        None
    };

    let why = last_failure_line(state, job, task).await;
    let body = comment(job.id, &job.state, strikes, why.as_deref());
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

    state
        .registry
        .publish(tenant, nook_proto::UiEvent::TaskChanged { task_id: task });
    Ok(())
}

fn comment(run: JobId, run_state: &str, strikes: Option<i32>, why: Option<&str>) -> String {
    let mut body = format!(
        "Build run {run} ended `{run_state}` with no outcome recorded — claim released, card \
         back in To-Do"
    );
    match strikes {
        Some(n) => body.push_str(&format!(" (attempt {n} of {MAX_STRIKES}).")),
        None => body.push_str(". A run that never ran does not count towards the stop."),
    }
    if let Some(why) = why {
        body.push_str(&format!("\n\nLast thing the run said: {why}"));
    }
    if strikes.is_some_and(|n| n >= MAX_STRIKES) {
        body.push_str(&format!(
            "\n\n{MAX_STRIKES} runs in a row have concluded nothing, so this card is now \
             `blocked` and the loop will not pick it up again. Remove the label once the cause \
             is fixed, or force one run with `nook builds enqueue`."
        ));
    }
    body
}

/// The run's own last word — the transcript line a failed preflight or a dead
/// executor leaves — so the card explains itself without anybody opening the
/// job. Absent for a run that died with nothing to say.
///
/// Also absent when the card has just been told: a REFUSAL comments its reason
/// itself (MAIN-482 AC-6) and that same reason is the transcript's last line,
/// so quoting it back would say one thing twice in two consecutive comments.
async fn last_failure_line(state: &AppState, job: &LoopJob, task: TaskId) -> Option<String> {
    let lines = state.jobs.transcript(job.id).await.ok()?;
    let last = lines
        .iter()
        .rev()
        .find(|l| !l.content.trim().is_empty())?
        .content
        .trim();
    let said = state.tasks.comments_of(task).await.unwrap_or_default();
    if said.iter().rev().take(2).any(|c| c.body_md.trim() == last) {
        return None;
    }
    Some(last.chars().take(400).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The card has to explain itself before it reappears (AC-2), and say how
    /// many attempts were made and what the last one said (AC-4).
    #[test]
    fn the_comment_names_the_failure_and_the_attempt_count() {
        let run = JobId::new();
        let first = comment(
            run,
            "failed",
            Some(1),
            Some("gh auth status: not logged in"),
        );
        assert!(first.contains("attempt 1 of 3"));
        assert!(first.contains("gh auth status: not logged in"));
        assert!(
            !first.contains("blocked"),
            "one strike escalates nothing: {first}"
        );

        let third = comment(run, "failed", Some(3), Some("no gh on the executor"));
        assert!(third.contains("attempt 3 of 3"));
        assert!(
            third.contains("`blocked`") && third.contains("no gh on the executor"),
            "the escalation says how many and why: {third}"
        );

        // A run that never ran still explains the card's return, without
        // pretending an attempt was made (MAIN-496's rule).
        let canceled = comment(run, "canceled", None, None);
        assert!(canceled.contains("claim released"));
        assert!(
            !canceled.contains("attempt") && canceled.contains("does not count"),
            "a cancel is not a strike: {canceled}"
        );
    }
}
