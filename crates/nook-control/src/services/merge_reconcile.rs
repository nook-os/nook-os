//! Reconcile merged pull requests onto the board (MAIN-491).
//!
//! A card whose PR has merged used to stay where it was — In Review, In
//! Progress — until a human dragged it or one of the agent skills (`nook-yolo`,
//! `nook-epic-runner`) happened to run. Between those runs the board lies:
//! shipped work still reads as in flight. This is the safety net underneath
//! those skills, not a replacement for them (NG-2), and it lives here because
//! the control plane is the only component that is always running.
//!
//! **Read-only against the forge (NG-3).** The sweep asks how a PR ended and
//! writes the answer onto the BOARD. It never comments on a pull request, sets
//! a PR label, or merges anything — merging stays the agent skills' authority.
//!
//! **Every pass reads before it writes (AC-7).** A forge error is an outage,
//! never an empty result, and the way to hold on an outage is to have written
//! nothing yet — so one workspace's forge reads all happen first, and a failure
//! anywhere in them abandons the pass with the board untouched.
//!
//! **Safe on every replica (AC-8).** Two writes decide everything, and each is
//! its own exactly-once fence: the guarded move
//! (`taskwork::complete_in_flight_card`) and the label insert
//! (`attach_label_once`, the `claim_reaper` precedent). Only the replica whose
//! write matched goes on to comment, so N control planes produce one move and
//! one comment — and so does one control plane sweeping N times.

use std::time::Duration;

use nook_types::{TaskId, TenantId, WorkspaceId};

use crate::error::ApiResult;
use crate::services::forge::{github_repo, Forge, GithubForge, MergeState, Repo};
use crate::state::AppState;

/// The escalation label, and the opt-out. A card carrying it belongs to a
/// person until they say otherwise, so the sweep neither moves it nor speaks
/// (AC-5) — the same name and the same meaning `claim_reaper` uses.
const ESCALATION_LABEL: &str = "needs-human-review";

/// How far back the body join looks for a merge (AC-2).
///
/// It only has to cover the gap between a PR merging and a sweep seeing it, and
/// the ordinary path is the card's own recorded `pr_url` — this is the backstop
/// for cards that never got one. A week is generous for that and keeps the read
/// to a single page of closed PRs.
const MERGE_WINDOW_DAYS: i64 = 7;

/// Spawn the sweep. Fire-and-forget like the reapers; the process exits on
/// shutdown, taking the task with it.
pub fn start(state: AppState) {
    tokio::spawn(async move {
        let scan = state.cfg.merge_reconcile_scan_secs;
        tracing::info!(scan_secs = scan, "merged-PR board reconciler started");
        run(state, scan).await;
    });
}

async fn run(state: AppState, scan_secs: u64) {
    let interval = Duration::from_secs(scan_secs.max(1));
    let mut switch = crate::services::loops::SwitchLog::default();
    loop {
        tokio::time::sleep(interval).await;
        // Gated with the rest of the loop machinery (AC-1). With loops off this
        // is one indexed lookup and nothing else: no forge call, no board
        // write. An operator who turned loops off gets the quiet they asked
        // for, including on GitHub's rate limit.
        if !switch.observe(
            "merge_reconcile",
            crate::services::loops::any_enabled(&*state.settings).await,
        ) {
            continue;
        }
        if let Err(e) = pass(&state).await {
            tracing::warn!(error = %e, "merged-PR reconcile pass failed");
        }
    }
}

