//! What managed agent work EXISTS, and how a run of it is named.
//!
//! The control plane converges runs: it asks a source what work there is, then
//! makes sure exactly one live headless run exists per item. Review is the
//! first source; a builder loop and an epic runner are meant to land here as
//! two more, which is why the pipeline below the seam knows nothing about pull
//! requests.
//!
//! ## Why a run and not a session
//!
//! A run is a `loop_jobs` row: headless (`claude -p --output-format
//! stream-json`), placed by the executor selection every other kind already
//! uses, and read afterwards through the transcript a spec run is read through.
//! The review loop used to be a tmux SESSION, started by typing
//! `/loop /nook-review` into an interactive TUI — which exposed an attachable
//! terminal on a machine, blocked on Claude Code's onboarding prompt with
//! nobody to answer it, and left nothing to read once it died.
//!
//! ## The rule that replaces the timer
//!
//! An item carries a `fingerprint`. A run is owed when the current fingerprint
//! differs from the one the last completed run recorded — so a PR that nobody
//! has pushed to is owed nothing, and a quiet repo costs no agents at all. The
//! old sweep asked every five minutes because a count cannot answer this.

use nook_types::{TaskId, TenantId, UserId, WorkspaceId};

/// One unit of managed agent work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkItem {
    /// Stable identity within its workspace — the PR number, for review. What
    /// "one live run per item" is keyed on.
    pub key: i64,
    /// What the item looks like RIGHT NOW: a PR's head sha. Two runs of the
    /// same key with the same fingerprint would do identical work, so the
    /// second is not raised.
    pub fingerprint: String,
    /// Passed to the agent so it knows which item it is working, without the
    /// pipeline having to understand what the number means.
    pub label: String,
    /// The card a BUILD item is about (MAIN-458): what the raised row targets,
    /// and — for a fresh pick — the card the control plane claims first.
    /// `None` for review items, whose unit is a PR.
    pub target_task_id: Option<TaskId>,
    /// A fresh pick is CLAIMED before its run is raised (MAIN-458 AC-3); a
    /// repair item's card already sits claimed-and-parked in In Review, and
    /// re-claiming it would drag the board around under a human's feet.
    pub claim_first: bool,
    /// When a human last restarted the item's card (MAIN-584 AC-2), for the one
    /// thing the fingerprint cannot say: a run concluded before this instant no
    /// longer speaks for the card. `None` for review items, whose unit is a PR
    /// and which nobody blocks.
    pub unblocked_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Where managed work comes from.
///
/// One method, deliberately: everything else about a run — placement, the
/// transcript, resume, the kind wall — is the same whatever produced it, and a
/// source that could influence those would not be a source any more.
#[async_trait::async_trait]
pub trait WorkSource: Send + Sync {
    /// The skill a run of this work drives. A fixed name, chosen here rather
    /// than by a caller, for the reason the node picks a runtime's login
    /// command: the wire never names an executable.
    fn skill(&self) -> &'static str;

    /// The job kind rows are written with, so existing dispatch, the kind wall
    /// and the reaper keep working unchanged.
    fn kind(&self) -> &'static str;

    /// What work this workspace has right now.
    ///
    /// `None` means UNKNOWN — the source could not tell, and the caller must
    /// hold rather than conclude "no work". An outage that read as an empty
    /// queue would scale reviewers to zero exactly when they were needed, which
    /// is the same distinction [`crate::services::forge`] draws.
    async fn items(&self, workspace: WorkspaceId, remote: Option<&str>) -> Option<Vec<WorkItem>>;
}

/// Review work: the repository's open pull requests, one run each.
///
/// The fingerprint is the PR's head sha, so a push is what earns a new run and
/// nothing else does. This is the whole of what replaced the five-minute sweep.
pub struct ReviewWork<'a> {
    pub demand: &'a crate::services::forge::ReviewDemand,
    /// The workspace's own forge token (MAIN-456); `None` falls back to the
    /// deployment forge. Resolved by the CALLER, which holds the vault.
    pub token: Option<String>,
}

