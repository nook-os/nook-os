//! Put stuck pull requests back where the loop can see them (MAIN-476).
//!
//! PR #353 sat for hours in a state no machinery watched: merge CONFLICTS, no
//! loop label on the PR (a second verdict clobbered one, a human removed the
//! other), while its card still said loop-approved. The reviewer would not
//! re-run (verdicted-head finality), the builder's repair query found nothing,
//! and the epic runner rightly refused to merge. Two heals close that hole:
//!
//! - a CONFLICTING open PR re-enters the repair queue — a recorded
//!   `changes_requested` at the conflicting head, `loop-changes-requested`
//!   plus one comment per head, mirrored onto the linked card — so the builder
//!   rebases it (the control plane NEVER rebases; it only routes the work);
//! - a PR whose verdict label was stripped externally gets it restored from the
//!   verdict this deployment itself recorded for the CURRENT head, and only
//!   from that: one writer wins, and someone else's judgement is not ours to
//!   reapply.
//!
//! The RECORD is the load-bearing half, and it was missing until MAIN-516: the
//! repair queue reads the job ledger (`rejected_review_heads`), never the PR's
//! labels, so a labelled PR with no recorded rejection was in a queue nothing
//! could see it in. An approved PR that a merge made conflict does not move its
//! head, so nothing ever re-triggered — a stable deadlock, observed on #407.
//!
//! This is deliberately not part of `run_reconcile`, whose whole contract is
//! that it does not know what a pull request is.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::repo::jobs::RecordedVerdict;
use crate::services::forge::{verdict_label, Forge, PullRequest, Repo, VERDICT_LABELS};
use crate::state::AppState;
use nook_types::{TenantId, UserId, WorkspaceId};

/// The first line of the conflict comment, and the once-per-head marker: a
/// comment containing `"{CONFLICT_MARK} {head}"` proves this head was already
/// announced, however many passes have run since.
pub const CONFLICT_MARK: &str = "Loop conflict check of";

/// What one pass decided for one PR, before any IO.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Re-apply the label matching the verdict this deployment recorded for
    /// the PR's current head (AC-3).
    Restore { pr: u64, label: &'static str },
    /// The PR carries no label that routes it anywhere — ask the forge whether
    /// it conflicts, and if so put it into the repair queue (AC-1).
    CheckConflict { pr: u64, head: String },
}

/// The decision rule, split from the IO so it is testable as a function of its
/// inputs.
///
/// `needs-human-review` opts a PR out of everything here: it has left the
/// automated queue until a person resolves the escalation, and neither heal may
/// drag it back. A PR already labelled `loop-changes-requested` is already in
/// the repair queue, so a conflict adds nothing.
///
/// Note that AC-3's restore applies to `needs-human-review` like any recorded
/// verdict: clearing that label by hand is undone within a pass, by design —
/// one writer wins. The way OUT of a restored escalation is a new head (the
/// restore is head-scoped) or a new recorded verdict, not label surgery.
pub fn plan(prs: &[PullRequest], recorded: &[RecordedVerdict]) -> Vec<Action> {
    let mut actions = Vec::new();
    for pr in prs {
        let has = |l: &str| pr.labels.iter().any(|x| x == l);
        if has("needs-human-review") {
            continue;
        }
        let mut restored = None;
        if !VERDICT_LABELS.iter().any(|l| has(l)) {
            restored = recorded
                .iter()
                .find(|r| {
                    r.review_pr_number == pr.number as i64 && r.review_head_sha == pr.head_sha
                })
                .and_then(|r| verdict_label(&r.review_verdict));
            if let Some(label) = restored {
                actions.push(Action::Restore {
                    pr: pr.number,
                    label,
                });
            }
        }
        // Candidacy counts the label this same pass is about to restore: a
        // restored `loop-changes-requested` is already routed, and a restored
        // `needs-human-review` re-escalates rather than re-queues. A restored
        // `loop-approved` stays a candidate — the base may have moved under it.
        //
        // Since MAIN-516 a conflict records its own `changes_requested`, so a
        // stripped label at an ALREADY-ANNOUNCED head is now restored here
        // instead of being re-labelled by the conflict branch — and that branch
        // is also where `mirror_to_card` retries a mirror that failed earlier.
        // The retry is therefore given up for that pass. Deliberate, and narrow:
        // it needs a failed mirror AND a label stripped afterwards, and buying
        // it back would mean a `pr_details` round trip on every restore to learn
        // what only the conflict branch needs to know. The next pass in which
        // the label is absent AND no verdict restores it still retries.
        let routed = has("loop-changes-requested")
            || matches!(
                restored,
                Some("loop-changes-requested" | "needs-human-review")
            );
        if !routed {
            actions.push(Action::CheckConflict {
                pr: pr.number,
                head: pr.head_sha.clone(),
            });
        }
    }
    actions
}

