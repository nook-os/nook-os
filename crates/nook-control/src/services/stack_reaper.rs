//! Collect what a finished build leaves behind: its docker compose stack
//! (MAIN-507), and then the worktree and branch themselves (MAIN-537).
//!
//! A build run that boots the dev stack in its worktree left it running for as
//! long as the machine stayed up: three finished builds on azul held 28
//! containers and 5.8GB doing nothing, one of them for a card that had merged
//! hours earlier. Nothing reaped them because the compose project name is
//! derived from the worktree DIRECTORY, so after a prune it is unrecoverable.
//!
//! The worktree outlived the card for the mirror-image reason: MAIN-480's
//! persist-until-merge policy is right, the merge duly happened, and nothing
//! called the prune. `taskwork::prune_worktree` was reachable only from the REST
//! route, the MCP tool and a skill's own housekeeping, so a card the control
//! plane completed by itself leaked its checkout with no human in the loop to
//! notice.
//!
//! Two ways in, and both are answers to "is this card over?", which only this
//! side knows:
//!
//! - a card reaching a terminal column reaps its own stack and tree, and says so
//!   on the card;
//! - a node's periodic inventory of what it holds is answered with everything no
//!   unfinished card wants — which is what collects the stacks and trees
//!   orphaned before this existed.
//!
//! The node never decides. It also never obeys blindly: every name here is one
//! `nook_proto::compose` derives from a build worktree's path, and the node
//! checks the shape again before running docker.

use std::collections::HashSet;

use async_trait::async_trait;
use nook_proto::ControlToNode;
use nook_types::*;

use crate::error::ApiResult;
use crate::events::{self, EventDraft};
use crate::state::AppState;

/// The card is over — Done or canceled — and its stack is dead weight.
fn terminal(column_type: &str) -> bool {
    matches!(column_type, "completed" | "canceled")
}

/// What this needs from the node it is reaping on.
///
/// A trait because the DECISION is the part worth testing — which trees come
/// down, in which ORDER, and what the card is told when one does not — and a
/// live node with a docker daemon is not something a test can stand up (AC-8).
/// The real implementation is the registry; the tests supply one that records
/// what it was asked for and answers as instructed.
#[async_trait]
pub trait NodeOps: Send + Sync {
    /// Bring the projects down. `Ok(None)` means nothing was running.
    async fn stack_down(&self, node: NodeId, projects: &[String])
        -> Result<Option<String>, String>;

    /// Remove the worktree and free the branch it held, answering with what the
    /// node did — the sentence the card's report is written from (AC-7).
    async fn remove_worktree(&self, node: NodeId, path: &str) -> Result<String, String>;
}

struct OverTheRegistry<'a>(&'a AppState);

#[async_trait]
impl NodeOps for OverTheRegistry<'_> {
    async fn stack_down(
        &self,
        node: NodeId,
        projects: &[String],
    ) -> Result<Option<String>, String> {
        reap_on_node(self.0, node, projects).await
    }

    async fn remove_worktree(&self, node: NodeId, path: &str) -> Result<String, String> {
        crate::services::taskwork::remove_worktree_on_node(self.0, node, path, true).await
    }
}

/// Reap from the move path without making the move wait for docker.
///
/// A `compose down -v` takes seconds; a board drag must not. Detached also
/// keeps AC-4's promise structurally: nothing this does can fail the move,
/// whatever the daemon on the far end is doing.
pub fn on_terminal_column(state: &AppState, tenant: TenantId, task_id: TaskId, column_type: &str) {
    if !terminal(column_type) {
        return;
    }
    let state = state.clone();
    tokio::spawn(async move {
        if let Err(e) = reap_for_task(&state, tenant, task_id).await {
            tracing::warn!(task = %task_id.0, error = %e, "could not reap the card's build stack");
        }
    });
}

/// Bring down the stack the card's build worktree started, remove the worktree,
/// and report both on the card (AC-7).
///
/// A no-op for a card with no worktree, a worktree that is not a build's, or a
/// card that has not finished — which is most cards, and stays silent.
pub async fn reap_for_task(state: &AppState, tenant: TenantId, task_id: TaskId) -> ApiResult<()> {
    reap_for_task_with(state, tenant, task_id, &OverTheRegistry(state)).await
}