#[async_trait::async_trait]
impl WorkSource for ReviewWork<'_> {
    fn skill(&self) -> &'static str {
        "nook-review"
    }

    fn kind(&self) -> &'static str {
        crate::services::jobs::REVIEW_KIND
    }

    async fn items(&self, workspace: WorkspaceId, remote: Option<&str>) -> Option<Vec<WorkItem>> {
        Some(
            self.demand
                .prs(workspace, remote, self.token.as_deref())
                .await?
                .into_iter()
                .map(|pr| WorkItem {
                    key: pr.number as i64,
                    fingerprint: pr.head_sha,
                    label: format!("PR #{}", pr.number),
                    target_task_id: None,
                    claim_first: false,
                    unblocked_at: None,
                })
                .collect(),
        )
    }
}

/// A stable fingerprint of a card's CONTRACT — title and description — so a
/// human edit re-raises a run and the control plane's own claim/release
/// writes (which only touch `assignee`/`updated_at`) cannot clear a failure
/// hold they had nothing to do with. UUIDv5, so it never varies across
/// processes or releases the way `DefaultHasher` may.
pub fn card_fingerprint(title: &str, description: Option<&str>) -> String {
    uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_OID,
        format!("nook-build-card:{title}\n{}", description.unwrap_or("")).as_bytes(),
    )
    .to_string()
}

/// A repair item's fingerprint: the rejected head. Prefixed so a card that is
/// somehow both pickable and under repair cannot alias fingerprints.
pub fn repair_fingerprint(head_sha: &str) -> String {
    format!("repair:{head_sha}")
}

/// What the builder is TOLD a repair is for — the run's seed, and the first
/// line of its transcript.
///
/// A reviewer's `changes_requested` and a merge conflict need no explaining
/// here: both leave their contract on the pull request, where the builder's
/// repair pass reads it. A QUEUE EJECTION is the one that does (MAIN-542
/// AC-5), because the thing that failed is not a thing this branch can run:
/// the PR's own checks are green, and a builder that goes looking at them
/// concludes there is nothing to fix and hands the card back.
///
/// A HUMAN's ruling (MAIN-591) is the second: its contract is on the pull
/// request too, but as an ordinary comment rather than under the
/// `Loop review of <sha>` marker the repair pass looks for — so a builder told
/// only "repair PR #N" would find no verdict, conclude there was nothing to
/// answer, and hand the card back.
pub fn repair_label(pr: u64, source: Option<&str>) -> String {
    if source == Some(crate::repo::jobs::HUMAN_VERDICT_SOURCE) {
        return format!(
            "repair PR #{pr} — a PERSON requested the changes, not the review agent. Their \
             ruling is an ordinary comment on the pull request, NOT under a \
             `Loop review of <sha>` marker: read the newest human comment there and treat it \
             as the must-fix list."
        );
    }
    if source == Some(crate::repo::jobs::EJECTION_VERDICT_SOURCE) {
        return format!(
            "repair PR #{pr} — the merge queue EJECTED it. Its own checks passed against the \
             base it was branched from; the build the queue ran against the CURRENT base \
             branch failed, and that build runs nowhere else. Rebase onto the current base \
             branch and make the checks pass THERE — re-running this branch's own checks \
             will show nothing wrong."
        );
    }
    format!("repair PR #{pr}")
}

/// Build work: the board's approved cards, one run each — plus REPAIR items,
/// cards whose recorded PR carries `loop-changes-requested` at a head no
/// repair run has answered (MAIN-458 AC-1).
pub struct BuildWork<'a> {
    pub tasks: &'a dyn crate::repo::tasks::TaskRepository,
    pub tenant: TenantId,
    /// Who the pick is visible AS — the same identity the raised runs carry.
    pub viewer: UserId,
    pub demand: &'a crate::services::forge::ReviewDemand,
    pub token: Option<String>,
    /// Per PR: the newest head a `changes_requested` REJECTED, and what
    /// rejected it — what a repair item is fingerprinted on, and what it is
    /// labelled with. Never the PR's current head, which the repair's own push
    /// moves: that fingerprint would clear its own answer and raise a second
    /// repair before any reviewer looked.
    pub rejected_heads: std::collections::HashMap<i64, crate::repo::jobs::RejectedHead>,
    /// The card the manual trigger named — the one case where the
    /// `needs-human-review` exclusion stands down, in BOTH lanes. `None` on the
    /// reconciler's own passes, which is every auto-fire.
    pub unblock_task: Option<TaskId>,
}

