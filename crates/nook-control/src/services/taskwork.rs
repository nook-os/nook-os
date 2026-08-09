//! Kanban-driven work: triage dispatch, start-work (worktree + session),
//! submit-PR, prune. Shared by REST handlers and MCP tools so an AI can drive
//! the same lifecycle a human can.

use nook_proto::ControlToNode;
use nook_types::*;

use crate::error::{ApiError, ApiResult};
use crate::events::{self, EventDraft};
use crate::services::{identity::slugify, session_queries};
use crate::state::AppState;

/// The **present** checkout a working-directory path resolves to on a node
/// (MAIN-225) — the `node_workspaces` row a task's `checkout_id` binds to. `None`
/// until discovery has scanned a freshly-created worktree (never guessed), or for
/// an ad-hoc path with no checkout row. Mirrors `create_session_at`'s session
/// binding so a task and its session agree on which checkout they run in.
pub async fn present_checkout_at(
    repo: &dyn crate::repo::tasks::TaskRepository,
    node_id: NodeId,
    path: &str,
) -> ApiResult<Option<NodeWorkspaceId>> {
    repo.present_checkout_at(node_id, path).await
}

/// The `(path, node)` of the worktree `prune_worktree` should remove (MAIN-225
/// AC-4): the checkout `tasks.checkout_id` names among present rows when the task
/// has one, else the legacy `worktree_path`/`worktree_node_id` string pair. `None`
/// when the task has neither — the "no worktree to prune" case.
pub async fn prune_target(
    state: &AppState,
    task: &TaskItem,
) -> ApiResult<Option<(String, NodeId)>> {
    if let Some(checkout) = task.checkout_id {
        let row = state.tasks.checkout_location(checkout).await?;
        if let Some(pair) = row {
            return Ok(Some(pair));
        }
        // The id points at a checkout that has since gone missing — fall through
        // to the legacy strings, which item 7 has not yet retired.
    }
    Ok(match (task.worktree_path.clone(), task.worktree_node_id) {
        (Some(p), Some(n)) => Some((p, n)),
        _ => None,
    })
}

async fn load_task(state: &AppState, tenant: TenantId, id: TaskId) -> ApiResult<TaskItem> {
    state
        .tasks
        .get_row(tenant, id)
        .await?
        .ok_or(ApiError::NotFound)
}

/// Resolve a column on the task's board by name (case-insensitive), falling
/// back to a positional index when the name isn't found.
/// Find the column this lifecycle step should move a task to.
///
/// Tries the semantic TYPE first, then the name, then a position. Type first
/// is the point of having types at all: a board whose "Done" column somebody
/// renamed to "Shipped" still completes work correctly, and the name lookup
/// only remains as a fallback for a board whose types were never set.
async fn column_id(
    state: &AppState,
    board_id: BoardId,
    name: &str,
    fallback_pos: i32,
) -> ApiResult<ColumnId> {
    let wanted_type = match name.to_lowercase().as_str() {
        "triage" => Some("backlog"),
        "todo" => Some("unstarted"),
        "in progress" => Some("started"),
        "done" => Some("completed"),
        _ => None,
    };
    if let Some(t) = wanted_type {
        if let Some(id) = state.tasks.column_of_type(board_id, t).await? {
            return Ok(id);
        }
    }
    if let Some(id) = state.tasks.column_by_name(board_id, name).await? {
        return Ok(id);
    }
    state
        .tasks
        .column_at_position(board_id, fallback_pos.max(0) as i64)
        .await?
        .ok_or_else(|| ApiError::BadRequest("board has no columns".into()))
}

/// Triage → Todo: the scheduler picks the best online node by resources
/// (preferring one that already hosts the task's workspace, if any).
pub async fn dispatch(
    state: &AppState,
    tenant: TenantId,
    viewer: UserId,
    user: Option<UserId>,
    task_id: TaskId,
) -> ApiResult<TaskItem> {
    let task = load_task(state, tenant, task_id).await?;
    // MAIN-76 AC-9: a non-owner agent cannot act on a private card, even by id.
    if !crate::services::tasks::visible_to(&task, viewer) {
        return Err(ApiError::NotFound);
    }
    // Auto-placement is confined to the requester's OWN nodes (MAIN-131): `user`
    // is the acting identity (None on the MCP path, which then has no eligible
    // node rather than a tenant-wide pick), distinct from `viewer`, which the
    // MCP path fills with the tenant owner for visibility only.
    let placement = crate::services::schedule::pick(state, tenant, user, task.workspace_id).await?;
    let node = placement.node_id();
    // MAIN-227 AC-3: when the chosen node has no clone checkout of the workspace,
    // the node is still assigned but the outcome carries `needs_clone` — the
    // "clone it there first" decision is surfaced here, not as a late start-work
    // 400. Dispatch never auto-clones (NG-2).
    let needs_clone = placement.needs_clone();
    // Dispatch places, it does not relocate. Moving the card to Todo was
    // hardcoded and unconditional, so dispatching something already In Review
    // yanked it back into the queue — a mis-click cost an urgent ticket its
    // column. Where a card sits is the human's statement about its progress;
    // which machine should take it is a different question, and this answers
    // only that one. Use "send to board" to move a card.
    let mut updated = state
        .tasks
        .assign_node_and_column(task_id, node, None)
        .await?;
    updated.needs_clone = needs_clone;

    events::record(
        state,
        tenant,
        EventDraft::new("task.dispatched")
            // Attribute the dispatch to its acting user, like every other
            // lifecycle event, so activity scoping can place it (MAIN-134 AC-3).
            .actor("user", viewer.0)
            .payload(
                serde_json::json!({ "task_id": task_id, "node_id": node, "needs_clone": needs_clone }),
            ),
    )
    .await;
    fire_automation(state, tenant, &task, &updated).await;
    Ok(updated)
}