/// The `Closes KEY` line, which is the PR's only join to its board card — the
/// same literal contract the reviewer parses.
fn closes_key(body: &str) -> Option<String> {
    body.lines().find_map(|l| {
        let token = l
            .trim()
            .strip_prefix("Closes ")?
            .split_whitespace()
            .next()?;
        let (prefix, num) = token.rsplit_once('-')?;
        (!prefix.is_empty() && !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()))
            .then(|| token.to_string())
    })
}

/// What the recorded conflict row says it is, for anyone reading the job
/// ledger: the cause, and the fact that no agent produced it (AC-6). The
/// `review_verdict_source` column is the machine-readable half of the same
/// statement.
fn conflict_seed(pr: u64) -> String {
    format!(
        "PR #{pr} conflicts with its base branch — rebase required. Recorded by the \
         control plane's pull-request hygiene pass: no review run was raised, no agent \
         read this head, and there are no findings behind this verdict."
    )
}

/// What one heal pass did, for the caller's log line.
#[derive(Debug, Default)]
pub struct Healed {
    pub restored: usize,
    pub marked: usize,
    /// Conflicting heads returned to the repair queue AS THE QUEUE READS IT —
    /// a recorded `changes_requested` (MAIN-516). `marked` counts the label,
    /// which is a different write and can succeed or fail on its own.
    pub recorded: usize,
}