/// The pick contract as code (MAIN-458 AC-1a / NG-1): `agent-ready` is the
/// human approval gate and the converger NEVER reads a card without it;
/// `blocked`, `needs-human-review`, assigned, still-blocked, backlog, done and
/// epic cards are excluded exactly as `nook tasks`' server-side filters exclude
/// them (MAIN-80, MAIN-464) — the filtering is the query's, not re-implemented
/// here, which is what stops the two drifting.
///
/// `needs-human-review` is MAIN-386 AC-4's stop, and it belongs beside
/// `blocked` rather than in the ladder: by the time the label is on, the ladder
/// has already said everything it has to say, and a card a HUMAN escalated for
/// their own reason must be excluded by exactly the same rule.
///
/// A named constructor rather than a literal inside `fresh_items`, so a test
/// proving what the fresh pick excludes runs THESE parameters instead of a copy
/// that would keep passing after the real ones changed (MAIN-496).
pub fn fresh_pick_params(workspace: Option<WorkspaceId>) -> crate::repo::tasks::PickParams {
    crate::repo::tasks::PickParams {
        board: None,
        workspace: workspace.map(|w| w.0),
        column_type: None,
        priority: None,
        unassigned_only: true,
        assignee: None,
        labels: vec!["agent-ready".into()],
        not_labels: vec!["blocked".into(), "needs-human-review".into()],
        is_blocked: Some(false),
        created_after: None,
        limit: 200,
        archived: false,
        q: None,
        types: vec!["task".into(), "bug".into(), "story".into(), "chore".into()],
        parent: None,
        backlog: false,
        done: false,
        visibility: vec![],
        node: None,
    }
}

impl BuildWork<'_> {
    /// Exactly one rule stands down, for the manual trigger (MAIN-489 AC-5,
    /// MAIN-386 AC-6): a human naming a card the ladder stopped is overruling
    /// the loop's own escalation, the way a forced re-review overrules an
    /// already-verdicted head (MAIN-473) — which is how such a card is nudged
    /// back without anybody editing its labels. The caller narrows the result
    /// to the one named card; everything else still comes from
    /// [`fresh_pick_params`], so there is still one definition of the contract.
    ///
    /// `blocked` is NOT in that stand-down, and the asymmetry is the whole
    /// point. `needs-human-review` is what the LOOP writes when it gives up, so
    /// lifting it for a card a person is asking for right now is the loop
    /// deferring to them; `blocked` is a person's own hold on the work, which
    /// the loop no longer writes at all and is not this trigger's to lift.
    async fn fresh_items(&self, workspace: WorkspaceId) -> Option<Vec<WorkItem>> {
        let mut params = fresh_pick_params(Some(workspace));
        if self.unblock_task.is_some() {
            params.not_labels.retain(|l| l != "needs-human-review");
        }
        let rows = self
            .tasks
            .pick_tasks(self.tenant, self.viewer, params)
            .await
            .ok()?;
        Some(
            rows.into_iter()
                .filter_map(|t| {
                    Some(WorkItem {
                        key: i64::from(t.number?),
                        fingerprint: card_fingerprint(&t.title, t.description.as_deref()),
                        label: t.key.clone().unwrap_or_else(|| t.id.0.to_string()),
                        target_task_id: Some(t.id),
                        claim_first: true,
                        unblocked_at: t.unblocked_at,
                    })
                })
                .collect(),
        )
    }

    /// Cards whose recorded PR the reviewer rejected, at the rejected head.
    ///
    /// `None` from the forge means UNKNOWN, and for repair items alone that is
    /// tolerable as "no repairs this pass": the fresh items above never depend
    /// on the forge, and holding the whole board because GitHub blinked would
    /// invert the review loop's own outage rule for no gain.
    async fn repair_items(&self, workspace: WorkspaceId, remote: Option<&str>) -> Vec<WorkItem> {
        let Ok(cards) = self
            .tasks
            .tasks_with_pr(self.tenant, workspace, self.unblock_task)
            .await
        else {
            return Vec::new();
        };
        if cards.is_empty() {
            return Vec::new();
        }
        let Some(prs) = self
            .demand
            .prs(workspace, remote, self.token.as_deref())
            .await
        else {
            return Vec::new();
        };
        cards
            .into_iter()
            .filter_map(|(task, number, pr_url, unblocked_at)| {
                let pr_number: u64 = pr_url.rsplit('/').next()?.parse().ok()?;
                let pr = prs.iter().find(|p| p.number == pr_number)?;
                if !pr.labels.iter().any(|l| l == "loop-changes-requested") {
                    return None;
                }
                // No recorded rejection means the label predates verdict
                // recording (or was hand-applied): nothing to fingerprint on,
                // so nothing raised — a hot loop against a guess is worse
                // than waiting for the next verdict to record one.
                let rejected = self.rejected_heads.get(&(pr_number as i64))?;
                Some(WorkItem {
                    // NEGATED: repair is its own fingerprint space with its
                    // own bookkeeping (see `build_run_heads`), so a repair
                    // outcome can never overwrite the record that the card's
                    // content was already built.
                    key: -number,
                    fingerprint: repair_fingerprint(&rejected.review_head_sha),
                    label: repair_label(pr_number, rejected.review_verdict_source.as_deref()),
                    target_task_id: Some(task),
                    // The card already sits claimed-and-parked in In Review;
                    // re-claiming would drag the board under a human's feet.
                    claim_first: false,
                    unblocked_at,
                })
            })
            .collect()
    }
}

