//! Bring a finished build's docker compose stack down (MAIN-507).
//!
//! A build run that boots the dev stack in its worktree left it running for as
//! long as the machine stayed up: three finished builds on azul held 28
//! containers and 5.8GB doing nothing, one of them for a card that had merged
//! hours earlier. Nothing reaped them because the compose project name is
//! derived from the worktree DIRECTORY, so after a prune it is unrecoverable.
//!
//! Two ways in, and both are answers to "is this card over?", which only this
//! side knows:
//!
//! - a card reaching a terminal column reaps its own stack, and says so on the
//!   card;
//! - a node's periodic inventory of the build stacks it holds is answered with
//!   the ones no unfinished card wants — which is what collects the stacks
//!   orphaned before this existed.
//!
//! The node never decides. It also never obeys blindly: every name here is one
//! `nook_proto::compose` derives from a build worktree's path, and the node
//! checks the shape again before running docker.

use std::collections::HashSet;

use nook_proto::ControlToNode;
use nook_types::*;

use crate::error::ApiResult;
use crate::events::{self, EventDraft};
use crate::state::AppState;

/// The card is over — Done or canceled — and its stack is dead weight.
fn terminal(column_type: &str) -> bool {
    matches!(column_type, "completed" | "canceled")
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

/// Bring down the stack the card's build worktree started, and report it on the
/// card when something actually came down (AC-7).
///
/// A no-op for a card with no worktree, a worktree that is not a build's, or a
/// repo whose builds boot nothing — which is most cards, and stays silent.
pub async fn reap_for_task(state: &AppState, tenant: TenantId, task_id: TaskId) -> ApiResult<()> {
    let Some(task) = state.tasks.get_row(tenant, task_id).await? else {
        return Ok(());
    };
    let Some((path, node_id)) = crate::services::taskwork::prune_target(state, &task).await? else {
        return Ok(());
    };
    let projects = nook_proto::compose::build_stack_projects(&path);
    if projects.is_empty() {
        return Ok(());
    }

    match reap_on_node(state, node_id, &projects).await {
        Ok(None) => Ok(()),
        Ok(Some(reaped)) => {
            report(state, tenant, task_id, &reaped).await;
            Ok(())
        }
        Err(e) => {
            // AC-4: a daemon that is absent, a node that is offline, a stack
            // somebody already took down. None of these is a problem with the
            // card, and the node's next inventory sweeps whatever survived.
            tracing::warn!(
                task = %task_id.0, node = %node_id.0, path = %path, error = %e,
                "could not bring the card's build stack down — the node's next \
                 stack inventory collects it"
            );
            Ok(())
        }
    }
}

/// Answer a node's inventory of the build stacks it holds (AC-5): anything no
/// unfinished card wants is orphaned, and is brought down.
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

/// "Where did my test database go" now has an answer on the card (AC-7).
async fn report(state: &AppState, tenant: TenantId, task_id: TaskId, reaped: &str) {
    let _ = state
        .tasks
        .create_comment(crate::repo::tasks::NewComment {
            tenant,
            task: task_id,
            author_type: "system".into(),
            author_id: None,
            author_name: "Stack reaper".into(),
            body_md: format!(
                "The card is finished, so its build stack was brought down with its \
                 volumes: `{reaped}`."
            ),
        })
        .await;
    events::record(
        state,
        tenant,
        EventDraft::new("task.build_stack_reaped")
            .payload(serde_json::json!({ "task_id": task_id, "projects": reaped })),
    )
    .await;
    state
        .registry
        .publish(tenant, nook_proto::UiEvent::TaskChanged { task_id });
}