/// Run both heals over one workspace's open PRs.
///
/// Forge writes fail SOFT, per PR: this runs beside the reconciler, and one
/// PR's failed label write must not stop the next PR's heal or the pass
/// itself. Nothing is retried eagerly — the next hygiene pass sees the same
/// facts and tries again.
pub async fn heal(
    state: &AppState,
    forge: &dyn Forge,
    repo: &Repo,
    tenant: TenantId,
    requested_by: UserId,
    workspace: WorkspaceId,
    prs: &[PullRequest],
) -> crate::error::ApiResult<Healed> {
    let recorded = state
        .jobs
        .recorded_review_verdicts(tenant, workspace)
        .await?;
    let mut healed = Healed::default();
    for action in plan(prs, &recorded) {
        match action {
            Action::Restore { pr, label } => match forge.set_verdict_label(repo, pr, label).await {
                Ok(()) => {
                    healed.restored += 1;
                    tracing::info!(
                        %workspace, pr, label,
                        "restored a verdict label stripped outside the loop"
                    );
                }
                Err(e) => {
                    tracing::warn!(%workspace, pr, label, error = %e, "verdict label restore failed")
                }
            },
            Action::CheckConflict { pr, head } => {
                let details = match forge.pr_details(repo, pr).await {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::warn!(%workspace, pr, error = %e, "could not read PR details");
                        continue;
                    }
                };
                // `None` is GitHub still computing: unknown, so do nothing —
                // never treat it as either answer. A later pass asks again.
                if details.mergeable != Some(false) {
                    continue;
                }
                // FIRST, before either forge write, because the label is
                // one-way: once `loop-changes-requested` is on, `plan` calls
                // the PR routed and no later pass reaches this branch again
                // (NG-2). Labelling before recording is therefore the exact
                // deadlock MAIN-516 exists to end — a PR in the repair queue
                // by its label, and invisible to the queue's own query.
                //
                // The reverse partial failure is self-healing: a recorded row
                // with no label is restored from that very verdict by the
                // restore heal above, on the next pass.
                let row = nook_types::JobId::new();
                let why = conflict_seed(pr);
                match state
                    .jobs
                    .record_conflict_rejection(crate::repo::jobs::ConflictRejection {
                        id: row,
                        tenant,
                        workspace,
                        requested_by,
                        pr: pr as i64,
                        head: head.clone(),
                        seed: why.clone(),
                    })
                    .await
                {
                    Ok(true) => {
                        healed.recorded += 1;
                        // The transcript is where a person looks to see what a
                        // run did, and this one has nothing to show. Say so, or
                        // an empty transcript beside a verdict reads as an
                        // agent that reviewed and found nothing.
                        crate::services::jobs::append_transcript(state, row, "system", &why)
                            .await
                            .ok();
                        tracing::info!(%workspace, pr, head = %head, "recorded a conflict rejection — the repair queue can see this PR");
                    }
                    Ok(false) => {}
                    Err(e) => {
                        // Do not label what the queue cannot read: leaving the
                        // PR untouched costs one pass, where labelling it costs
                        // every future pass.
                        tracing::warn!(%workspace, pr, error = %e, "could not record the conflict rejection");
                        continue;
                    }
                }
                let marker = format!("{CONFLICT_MARK} {head}");
                let announced = match forge.issue_comment_bodies(repo, pr).await {
                    Ok(bodies) => bodies.iter().any(|b| b.contains(&marker)),
                    Err(e) => {
                        // Without the comment list, once-per-head cannot be
                        // proven — do nothing rather than risk repeating.
                        tracing::warn!(%workspace, pr, error = %e, "could not read PR comments");
                        continue;
                    }
                };
                if !announced {
                    let body = format!(
                        "{marker}\n\nThis pull request conflicts with the base branch — \
                         rebase required. It is back in the loop's repair queue; the \
                         builder's next repair pass rebases it."
                    );
                    if let Err(e) = forge.comment(repo, pr, &body).await {
                        tracing::warn!(%workspace, pr, error = %e, "conflict comment failed");
                        continue;
                    }
                }
                // The label goes on even when the comment already existed: a
                // stripped label at an already-announced head is re-applied
                // without a second comment. Its failure does NOT skip the card
                // mirror below — each write catches up independently, or a
                // one-off 403 here would strand the card at PR #353's exact
                // split-brain forever.
                match forge
                    .set_verdict_label(repo, pr, "loop-changes-requested")
                    .await
                {
                    Ok(()) => {
                        healed.marked += 1;
                        tracing::info!(%workspace, pr, head = %head, "conflicting PR returned to the repair queue");
                    }
                    Err(e) => {
                        tracing::warn!(%workspace, pr, error = %e, "conflict label failed")
                    }
                }
                // Deduped against the CARD's own comments, not against whether
                // this iteration posted the PR comment — a mirror that failed
                // (or was skipped by an earlier partial failure) is retried on
                // every pass until the card carries the marker.
                if let Err(e) =
                    mirror_to_card(state, tenant, requested_by, repo, pr, &head, &details.body)
                        .await
                {
                    tracing::warn!(%workspace, pr, error = %e, "conflict card mirror failed");
                }
            }
        }
    }
    Ok(healed)
}