/// Fire board automation for a taskwork move (MAIN-73). One helper so every
/// move site reads the same and the no-op-on-same-column rule lives in one place.
async fn fire_automation(state: &AppState, tenant: TenantId, before: &TaskItem, after: &TaskItem) {
    crate::services::triggers::on_column_change(
        state,
        tenant,
        after.id,
        after.board_id,
        before.column_id,
        after.column_id,
    )
    .await;
}

pub struct StartWork {
    pub node_id: Option<NodeId>,
    pub runtime: String,
    pub branch: Option<String>,
    pub workspace_id: Option<WorkspaceId>,
}

/// Todo → In Progress: create a worktree for the task's branch and start a
/// session in it. Returns the linked task and its session.
pub async fn start_work(
    state: &AppState,
    tenant: TenantId,
    viewer: UserId,
    user: Option<UserId>,
    task_id: TaskId,
    req: StartWork,
) -> ApiResult<(TaskItem, Session)> {
    let task = load_task(state, tenant, task_id).await?;
    // MAIN-76 AC-9: a non-owner agent cannot start-work a private card by id.
    if !crate::services::tasks::visible_to(&task, viewer) {
        return Err(ApiError::NotFound);
    }
    if task.worktree_path.is_some() {
        return Err(ApiError::Conflict(
            "work already started for this task".into(),
        ));
    }
    let node_id = req
        .node_id
        .or(task.assigned_node_id)
        .ok_or_else(|| ApiError::BadRequest("no node — dispatch the task or pick one".into()))?;

    // Spawn authorization (MAIN-130, relaxed for shared nodes in MAIN-136): a
    // session starts on a node the acting PERSON owns OR one shared with the
    // team — the one chokepoint both the HTTP route and MCP reach. `user` is
    // None on the MCP path, which is refused until it carries a per-user
    // identity (AC-3); when auto-dispatch (MAIN-131) hands over a node the
    // requester does not own, an unshared node still 403s here (AC-6). Runs
    // before the worktree op so an unauthorized start leaves nothing behind.
    crate::auth::require_person_may_use_node(state, tenant, user, node_id).await?;

    let workspace_id = req
        .workspace_id
        .or(task.workspace_id)
        .ok_or_else(|| ApiError::BadRequest("link a workspace first (clone or pick one)".into()))?;
    let branch = req
        .branch
        .filter(|b| !b.trim().is_empty())
        .unwrap_or_else(|| slugify(&task.title));

    // A checkout of this workspace must exist on the node to worktree from.
    let repo_path = state
        .tasks
        .clone_path_on_node(tenant, workspace_id, node_id)
        .await?;
    let Some(repo_path) = repo_path else {
        return Err(ApiError::BadRequest(
            "that workspace has no checkout on that node — clone it there first".into(),
        ));
    };

    // Create the worktree for the task branch.
    let rx = state
        .registry
        .request_op(node_id, |request_id| ControlToNode::AddWorktree {
            request_id,
            repo_path,
            branch: branch.clone(),
        })
        .ok_or_else(|| ApiError::BadRequest("node is offline".into()))?;
    let op = tokio::time::timeout(std::time::Duration::from_secs(30), rx)
        .await
        .map_err(|_| ApiError::BadRequest("node did not answer in time".into()))?
        .map_err(|_| ApiError::BadRequest("node disconnected".into()))?;
    if !op.ok {
        return Err(ApiError::BadRequest(format!(
            "worktree failed: {}",
            op.message
        )));
    }
    let worktree_path = op
        .path
        .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("node returned no worktree path")))?;

    // Start a session pinned to the new worktree checkout.
    let session = session_queries::create_session_at(
        state,
        tenant,
        user,
        workspace_id,
        node_id,
        &req.runtime,
        Some(task.title.clone()),
        &worktree_path,
        // Start-work is a person taking a ticket: ad-hoc, not reconciled.
        false,
        ManagedPurpose::Access,
        nook_types::ShardAssignment::SOLO,
    )
    .await?;

    // The checkout this task's working directory now IS (MAIN-225 AC-3): resolved
    // from the worktree path the same present-rows way `create_session_at` binds a
    // session's checkout. NULL when discovery has not yet scanned the just-created
    // worktree — never guessed. The legacy strings are still written in the same
    // UPDATE, so no existing reader regresses (NG-1).
    let checkout_id = present_checkout_at(state.tasks.as_ref(), node_id, &worktree_path).await?;

    let in_progress = column_id(state, task.board_id, "In Progress", 2).await?;
    let updated = state
        .tasks
        .record_started_work(
            task_id,
            crate::repo::tasks::StartedWork {
                workspace_id: workspace_id.0,
                node_id,
                branch: branch.clone(),
                worktree_path: worktree_path.clone(),
                session_id: Some(session.id.0),
                column_id: in_progress,
                checkout_id: checkout_id.map(|c| c.0),
                // The lease that makes this an agent claim (MAIN-229): the
                // backstop cap, not a renewal interval — session liveness is
                // what actually renews it.
                claim_ttl_secs: state.cfg.max_claim_secs as i64,
            },
        )
        .await?;

    events::record(
        state,
        tenant,
        EventDraft::new("task.work_started")
            .workspace(workspace_id)
            .node(node_id)
            .session(session.id)
            .payload(serde_json::json!({ "task_id": task_id, "branch": branch })),
    )
    .await;
    fire_automation(state, tenant, &task, &updated).await;
    Ok((updated, session))
}