/// One sweep over every workspace of every loops-on tenant.
///
/// Public so a test can drive a pass without waiting on the timer.
pub async fn pass(state: &AppState) -> ApiResult<()> {
    for (tenant, value) in state
        .settings
        .tenants_with_value(crate::services::loops::KEY)
        .await?
    {
        // A row can exist reading `false`; the same truthiness as the per-tenant
        // gate, so on and off mean the same thing here as everywhere else.
        if !crate::services::loops::truthy(Some(&value)) {
            continue;
        }
        for ws in state.workspaces.list(tenant).await? {
            // No GitHub repo, or nothing to authenticate with: a silent no-op
            // (AC-7). Both mean the same thing — there is no question to ask —
            // and a workspace on a local path is a supported state, not a
            // failure worth logging every pass.
            let Some(repo) = ws.git_remote_url.as_deref().and_then(github_repo) else {
                continue;
            };
            let token = crate::services::workspace_gh_token(state, tenant, ws.id).await;
            let own;
            let forge: &dyn Forge = match token {
                Some(t) => {
                    own = GithubForge::from_token(&t);
                    &own
                }
                None => match state.review_demand.forge() {
                    Some(f) => f,
                    None => continue,
                },
            };
            match reconcile_workspace(state, forge, &repo, tenant, ws.id).await {
                Ok(r) if r.is_empty() => {}
                Ok(r) => tracing::info!(
                    workspace = %ws.id,
                    moved = r.moved,
                    escalated = r.escalated,
                    backfilled = r.backfilled,
                    "reconciled merged pull requests onto the board"
                ),
                // The outage case lands here as well as a genuine bug, and both
                // want the same treatment: say so, change nothing, try again
                // next pass.
                Err(e) => {
                    tracing::warn!(workspace = %ws.id, error = %e, "merged-PR reconcile held")
                }
            }
        }
    }
    Ok(())
}

/// What one workspace's sweep did, for the caller's log line.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Reconciled {
    /// Cards moved to Done because their PR merged.
    pub moved: usize,
    /// Cards labelled `needs-human-review` because their PR was closed unmerged.
    pub escalated: usize,
    /// Cards whose missing `pr_url` the body join filled in.
    pub backfilled: usize,
}