/// The whole decision, against whichever node transport it is handed.
pub async fn reap_for_task_with(
    state: &AppState,
    tenant: TenantId,
    task_id: TaskId,
    ops: &dyn NodeOps,
) -> ApiResult<()> {
    let Some(task) = state.tasks.get_row(tenant, task_id).await? else {
        return Ok(());
    };
    let Some((path, node_id)) = crate::services::taskwork::prune_target(state, &task).await? else {
        return Ok(());
    };
    // The path is the whole eligibility test, and it is why a human's own
    // checkout is never touched by any of this: only a build worktree's
    // directory names a project, so `is_empty()` means "not a tree this owns".
    let projects = nook_proto::compose::build_stack_projects(&path);
    if projects.is_empty() {
        return Ok(());
    }
    // Asked here rather than trusted from the caller, so every entry point —
    // and every future one — obeys MAIN-480's policy by construction (AC-6).
    if !state
        .tasks
        .column_type_of(task.column_id)
        .await?
        .is_some_and(|t| terminal(&t))
    {
        return Ok(());
    }
    // A card can reach Done while its build is still running — a human moving
    // it, the merge reconciler landing a PR the run is still amending. The tree
    // is that run's working directory and the stack is what it is testing
    // against, so neither is touched (AC-6). The next inventory collects them
    // once the run is over.
    if let Some(job) = live_run_for(state, tenant, task_id).await? {
        say(
            state,
            tenant,
            task_id,
            format!(
                "The card is finished, but a loop run (`{}`) is still using its build \
                 worktree — nothing was removed. The next inventory collects it.",
                job.0
            ),
        )
        .await;
        return Ok(());
    }

    let reaped = match ops.stack_down(node_id, &projects).await {
        Ok(reaped) => {
            // The stack is down, so the ports it bound are genuinely free
            // (MAIN-552 AC-3). Here and not at the end of the run: a build's
            // stack outlives its run by design, and handing a port back while
            // containers are still listening on it is worse than never leasing
            // it — the next holder would be told a number that is already busy.
            //
            // After the down and not before: a failed `down` returns above with
            // the worktree kept, and the leases have to be kept with it.
            if let Err(e) = state
                .sessions
                .release_leases_by_holder(node_id, task_id.0)
                .await
            {
                tracing::warn!(
                    task = %task_id.0, node = %node_id.0, error = %e,
                    "the card's build stack came down but its port leases would not release"
                );
            }
            reaped
        }
        Err(e) => {
            // AC-3. Leaving a worktree behind wastes disk and is recoverable;
            // removing it would leave a stack whose name is derived from this
            // directory and can no longer be derived at all.
            tracing::warn!(
                task = %task_id.0, node = %node_id.0, path = %path, error = %e,
                "could not bring the card's build stack down — its worktree is kept"
            );
            say(
                state,
                tenant,
                task_id,
                format!(
                    "The card is finished, but its build stack would not come down \
                     ({e}) — its worktree was KEPT: the stack's name is derived from \
                     that directory, so removing it now would orphan the containers. \
                     The next inventory tries again."
                ),
            )
            .await;
            return Ok(());
        }
    };

    // Both arms of a successful down prune, and the stackless one is most of the
    // disk: a build that never ran `dev-up.sh` has no stack to bring down, and
    // wiring the prune to "something came down" would leave exactly those trees
    // on the machine forever.
    match ops.remove_worktree(node_id, &path).await {
        Ok(what) => {
            state.tasks.clear_worktree(task_id).await?;
            report(state, tenant, task_id, &path, reaped.as_deref(), &what).await;
        }
        Err(e) => {
            // The record is deliberately NOT cleared. It is the only thing that
            // still names this directory, and the tree is provably still there;
            // clearing it would hand the tree to nothing at all.
            //
            // This is also where a tree that PREDATES the host-uid fix lands,
            // and it is the honest answer for one: it holds root-owned files an
            // ordinary node cannot unlink, so the removal fails with `Permission
            // denied (os error 13)` however many times it is retried. NG-4
            // forbids a retroactive chown and this does not attempt one — it
            // reports the reason on the card, and the one-time clear is a
            // human's.
            tracing::warn!(
                task = %task_id.0, node = %node_id.0, path = %path, error = %e,
                "could not remove the card's build worktree — the record is kept so \
                 the next inventory tries again"
            );
            say(
                state,
                tenant,
                task_id,
                format!("The card's build worktree could not be removed ({e}) — `{path}` is still on the node."),
            )
            .await;
        }
    }
    Ok(())
}

/// The loop run still using this card's worktree, if any.
///
/// Every kind, not just `build`: what matters is whether a run is live on the
/// card, and a narrower filter here would be a second definition of "in use"
/// that could disagree with the first.
async fn live_run_for(
    state: &AppState,
    tenant: TenantId,
    task_id: TaskId,
) -> ApiResult<Option<JobId>> {
    Ok(state
        .jobs
        .list_for_task(tenant, task_id)
        .await?
        .into_iter()
        .find(|j| {
            matches!(
                j.state.as_str(),
                "queued" | "claimed" | "running" | "waiting_on_human"
            )
        })
        .map(|j| j.id))
}

/// Answer a node's inventory of the build stacks it holds (MAIN-507 AC-5):
/// anything no unfinished card wants is orphaned, and is brought down.
///
/// The protected set is derived from worktree paths rather than read from the
/// node, for the reason the whole module exists: the path is the only thing
/// that names the project, and a card is the only thing that says the path is
/// still wanted. A card in review protects its stack (AC-6) — its build has
/// finished, but a repair run reuses that worktree and expects the stack there.
pub async fn sweep_stacks_on_node(
    state: &AppState,
    node: NodeId,
    held: &[String],
) -> ApiResult<usize> {
    let protected: HashSet<String> = state
        .tasks
        .active_worktree_paths_on_node(node)
        .await?
        .iter()
        .flat_map(|path| nook_proto::compose::build_stack_projects(path))
        .collect();
    let orphans: Vec<String> = held
        .iter()
        .filter(|project| nook_proto::compose::is_build_stack_project(project))
        .filter(|project| !protected.contains(*project))
        .cloned()
        .collect();
    if orphans.is_empty() {
        return Ok(0);
    }
    tracing::info!(
        node = %node.0, projects = %orphans.join(", "),
        "bringing down build stacks no unfinished card records"
    );
    let _ = state
        .registry
        .request_op(node, |request_id| ControlToNode::ReapBuildStacks {
            request_id,
            projects: orphans.clone(),
        });
    Ok(orphans.len())
}

