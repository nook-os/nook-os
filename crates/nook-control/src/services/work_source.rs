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
    /// Per PR: the newest head a `changes_requested` review REJECTED — what a
    /// repair item is fingerprinted on. Never the PR's current head, which
    /// the repair's own push moves: that fingerprint would clear its own
    /// answer and raise a second repair before any reviewer looked.
    pub rejected_heads: std::collections::HashMap<i64, String>,
}

impl BuildWork<'_> {
    /// The pick contract as code (MAIN-458 AC-1a / NG-1): `agent-ready` is the
    /// human approval gate and the converger NEVER reads a card without it;
    /// `blocked`, assigned, still-blocked, backlog, done and epic cards are
    /// excluded exactly as `nook tasks`' server-side filters exclude them
    /// (MAIN-80, MAIN-464) — the filtering is the query's, not re-implemented
    /// here, which is what stops the two drifting.
    async fn fresh_items(&self, workspace: WorkspaceId) -> Option<Vec<WorkItem>> {
        let rows = self
            .tasks
            .pick_tasks(
                self.tenant,
                self.viewer,
                crate::repo::tasks::PickParams {
                    board: None,
                    workspace: Some(workspace.0),
                    column_type: None,
                    priority: None,
                    unassigned_only: true,
                    assignee: None,
                    labels: vec!["agent-ready".into()],
                    not_labels: vec!["blocked".into()],
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
                },
            )
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
        let Ok(cards) = self.tasks.tasks_with_pr(self.tenant, workspace).await else {
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
            .filter_map(|(task, number, pr_url)| {
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
                    fingerprint: repair_fingerprint(rejected),
                    label: format!("repair PR #{pr_number}"),
                    target_task_id: Some(task),
                    // The card already sits claimed-and-parked in In Review;
                    // re-claiming would drag the board under a human's feet.
                    claim_first: false,
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