/// In Progress → In Review: record the PR (given or derived from the remote)
/// and park the card in the board's review stage, not Done — so "Done" can mean
/// merged and review has a home (MAIN-71).
pub async fn submit_pr(
    state: &AppState,
    tenant: TenantId,
    task_id: TaskId,
    pr_url: Option<String>,
) -> ApiResult<TaskItem> {
    let task = load_task(state, tenant, task_id).await?;
    let branch = task
        .branch
        .clone()
        .ok_or_else(|| ApiError::BadRequest("task has no branch — start work first".into()))?;

    let url = match pr_url.filter(|u| !u.trim().is_empty()) {
        Some(u) => u,
        None => derive_pr_url(state, &task, &branch)
            .await
            .unwrap_or_else(|| format!("(no remote) branch {branch}")),
    };

    // By TYPE, not name: a submitted PR parks in the board's `review` column,
    // falling back to `completed` only for a board that somehow has no review
    // column (one that predates this and missed the backfill).
    let target =
        match crate::services::tasks::column_of_type(state.tasks.as_ref(), task.board_id, "review")
            .await
        {
            Ok(id) => id,
            Err(_) => {
                crate::services::tasks::column_of_type(
                    state.tasks.as_ref(),
                    task.board_id,
                    "completed",
                )
                .await?
            }
        };
    let updated = state.tasks.set_pr_url(task_id, &url, target).await?;

    events::record(
        state,
        tenant,
        EventDraft::new("task.pr_submitted")
            .payload(serde_json::json!({ "task_id": task_id, "pr_url": url })),
    )
    .await;
    fire_automation(state, tenant, &task, &updated).await;
    Ok(updated)
}

/// Done → prune: remove the task's worktree checkout.
pub async fn prune_worktree(
    state: &AppState,
    tenant: TenantId,
    task_id: TaskId,
) -> ApiResult<TaskItem> {
    let task = load_task(state, tenant, task_id).await?;
    // Address the worktree by ID when the task has one — the path is an attribute
    // of that checkout row (MAIN-225 AC-4) — falling back to the legacy strings
    // otherwise.
    let Some((path, node_id)) = prune_target(state, &task).await? else {
        return Err(ApiError::BadRequest("task has no worktree to prune".into()));
    };

    // An unreachable node must not strand the card (MAIN-480 AC-6). The record
    // is what PINS a build card's later passes to this node, so refusing to
    // clear it while the machine is down would leave the work unplaceable with
    // no way out. The directory is not lost: the node reports what it holds on
    // its next connect and is told to remove anything no card records.
    let removal = remove_worktree_on_node(state, node_id, &path).await;
    let unreachable = match removal {
        Ok(()) => None,
        Err(e) => {
            tracing::warn!(
                task = %task_id.0, node = %node_id.0, path = %path, error = %e,
                "pruning the worktree on the node failed — clearing the record anyway; \
                 the node removes the directory when it next reports what it holds"
            );
            Some(e)
        }
    };

    let updated = state.tasks.clear_worktree(task_id).await?;

    events::record(
        state,
        tenant,
        EventDraft::new("task.worktree_pruned").payload(serde_json::json!({
            "task_id": task_id,
            "path": path,
            "node_unreachable": unreachable.is_some(),
        })),
    )
    .await;
    Ok(updated)
}