/// The same shape for the worktrees themselves (MAIN-537 AC-4): every tree the
/// node holds whose card has finished is taken down and removed, here rather
/// than only at the moment the card moved.
///
/// This is what collects the trees already leaked. A merged card's checkout was
/// reachable from no sweep at all — the orphan sweep beside this one removes
/// only trees NO card records, and a completed card records its tree until
/// something prunes it — so on a node that stays up, nothing ever did.
///
/// Intersected with what the node actually holds, so a record whose directory
/// is already gone does not send the node on an errand or the card a comment.
pub async fn sweep_worktrees_on_node(
    state: &AppState,
    node: NodeId,
    held: &[String],
) -> ApiResult<usize> {
    let held: HashSet<&String> = held.iter().collect();
    let finished: Vec<(TenantId, TaskId, String)> = state
        .tasks
        .finished_worktrees_on_node(node)
        .await?
        .into_iter()
        .filter(|(_, _, path)| held.contains(path))
        .collect();
    for (tenant, task, path) in &finished {
        tracing::info!(
            node = %node.0, task = %task.0, path = %path,
            "pruning the build worktree of a card that is over"
        );
        if let Err(e) = reap_for_task(state, *tenant, *task).await {
            tracing::warn!(task = %task.0, error = %e, "could not prune a finished card's worktree");
        }
    }
    Ok(finished.len())
}

/// "Where did my worktree go" now has an answer on the card (AC-7), and so does
/// "why is it still here".
async fn report(
    state: &AppState,
    tenant: TenantId,
    task_id: TaskId,
    path: &str,
    reaped: Option<&str>,
    removal: &str,
) {
    let stack = match reaped {
        Some(projects) => {
            format!("its build stack came down with its volumes (`{projects}`), and ")
        }
        None => "no build stack was up, and ".into(),
    };
    say(
        state,
        tenant,
        task_id,
        format!("The card is finished, so {stack}its worktree `{path}` was pruned: {removal}."),
    )
    .await;
    events::record(
        state,
        tenant,
        EventDraft::new("task.build_stack_reaped").payload(serde_json::json!({
            "task_id": task_id,
            "projects": reaped,
            "worktree": path,
            "removal": removal,
        })),
    )
    .await;
    state
        .registry
        .publish(tenant, nook_proto::UiEvent::TaskChanged { task_id });
}

/// Say it on the card, once.
///
/// The inventory repeats every ten minutes and a refusal repeats with it, so a
/// card whose node is offline for a day would collect 144 identical comments.
/// Comparing against what the reaper has already said is the fence: a reason
/// that CHANGES is news and is posted, the same reason twice is not.
async fn say(state: &AppState, tenant: TenantId, task_id: TaskId, body: String) {
    const AUTHOR: &str = "Stack reaper";
    match state.tasks.comments_of(task_id).await {
        Ok(existing) => {
            if existing
                .iter()
                .any(|c| c.author_name == AUTHOR && c.body_md == body)
            {
                return;
            }
        }
        // Unreadable comments are not a reason to stay silent about a prune.
        Err(e) => {
            tracing::warn!(task = %task_id.0, error = %e, "could not read the card's comments")
        }
    }
    let _ = state
        .tasks
        .create_comment(crate::repo::tasks::NewComment {
            tenant,
            task: task_id,
            author_type: "system".into(),
            author_id: None,
            author_name: AUTHOR.into(),
            body_md: body,
        })
        .await;
    state
        .registry
        .publish(tenant, nook_proto::UiEvent::TaskChanged { task_id });
}

/// Ask the node to bring the projects down. `Ok(None)` means nothing was
/// running, which is the ordinary case and worth no comment.
async fn reap_on_node(
    state: &AppState,
    node_id: NodeId,
    projects: &[String],
) -> Result<Option<String>, String> {
    let projects = projects.to_vec();
    let rx = state
        .registry
        .request_op(node_id, |request_id| ControlToNode::ReapBuildStacks {
            request_id,
            projects: projects.clone(),
        })
        .ok_or_else(|| "node is offline".to_string())?;
    let op = tokio::time::timeout(std::time::Duration::from_secs(120), rx)
        .await
        .map_err(|_| "node did not answer in time".to_string())?
        .map_err(|_| "node disconnected".to_string())?;
    if op.ok {
        // `path` carries what actually came down; absent means nothing was up.
        Ok(op.path)
    } else {
        Err(op.message)
    }
}