/// AC-2: the card mirror, shaped exactly like the reviewer's own — one comment
/// naming the head and the URL, `loop-changes-requested` on, `loop-approved`
/// off. A pre-existing `needs-human-review` never reaches here (the plan skips
/// the PR), and the card does not move columns — a changes-requested verdict
/// does not move it either; the builder's repair pass does.
///
/// Idempotent on the CARD's own state: a card already carrying this head's
/// marker comment is left alone, so the caller can retry freely.
async fn mirror_to_card(
    state: &AppState,
    tenant: TenantId,
    requested_by: UserId,
    repo: &Repo,
    pr: u64,
    head: &str,
    pr_body: &str,
) -> crate::error::ApiResult<()> {
    let Some(key) = closes_key(pr_body) else {
        tracing::debug!(pr, "no `Closes KEY` line — nothing to mirror to");
        return Ok(());
    };
    let task = match crate::services::tasks::resolve_id(state.tasks.as_ref(), tenant, &key).await {
        Ok(t) => t,
        Err(_) => {
            tracing::warn!(pr, key = %key, "conflict mirror: the Closes key resolves to no card");
            return Ok(());
        }
    };
    let marker = format!("{CONFLICT_MARK} {head}");
    if state
        .tasks
        .comments_of(task)
        .await?
        .iter()
        .any(|c| c.body_md.contains(&marker))
    {
        return Ok(());
    }
    let url = format!("https://github.com/{}/{}/pull/{pr}", repo.owner, repo.name);
    let body = format!("{marker} — conflicts with the base branch, rebase required: {url}");
    crate::services::tasks::insert_agent_comment(
        state.tasks.as_ref(),
        tenant,
        task,
        requested_by.0,
        "nook loop",
        &body,
    )
    .await?;
    crate::services::tasks::attach_label(
        state.tasks.as_ref(),
        tenant,
        task,
        "loop-changes-requested",
    )
    .await?;
    crate::services::tasks::detach_label(state.tasks.as_ref(), tenant, task, "loop-approved")
        .await?;

    // The route's rule, kept exactly: a comment's event carries an excerpt and
    // human key ONLY for a non-private card — a private card's body must not
    // reach the tenant-wide feed, and the missing excerpt is what keeps the
    // notification bridge silent about it.
    let mut payload = serde_json::json!({ "task_id": task, "author": "nook loop" });
    if let Some((visibility, number, board_key)) =
        state.tasks.task_visibility_naming(task, tenant).await?
    {
        if visibility != "private" {
            payload["excerpt"] = serde_json::json!(body.chars().take(140).collect::<String>());
            if let (Some(k), Some(n)) = (board_key, number) {
                payload["key"] = serde_json::json!(format!("{k}-{n}"));
            }
        }
    }
    crate::events::record(
        state,
        tenant,
        crate::events::EventDraft::new("task.comment.created")
            .actor("user", requested_by.0)
            .payload(payload),
    )
    .await;
    state
        .registry
        .publish(tenant, nook_proto::UiEvent::TaskChanged { task_id: task });
    Ok(())
}

/// At most one heal per workspace per TTL — the reconciler polls every few
/// seconds, and per-PR detail reads must ride the forge cache's rhythm, not the
/// poll's.
pub struct Hygiene {
    ttl: Duration,
    last: Mutex<HashMap<WorkspaceId, Instant>>,
}

