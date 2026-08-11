//! Dispatch pauses while a workspace's own default branch is red (MAIN-543).
//!
//! On 2026-08-11 main was broken and the fleet kept dispatching build runs into
//! the hole: every one failed at the same compile error, and the review loop
//! escalated a card to `needs-human-review` for a failure its PR had not
//! caused. The control plane knew nothing about main's health, so it kept
//! handing out work that could not possibly pass.
//!
//! **The signal is DERIVED on every dispatch pass and stored nowhere (AC-2).**
//! That is the whole design, not an implementation detail: a stored flag is a
//! thing that has to be un-stuck, and the failure mode of this feature would
//! then be a fleet paused on a main that went green hours ago. With nothing
//! stored, recovery is the absence of a reason to pause — the next pass reads
//! green and dispatches, with no human action, no flag to clear and no restart.
//! It is also why there is no operator switch (NG-2).
//!
//! **Unknown is not red (AC-3).** A forge error, a rate limit, a repo with no
//! CI, a branch with nothing completed yet — all of them dispatch. A forge
//! outage that stopped the fleet would be a far worse failure than the one this
//! prevents, and it would arrive as silence. Only a COMPLETED run concluding
//! `failure` pauses anything.
//!
//! It holds back CLAIMING, never running work (AC-5): nothing here touches a
//! job past `queued`, so an in-flight run finishes exactly as it would have.
//! Same semantics as a node cordon at capacity `0` (MAIN-508) — stop claiming,
//! finish what you hold. And it is per workspace throughout (AC-7): one repo's
//! red trunk says nothing about another's.

use std::collections::HashMap;
use std::sync::Mutex;

use nook_types::{TenantId, WorkspaceId};

use crate::services::forge::{github_repo, CiRun, Forge, GithubForge};
use crate::state::AppState;

/// The conclusion that pauses dispatch, and the only one.
///
/// GitHub also says `cancelled`, `timed_out`, `stale`, `action_required` and
/// `neutral`. None of those is evidence the trunk is broken — a cancelled run
/// is usually somebody superseding a push — and AC-1 names `failure`. Widening
/// this is how a pause starts firing on noise, which is the failure mode AC-3
/// exists to keep us away from.
const RED: &str = "failure";

/// One dispatch pass's memo of what the forge said, so a pass with ten queued
/// builds for one workspace asks once rather than ten times.
///
/// Deliberately per-PASS and not a TTL cache: a cache is state that survives
/// the pause, and AC-2's "the next poll dispatches" is only true if the next
/// pass genuinely re-reads. It is created by the caller, used, and dropped.
#[derive(Default)]
pub struct Pass {
    seen: HashMap<WorkspaceId, Option<CiRun>>,
}

/// Which workspaces are currently paused, kept ONLY so the log speaks once per
/// transition rather than once per poll (AC-8) — the same contract as
/// [`crate::services::loops::SwitchLog`], one entry per workspace.
///
/// Never read as truth. The pause itself is derived fresh every pass; this
/// remembers only what has already been said out loud.
#[derive(Default)]
pub struct PauseLog {
    said: Mutex<HashMap<WorkspaceId, bool>>,
}

impl PauseLog {
    /// Note what this pass derived for one workspace, logging on the first
    /// observation and on every flip.
    pub fn observe(&self, workspace: WorkspaceId, red: Option<&CiRun>) {
        let Ok(mut said) = self.said.lock() else {
            return;
        };
        let now = red.is_some();
        if said.get(&workspace) == Some(&now) {
            return;
        }
        match red {
            Some(run) => tracing::warn!(
                %workspace,
                branch = %run.branch,
                workflow = %run.workflow,
                url = %run.url,
                "build dispatch paused — this workspace's default branch is red"
            ),
            None => tracing::info!(
                %workspace,
                "build dispatch resumed — this workspace's default branch is no longer red"
            ),
        }
        said.insert(workspace, now);
    }
}

/// The failing run holding this workspace's build dispatch, or `None` to
/// dispatch.
///
/// Every unknown collapses to `None` here rather than at four call sites: no
/// remote, a remote that is not a forge this build knows, no credential, an
/// error from the forge, no completed run. They all mean *we do not know, so
/// carry on*, and that judgement belongs in one place.
pub async fn red_default_branch(
    state: &AppState,
    tenant: TenantId,
    workspace: WorkspaceId,
    pass: &mut Pass,
) -> Option<CiRun> {
    if let Some(seen) = pass.seen.get(&workspace) {
        return seen.clone();
    }
    let red = derive(state, tenant, workspace).await;
    state.main_ci.observe(workspace, red.as_ref());
    pass.seen.insert(workspace, red.clone());
    red
}