#[async_trait::async_trait]
impl WorkSource for BuildWork<'_> {
    fn skill(&self) -> &'static str {
        "nook-build"
    }

    fn kind(&self) -> &'static str {
        crate::services::jobs::BUILD_KIND
    }

    async fn items(&self, workspace: WorkspaceId, remote: Option<&str>) -> Option<Vec<WorkItem>> {
        // Board unreadable is UNKNOWN — hold, exactly as a forge outage holds
        // reviews. A card can appear in both halves (released after a repair
        // began, say); 0050's per-card index arbitrates the duplicate at
        // raise, the same way it arbitrates two replicas.
        let mut items = self.fresh_items(workspace).await?;
        items.extend(self.repair_items(workspace, remote).await);
        Some(items)
    }
}

#[cfg(test)]
mod fingerprint_tests {
    use super::*;

    /// The fingerprint is the card's CONTRACT: stable under everything the
    /// control plane itself writes (claim, release, column moves — none of
    /// which appear in it), moved only by a human edit — which is what earns
    /// a fresh run (MAIN-458 AC-6's repair rule, applied to fresh picks).
    #[test]
    fn a_cards_fingerprint_moves_with_its_contract_and_nothing_else() {
        let a = card_fingerprint("Add a thing", Some("## AC-1"));
        assert_eq!(a, card_fingerprint("Add a thing", Some("## AC-1")));
        assert_ne!(a, card_fingerprint("Add a thing", Some("## AC-1 amended")));
        assert_ne!(a, card_fingerprint("Retitled", Some("## AC-1")));
        // Absent and empty descriptions are the same contract.
        assert_eq!(card_fingerprint("t", None), card_fingerprint("t", Some("")));
    }

    /// MAIN-542 AC-5. Only the ejection needs explaining, and it needs the
    /// whole explanation: a builder told "repair PR #7" re-runs this branch's
    /// checks, finds them green, and concludes there is nothing to fix.
    #[test]
    fn a_queue_ejections_repair_says_what_actually_failed() {
        let ejected = repair_label(7, Some(crate::repo::jobs::EJECTION_VERDICT_SOURCE));
        assert!(ejected.starts_with("repair PR #7 —"));
        assert!(ejected.contains("merge queue EJECTED it"));
        assert!(ejected.contains("CURRENT base branch"));
        assert!(ejected.contains("Rebase onto the current base branch"));

        // A reviewer's own rejection and a conflict both leave their contract
        // on the pull request, where the builder's repair pass reads it.
        assert_eq!(repair_label(7, None), "repair PR #7");
        assert_eq!(
            repair_label(7, Some(crate::repo::jobs::CONFLICT_VERDICT_SOURCE)),
            "repair PR #7"
        );
    }

    /// A new rejected head re-raises; an unchanged one does not — and a card
    /// that is somehow both pickable and under repair cannot alias the two
    /// fingerprint spaces.
    #[test]
    fn repair_fingerprints_track_the_rejected_head() {
        assert_eq!(repair_fingerprint("abc123"), repair_fingerprint("abc123"));
        assert_ne!(repair_fingerprint("abc123"), repair_fingerprint("def456"));
        assert!(repair_fingerprint("abc123").starts_with("repair:"));
        assert_ne!(
            repair_fingerprint("x"),
            card_fingerprint("x", None),
            "the two item kinds never collide on a fingerprint"
        );
    }
}