impl Reconciled {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// The whole sweep for one workspace: read what the forge says, then write.
///
/// `forge` is a parameter rather than something resolved in here so a test can
/// script merged / closed-unmerged / error answers — the same seam the hygiene
/// pass uses.
pub async fn reconcile_workspace(
    state: &AppState,
    forge: &dyn Forge,
    repo: &Repo,
    tenant: TenantId,
    workspace: WorkspaceId,
) -> ApiResult<Reconciled> {
    // ── read ────────────────────────────────────────────────────────────
    // Every `?` below is AC-7: an unreachable forge abandons the pass here,
    // with nothing yet written, rather than acting on a partial picture.
    let mut recorded = Vec::new();
    for (task, pr_url) in state
        .tasks
        .in_flight_tasks_with_pr(tenant, workspace)
        .await?
    {
        // A `pr_url` that is not a PR permalink — the compare URL `submit-pr`
        // derives when the agent recorded no real one — names no pull request
        // to ask about. Skipped, not guessed at.
        let Some(number) = pr_number(&pr_url) else {
            continue;
        };
        recorded.push((
            task,
            pr_url,
            forge.pr_details(repo, number).await?.merge_state,
        ));
    }
    let since = chrono::Utc::now() - chrono::Duration::days(MERGE_WINDOW_DAYS);
    let merged = forge.merged_prs_since(repo, since).await?;

    // ── write ───────────────────────────────────────────────────────────
    let mut out = Reconciled::default();
    for (task, pr_url, state_of_pr) in recorded {
        match state_of_pr {
            // Still open: not finished, nothing owed.
            MergeState::Open => {}
            MergeState::Merged => {
                if complete(state, tenant, task, &pr_url).await? {
                    out.moved += 1;
                }
            }
            MergeState::ClosedUnmerged => {
                if escalate(state, tenant, task, &pr_url).await? {
                    out.escalated += 1;
                }
            }
        }
    }

    // The second join, in the contract's order: a card reaches here only if it
    // has NO recorded `pr_url`, because the backfill's own guard is that test.
    // So a card the loop above already handled cannot be handled twice, and a
    // card whose recorded PR says one thing is never overruled by a body scan.
    for m in merged {
        let Some(key) = crate::services::jobs::closes_key(&m.body) else {
            continue;
        };
        let Ok(task) = crate::services::tasks::resolve_id(state.tasks.as_ref(), tenant, &key).await
        else {
            // A PR closing a key this tenant does not have is ordinary — the
            // fleet's repos are not all one board's.
            continue;
        };
        let url = pr_web_url(repo, m.number);
        if !state.tasks.backfill_pr_url(tenant, task, &url).await? {
            continue;
        }
        out.backfilled += 1;
        tracing::info!(%task, pr = m.number, "backfilled a card's missing pr_url from the merged PR");
        if complete(state, tenant, task, &url).await? {
            out.moved += 1;
        }
    }
    Ok(out)
}

/// Move a merged card to Done and say so exactly once (AC-3, AC-4).
///
/// The label check comes first and is the whole of AC-5: a card a human owns is
/// not moved, not commented, not touched.
async fn complete(
    state: &AppState,
    tenant: TenantId,
    task: TaskId,
    pr_url: &str,
) -> ApiResult<bool> {
    if escalated(state, task).await? {
        return Ok(false);
    }
    // The guarded move IS the once-fence: a second pass (or a second replica)
    // finds the card completed, matches nothing, and therefore never comments.
    if crate::services::taskwork::complete_in_flight_card(state, tenant, task)
        .await?
        .is_none()
    {
        return Ok(false);
    }
    announce(
        state,
        tenant,
        task,
        "merged",
        pr_url,
        &format!("Reconciled: PR {pr_url} is merged; card moved to Done."),
    )
    .await;
    Ok(true)
}

/// Raise a hand for a card whose PR was closed without merging (AC-6).
///
/// Never a move: closed-unmerged means the work did not land, and where that
/// leaves the card is a person's judgement, not a sweep's. The label insert is
/// the once-fence, exactly as in `claim_reaper` — and a card already carrying
/// the label is already a human's, so it stays silent (AC-5).
async fn escalate(
    state: &AppState,
    tenant: TenantId,
    task: TaskId,
    pr_url: &str,
) -> ApiResult<bool> {
    if !state
        .tasks
        .attach_label_once(tenant, task, ESCALATION_LABEL)
        .await?
    {
        return Ok(false);
    }
    announce(
        state,
        tenant,
        task,
        "closed_unmerged",
        pr_url,
        &format!(
            "Reconciled: PR {pr_url} was closed without merging; \
             left in place for a human."
        ),
    )
    .await;
    Ok(true)
}

async fn escalated(state: &AppState, task: TaskId) -> ApiResult<bool> {
    Ok(state
        .tasks
        .labels_of_task(task)
        .await?
        .iter()
        .any(|l| l.name == ESCALATION_LABEL))
}

/// The three things a reconciliation owes: the card comment that says what
/// happened, an activity event, and a board refresh.
///
/// No notification, unlike `claim_reaper`'s escalation. Both outcomes here are
/// a pull request reaching the end a person already decided for it — they
/// merged it, or they closed it — so the board and the feed are the right
/// loudness. `task.pr_reconciled` is deliberately absent from
/// `events::catalog`, which is what keeps it out of the bell.
async fn announce(
    state: &AppState,
    tenant: TenantId,
    task: TaskId,
    outcome: &str,
    pr_url: &str,
    body: &str,
) {
    let _ = state
        .tasks
        .create_comment(crate::repo::tasks::NewComment {
            tenant,
            task,
            author_type: "system".into(),
            author_id: None,
            author_name: "Merge reconciler".into(),
            body_md: body.to_string(),
        })
        .await;

    crate::events::record(
        state,
        tenant,
        crate::events::EventDraft::new("task.pr_reconciled").payload(serde_json::json!({
            "task_id": task,
            "pr_url": pr_url,
            "outcome": outcome,
        })),
    )
    .await;

    state
        .registry
        .publish(tenant, nook_proto::UiEvent::TaskChanged { task_id: task });
}

/// The pull request number a recorded `pr_url` names, the same way the repair
/// queue reads one: the trailing path segment.
fn pr_number(pr_url: &str) -> Option<u64> {
    pr_url
        .trim_end_matches('/')
        .rsplit('/')
        .next()?
        .parse()
        .ok()
}

fn pr_web_url(repo: &Repo, number: u64) -> String {
    format!(
        "https://github.com/{}/{}/pull/{number}",
        repo.owner, repo.name
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pr_permalink_yields_its_number_and_a_compare_url_yields_nothing() {
        assert_eq!(
            pr_number("https://github.com/nook-os/nook-os/pull/375"),
            Some(375)
        );
        assert_eq!(
            pr_number("https://github.com/nook-os/nook-os/pull/375/"),
            Some(375)
        );
        // What `submit-pr` derives when no real PR was reported — it names no
        // pull request, so there is nothing to ask the forge about.
        assert_eq!(
            pr_number("https://github.com/nook-os/nook-os/compare/main-491?expand=1"),
            None
        );
        assert_eq!(pr_number(""), None);
    }
}