/// Ask the node to remove a worktree, waiting for its answer.
///
/// Every failure is one value — offline, silent, disconnected, or a refusal
/// from git itself — because the caller treats them identically: the record is
/// cleared regardless and the reason is logged.
async fn remove_worktree_on_node(
    state: &AppState,
    node_id: NodeId,
    path: &str,
) -> Result<(), String> {
    let rx = state
        .registry
        .request_op(node_id, |request_id| ControlToNode::RemoveWorktree {
            request_id,
            worktree_path: path.to_string(),
        })
        .ok_or_else(|| "node is offline".to_string())?;
    let op = tokio::time::timeout(std::time::Duration::from_secs(30), rx)
        .await
        .map_err(|_| "node did not answer in time".to_string())?
        .map_err(|_| "node disconnected".to_string())?;
    if op.ok {
        Ok(())
    } else {
        Err(format!("prune failed: {}", op.message))
    }
}

/// Push a claim's backstop out to `max_claim_secs` from now (MAIN-229 AC-5).
///
/// A **seam, not a requirement**: session liveness is what actually keeps a card
/// alive (AC-3), and nothing in the build/review skills has to call this. It
/// exists so a future supervisor can extend the cap on a card it is actively
/// shepherding, rather than watch it get escalated for being slow.
///
/// Authorized to the claim holder, at the same grade as claiming it: the
/// assignee themself, or the node the work was placed on acting for itself.
pub async fn renew_claim(
    state: &AppState,
    tenant: TenantId,
    viewer: UserId,
    principal: crate::auth::Principal,
    task_id: TaskId,
) -> ApiResult<TaskItem> {
    let task = load_task(state, tenant, task_id).await?;
    if !crate::services::tasks::visible_to(&task, viewer) {
        return Err(ApiError::NotFound);
    }
    let Some(holder) = task.assignee_user_id else {
        return Err(ApiError::BadRequest(
            "task is not claimed — nothing to renew".into(),
        ));
    };
    let held_by_caller = match principal {
        crate::auth::Principal::User => holder == viewer,
        crate::auth::Principal::Node(node) => task.assigned_node_id == Some(node),
    };
    if !held_by_caller {
        return Err(ApiError::ForbiddenMsg(
            "only the claim holder can renew this claim".into(),
        ));
    }

    match state
        .tasks
        .renew_claim(task_id, tenant, state.cfg.max_claim_secs as i64)
        .await?
    {
        Some(Some(renewed)) => Ok(renewed),
        // The row is there but carries no lease — a card that reached In
        // Progress by hand, or one already requeued. Renewing it would mint a
        // lease the reaper never granted, so it is refused rather than created.
        Some(None) => Err(ApiError::Conflict(
            "task carries no claim lease — only an agent claim can be renewed".into(),
        )),
        None => Err(ApiError::NotFound),
    }
}

/// Move a task to a named column (drives the board from MCP/AI).
pub async fn move_task(
    state: &AppState,
    tenant: TenantId,
    task_id: TaskId,
    column: &str,
) -> ApiResult<TaskItem> {
    let task = load_task(state, tenant, task_id).await?;
    let col = column_id(state, task.board_id, column, 0).await?;
    let updated = state.tasks.set_column(task_id, col).await?;
    fire_automation(state, tenant, &task, &updated).await;
    Ok(updated)
}

/// Best-effort compare/MR URL from the worktree's git remote.
async fn derive_pr_url(state: &AppState, task: &TaskItem, branch: &str) -> Option<String> {
    let raw: String = state
        .tasks
        .git_remote_for_workspace(task.tenant_id, task.workspace_id?)
        .await
        .ok()??;
    // Normalize to https host/path (reuse the discovery normalizer).
    let norm = crate::services::discovery::normalize_remote(&raw); // e.g. github.com/org/repo
    let https = format!("https://{norm}");
    if norm.contains("github.com") {
        Some(format!("{https}/compare/{branch}?expand=1"))
    } else if norm.contains("gitlab") {
        Some(format!(
            "{https}/-/merge_requests/new?merge_request%5Bsource_branch%5D={branch}"
        ))
    } else if norm.contains("bitbucket") {
        Some(format!("{https}/pull-requests/new?source={branch}"))
    } else {
        Some(https)
    }
}
