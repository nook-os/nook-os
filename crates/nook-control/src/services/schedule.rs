//! Resource-aware node placement (the "Auto" default). Wraps
//! `nook_dispatcher::pick_node` with the online-node + workspace-affinity
//! logic shared by triage dispatch and the New Work "Auto" option.

use nook_types::{NodeId, NodeResources, TenantId, WorkspaceId};

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Online nodes with their latest resource sample.
pub async fn online_nodes(
    state: &AppState,
    tenant: TenantId,
) -> ApiResult<Vec<(NodeId, NodeResources)>> {
    let rows: Vec<(NodeId, serde_json::Value)> =
        sqlx::query_as("SELECT id, resources FROM nodes WHERE tenant_id = $1")
            .bind(tenant)
            .fetch_all(&state.db)
            .await?;
    Ok(rows
        .into_iter()
        .filter(|(id, _)| state.registry.node_online(*id))
        .map(|(id, res)| (id, serde_json::from_value(res).unwrap_or_default()))
        .collect())
}

/// Pick the best online node. When a workspace is given, prefer nodes that
/// already have it checked out (so worktrees/sessions land where the repo is);
/// otherwise rank across all online nodes.
pub async fn pick(
    state: &AppState,
    tenant: TenantId,
    workspace: Option<WorkspaceId>,
) -> ApiResult<NodeId> {
    let all = online_nodes(state, tenant).await?;
    if all.is_empty() {
        return Err(ApiError::BadRequest("no online node available".into()));
    }

    if let Some(ws) = workspace {
        let hosts: Vec<(NodeId,)> = sqlx::query_as(
            "SELECT DISTINCT node_id FROM node_workspaces WHERE tenant_id = $1 AND workspace_id = $2",
        )
        .bind(tenant)
        .bind(ws)
        .fetch_all(&state.db)
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

    nook_dispatcher::pick_node(&all)
        .ok_or_else(|| ApiError::BadRequest("no online node available".into()))
}
