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

use nook_types::WorkspaceId;

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
                })
                .collect(),
        )
    }
}