async fn derive(state: &AppState, tenant: TenantId, workspace: WorkspaceId) -> Option<CiRun> {
    let ws = state.workspaces.get(tenant, workspace).await.ok()??;
    let repo = github_repo(ws.git_remote_url.as_deref()?)?;

    // The workspace's own token OUTRANKS the deployment's forge, exactly as
    // `ReviewDemand::prs` resolves it (MAIN-456): a tenant that configured its
    // identity asks GitHub as itself. The deployment forge — the fleet token,
    // or a test's injected fake — is the fallback.
    let own;
    let forge: &dyn Forge =
        match crate::services::workspace_gh_token(state, tenant, workspace).await {
            Some(t) => {
                own = GithubForge::from_token(&t);
                &own
            }
            None => state.review_demand.forge()?,
        };

    match forge.default_branch_ci(&repo).await {
        Ok(Some(run)) if run.conclusion == RED => Some(run),
        Ok(_) => None,
        Err(e) => {
            // One line at debug, not a warning per pass: an unreadable signal
            // is an ordinary state here (a repo with no Actions permission on
            // the fleet token), and it changes nothing about what happens next.
            tracing::debug!(%workspace, error = %e, "could not read the default branch's CI — dispatching");
            None
        }
    }
}

/// The sentence a held-back run carries, naming the run a human should go look
/// at (AC-6).
///
/// It deliberately shares no wording with `no_executor_reason`'s family: the
/// fleet is fine, the node is fine, and reading "no eligible executor" here
/// would send somebody hunting capacity that was never the problem. It also
/// says there is nothing to clear, because the first instinct on seeing a
/// pause is to look for the switch that lifts it.
pub fn reason(run: &CiRun) -> String {
    let at = if run.url.is_empty() {
        String::new()
    } else {
        format!(" — {}", run.url)
    };
    format!(
        "held: this workspace's default branch ({}) is red, so no new build run is dispatched \
         until it is green again. The latest completed run there, {} at {}, concluded {}{at}. \
         Nothing to clear — the next dispatch poll after a green run places this job.",
        run.branch,
        run.workflow,
        short(&run.head_sha),
        run.conclusion,
    )
}

/// A sha a human can compare against `git log` without it eating the sentence.
fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn run(conclusion: &str) -> CiRun {
        CiRun {
            branch: "main".into(),
            workflow: "CI".into(),
            conclusion: conclusion.into(),
            url: "https://github.com/acme/api/actions/runs/9".into(),
            head_sha: "deadbeefcafe".into(),
        }
    }

    #[test]
    fn the_reason_names_the_run_and_is_not_an_executor_complaint() {
        let r = reason(&run("failure"));
        assert!(r.contains("main"), "{r}");
        assert!(r.contains("deadbee"), "{r}");
        assert!(r.contains("actions/runs/9"), "{r}");
        assert!(
            !r.contains("no eligible executor"),
            "AC-6: it must not read as any existing cause: {r}"
        );
    }

    #[test]
    fn the_pause_log_speaks_on_change_and_stays_quiet_otherwise() {
        let log = PauseLog::default();
        let ws = WorkspaceId(Uuid::from_u128(3));
        let red = run("failure");
        log.observe(ws, Some(&red));
        assert_eq!(log.said.lock().unwrap().get(&ws), Some(&true));
        log.observe(ws, Some(&red));
        assert_eq!(log.said.lock().unwrap().get(&ws), Some(&true));
        log.observe(ws, None);
        assert_eq!(log.said.lock().unwrap().get(&ws), Some(&false));
    }

    #[test]
    fn a_workspace_is_logged_apart_from_every_other() {
        let log = PauseLog::default();
        let (a, b) = (
            WorkspaceId(Uuid::from_u128(1)),
            WorkspaceId(Uuid::from_u128(2)),
        );
        log.observe(a, Some(&run("failure")));
        log.observe(b, None);
        let said = log.said.lock().unwrap();
        assert_eq!(said.get(&a), Some(&true));
        assert_eq!(said.get(&b), Some(&false));
    }
}
