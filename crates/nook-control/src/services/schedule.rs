//! Resource-aware node placement (the "Auto" default). Wraps
//! `nook_dispatcher::pick_node` with the online-node + workspace-affinity
//! logic shared by triage dispatch and the New Work "Auto" option.
//!
//! Placement is confined to the requester's OWN machines (MAIN-131): a session
//! only ever starts on a node you own (MAIN-130), so auto-dispatch must not
//! route work onto a teammate's node — that would only manufacture a guaranteed
//! 403 at start-work. The ownership filter here is the up-front half of that
//! rule; the spawn guard is the enforcing half.

use nook_db::{params, Db};
use nook_types::{NodeId, NodeResources, TenantId, UserId, WorkspaceId};
use uuid::Uuid;

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

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
) -> ApiResult<NodeId> {
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

    // Workspace affinity, preserved WITHIN the owned set: prefer an owned node
    // that hosts the checkout, but never fall back to an unowned one — `among`
    // and the final ranking both draw only from `all`, which is already
    // ownership-filtered (AC-3).
    if let Some(ws) = workspace {
        let hosts: Vec<(NodeId,)> = state
            .db
            .query_all(
                "SELECT DISTINCT node_id FROM node_workspaces
                 WHERE tenant_id = $1 AND workspace_id = $2 AND missing_at IS NULL",
                params![tenant, ws],
            )
            .await?;
        let host_set: std::collections::HashSet<NodeId> =
            hosts.into_iter().map(|(id,)| id).collect();
        let among: Vec<(NodeId, NodeResources)> = all
            .iter()
            .filter(|(id, _)| host_set.contains(id))
            .cloned()
            .collect();
        if let Some(node) = nook_dispatcher::pick_node(&among) {
            return Ok(node);
        }
    }

    nook_dispatcher::pick_node(&all).ok_or_else(no_eligible)
}