impl Hygiene {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            last: Mutex::new(HashMap::new()),
        }
    }

    /// True at most once per TTL per workspace; stamps when it says yes, so a
    /// failing heal also waits a TTL rather than hammering a broken forge.
    pub fn due(&self, workspace: WorkspaceId) -> bool {
        let Ok(mut last) = self.last.lock() else {
            return false;
        };
        match last.get(&workspace) {
            Some(t) if t.elapsed() < self.ttl => false,
            _ => {
                last.insert(workspace, Instant::now());
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(number: u64, head: &str, labels: &[&str]) -> PullRequest {
        PullRequest {
            number,
            head_sha: head.into(),
            labels: labels.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn verdict(pr: i64, head: &str, verdict: &str) -> RecordedVerdict {
        RecordedVerdict {
            review_pr_number: pr,
            review_head_sha: head.into(),
            review_verdict: verdict.into(),
        }
    }

    #[test]
    fn an_unlabeled_pr_is_a_conflict_candidate() {
        assert_eq!(
            plan(&[pr(7, "aaa", &[])], &[]),
            vec![Action::CheckConflict {
                pr: 7,
                head: "aaa".into()
            }]
        );
    }

    /// AC-1's "second pass adds nothing", at the decision level: once the label
    /// is on, the PR is already routed and nothing more is planned.
    #[test]
    fn a_pr_already_in_the_repair_queue_plans_nothing() {
        assert!(plan(&[pr(7, "aaa", &["loop-changes-requested"])], &[]).is_empty());
    }

    /// `needs-human-review` means a person owns this PR now. Neither heal may
    /// touch it — not even to restore a label.
    #[test]
    fn an_escalated_pr_is_left_entirely_alone() {
        let recorded = [verdict(7, "aaa", "changes_requested")];
        assert!(plan(&[pr(7, "aaa", &["needs-human-review"])], &recorded).is_empty());
    }

    /// AC-3: recorded verdict for the CURRENT head, no verdict label on the PR
    /// → restore the matching label. An approved PR stays a conflict candidate
    /// too, because the base may have moved under a clean verdict.
    #[test]
    fn a_stripped_label_is_restored_from_the_recorded_verdict() {
        let recorded = [verdict(7, "aaa", "approved")];
        assert_eq!(
            plan(&[pr(7, "aaa", &["bug"])], &recorded),
            vec![
                Action::Restore {
                    pr: 7,
                    label: "loop-approved"
                },
                Action::CheckConflict {
                    pr: 7,
                    head: "aaa".into()
                },
            ]
        );
    }

    /// A restored `loop-changes-requested` already routes the PR — planning a
    /// conflict check on top would be double work.
    #[test]
    fn a_restored_changes_requested_is_already_routed() {
        let recorded = [verdict(7, "aaa", "changes_requested")];
        assert_eq!(
            plan(&[pr(7, "aaa", &[])], &recorded),
            vec![Action::Restore {
                pr: 7,
                label: "loop-changes-requested"
            }]
        );
    }

    /// AC-3's negative: the verdict was for another head, so it says nothing
    /// about this one. The PR is a plain conflict candidate, never labeled from
    /// a stale judgement.
    #[test]
    fn a_verdict_for_an_old_head_restores_nothing() {
        let recorded = [verdict(7, "old", "approved")];
        assert_eq!(
            plan(&[pr(7, "new", &[])], &recorded),
            vec![Action::CheckConflict {
                pr: 7,
                head: "new".into()
            }]
        );
    }

    /// AC-4's last clause: no recorded verdict, no restoration — someone else's
    /// labels are not ours to reinvent.
    #[test]
    fn no_recorded_verdict_never_restores() {
        assert_eq!(
            plan(&[pr(7, "aaa", &[])], &[]),
            vec![Action::CheckConflict {
                pr: 7,
                head: "aaa".into()
            }]
        );
    }

    /// A PR that still carries SOME verdict label is not restored over: one
    /// writer wins, and a label present means a writer already spoke.
    #[test]
    fn a_present_verdict_label_is_never_overwritten() {
        let recorded = [verdict(7, "aaa", "changes_requested")];
        assert_eq!(
            plan(&[pr(7, "aaa", &["loop-approved"])], &recorded),
            vec![Action::CheckConflict {
                pr: 7,
                head: "aaa".into()
            }],
            "conflict check still runs, but no restore"
        );
    }

    #[test]
    fn skipped_verdicts_restore_nothing() {
        let recorded = [verdict(7, "aaa", "skipped")];
        assert_eq!(
            plan(&[pr(7, "aaa", &[])], &recorded),
            vec![Action::CheckConflict {
                pr: 7,
                head: "aaa".into()
            }]
        );
    }

    #[test]
    fn the_closes_line_finds_its_key_and_ignores_lookalikes() {
        assert_eq!(
            closes_key("What changed\n\nCloses MAIN-476\n\nRisk: Low"),
            Some("MAIN-476".into())
        );
        assert_eq!(closes_key("Closes WEB-UI-7 tail"), Some("WEB-UI-7".into()));
        assert_eq!(closes_key("It closes MAIN-476"), None, "mid-sentence");
        assert_eq!(closes_key("Closes the gap"), None, "no key shape");
        assert_eq!(closes_key("Closes MAIN-"), None, "no number");
        assert_eq!(closes_key(""), None);
    }

    #[test]
    fn the_throttle_stamps_once_per_ttl_per_workspace() {
        let h = Hygiene::new(Duration::from_secs(300));
        let a = WorkspaceId(uuid::Uuid::from_u128(1));
        let b = WorkspaceId(uuid::Uuid::from_u128(2));
        assert!(h.due(a));
        assert!(!h.due(a), "inside the TTL");
        assert!(h.due(b), "workspaces do not share a stamp");

        let zero = Hygiene::new(Duration::ZERO);
        assert!(zero.due(a));
        assert!(zero.due(a), "an expired stamp is due again");
    }
}
