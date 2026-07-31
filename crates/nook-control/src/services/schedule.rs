//! Resource-aware node placement (the "Auto" default). Wraps
//! `nook_dispatcher::pick_node` with the online-node + workspace-affinity
//! logic shared by triage dispatch and the New Workspace "Auto" option.
//!
//! Placement is confined to the requester's OWN machines (MAIN-131): a session
//! only ever starts on a node you own (MAIN-130), so auto-dispatch must not
//! route work onto a teammate's node — that would only manufacture a guaranteed
//! 403 at start-work. The ownership filter here is the up-front half of that
//! rule; the spawn guard is the enforcing half.

use nook_db::{params, Db};
use nook_types::{NodeId, NodeResources, NodeWorkspaceId, TenantId, UserId, WorkspaceId};
use uuid::Uuid;

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// The result of placement (MAIN-227): an explicit outcome, never a bare node
/// that might have no checkout. `dispatch` and the New Workspace "Auto" picker both
/// consume this instead of guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// An owned, online node that can host the work now: it has a `kind='clone'`,
    /// present checkout of the workspace (`checkout_id = Some`), or the task has
    /// no workspace at all (`checkout_id = None`, rank-only — needs-clone never
    /// applies).
    Placed {
        node_id: NodeId,
        checkout_id: Option<NodeWorkspaceId>,
    },
    /// The best owned online node was chosen, but it has no clone checkout of the
    /// workspace — the caller must clone there first. Surfaced at dispatch time
    /// (AC-3) instead of as a late start-work 400.
    NeedsClone { node_id: NodeId },
}

impl Placement {
    /// The node placement chose, whichever outcome.
    pub fn node_id(&self) -> NodeId {
        match self {
            Placement::Placed { node_id, .. } | Placement::NeedsClone { node_id } => *node_id,
        }
    }

    /// True only for the needs-clone outcome — what `dispatch` records.
    pub fn needs_clone(&self) -> bool {
        matches!(self, Placement::NeedsClone { .. })
    }
}

/// The PRESENT clone checkouts of a workspace, keyed node → checkout id (first
/// checkout per node, by discovery order). A `worktree` row, or a tombstoned
/// (`missing_at` set) clone, is deliberately excluded — only a live clone makes a
/// node a placement host (MAIN-227 AC-2, the worktree-from-clone-only invariant).
pub async fn clone_hosts(
    db: &nook_db::DbPool,
    tenant: TenantId,
    workspace: WorkspaceId,
) -> ApiResult<std::collections::HashMap<NodeId, NodeWorkspaceId>> {
    let hosts: Vec<(NodeId, NodeWorkspaceId)> = db
        .query_all(
            "SELECT node_id, id FROM node_workspaces
             WHERE tenant_id = $1 AND workspace_id = $2
               AND kind = 'clone' AND missing_at IS NULL
             ORDER BY discovered_at",
            params![tenant, workspace],
        )
        .await?;
    let mut map = std::collections::HashMap::new();
    for (node, checkout) in hosts {
        map.entry(node).or_insert(checkout);
    }
    Ok(map)
}

/// The person behind a user — the identity a node's `owner_person_id` is keyed
/// on. Resolved locally so scheduling carries no dependency on other modules'
/// identity plumbing.
async fn person_of(state: &AppState, user: UserId) -> ApiResult<Option<Uuid>> {
    let row: Option<(Uuid,)> = state
        .db
        .query_opt("SELECT person_id FROM users WHERE id = $1", params![user])
        .await?;
    Ok(row.map(|(p,)| p))
}

/// Online nodes **owned by `person`**, with their latest resource sample. The
/// ownership predicate is the candidate gate (MAIN-131): a node someone else
/// owns is never a candidate, however well-resourced or idle it is.
async fn owned_online_nodes(
    state: &AppState,
    tenant: TenantId,
    person: Uuid,
) -> ApiResult<Vec<(NodeId, NodeResources)>> {
    let rows: Vec<(NodeId, serde_json::Value)> = state
        .db
        .query_all(
            "SELECT id, resources FROM nodes WHERE tenant_id = $1 AND owner_person_id = $2",
            params![tenant, person],
        )
        .await?;
    Ok(rows
        .into_iter()
        .filter(|(id, _)| state.registry.node_online(*id))
        .map(|(id, res)| (id, serde_json::from_value(res).unwrap_or_default()))
        .collect())
}

/// The error a caller surfaces when placement finds nothing: no online node the
/// requester owns. One message for every dead end — no acting person (MCP), no
/// owned node at all, or none of them online — because they are the same
/// outcome to the person waiting: get a machine of your own online first.
fn no_eligible() -> ApiError {
    ApiError::BadRequest("no eligible node of yours is online".into())
}

/// Pick the best online node the acting person owns. When a workspace is given,
/// prefer owned nodes that already have it checked out (so worktrees/sessions
/// land where the repo is); otherwise rank across all owned online nodes.
///
/// `actor` is the acting user, or `None` when the caller has no per-user
/// identity — today only the MCP dispatch path, whose work is unattributed.
/// A `None` actor has no person and therefore no eligible node: it is refused,
/// never widened to tenant-wide selection (MAIN-131 AC-2).
pub async fn pick(
    state: &AppState,
    tenant: TenantId,
    actor: Option<UserId>,
    workspace: Option<WorkspaceId>,
) -> ApiResult<Placement> {
    let Some(person) = (match actor {
        Some(u) => person_of(state, u).await?,
        None => None,
    }) else {
        return Err(no_eligible());
    };

    let all = owned_online_nodes(state, tenant, person).await?;
    if all.is_empty() {
        return Err(no_eligible());
    }

    // No workspace: rank across owned online nodes exactly as before; there is no
    // checkout affinity and needs-clone does not apply (AC-3).
    let Some(ws) = workspace else {
        let node_id = nook_dispatcher::pick_node(&all).ok_or_else(no_eligible)?;
        return Ok(Placement::Placed {
            node_id,
            checkout_id: None,
        });
    };

    // A node HOSTS the workspace only when it has a `kind='clone'`, present
    // checkout of it (MAIN-227 AC-2): a worktree, or a tombstoned clone, is not a
    // placement host — the worktree-from-clone-only invariant.
    let clone_at = clone_hosts(&state.db, tenant, ws).await?;

    // Prefer an owned online node that already hosts a clone; rank only within
    // that set (ownership-filtered `all`).
    let among: Vec<(NodeId, NodeResources)> = all
        .iter()
        .filter(|(id, _)| clone_at.contains_key(id))
        .cloned()
        .collect();
    if let Some(node_id) = nook_dispatcher::pick_node(&among) {
        return Ok(Placement::Placed {
            node_id,
            checkout_id: clone_at.get(&node_id).copied(),
        });
    }

    // None of the owned online nodes hosts a clone: name the best one to clone
    // onto — an EXPLICIT needs-clone, not a silent placement that fails later.
    let node_id = nook_dispatcher::pick_node(&all).ok_or_else(no_eligible)?;
    Ok(Placement::NeedsClone { node_id })
}
