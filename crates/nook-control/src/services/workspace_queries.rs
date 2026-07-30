//! Workspace reads (MAIN-245).
//!
//! Split out of `services/core.rs`, which had grown into a grab-bag of queries
//! for every aggregate at once. The repository chain needs each aggregate to own
//! its files exclusively, so the later `WorkspaceRepository` card has one place
//! to change. The moves are verbatim — same SQL, same signatures.

use nook_db::{params, Db, DbPool};
use nook_types::*;

use crate::error::ApiResult;

pub async fn workspace_locations(
    db: &DbPool,
    tenant: TenantId,
    workspace: WorkspaceId,
) -> ApiResult<Vec<WorkspaceLocation>> {
    let rows: Vec<(
        NodeId,
        String,
        String,
        String,
        Option<String>,
        serde_json::Value,
    )> = db
        .query_all(
            "SELECT n.id, n.name, n.status, nw.path, nw.git_branch, nw.git_status
             FROM node_workspaces nw
             JOIN nodes n ON n.id = nw.node_id
             WHERE nw.tenant_id = $1 AND nw.workspace_id = $2
             ORDER BY n.name",
            params![tenant, workspace],
        )
        .await?;
    Ok(rows
        .into_iter()
        .map(
            |(node_id, node_name, node_status, path, git_branch, git_status)| WorkspaceLocation {
                node_id,
                node_name,
                node_status,
                path,
                git_branch,
                dirty: git_status
                    .get("dirty")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                worktree: git_status
                    .get("worktree")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            },
        )
        .collect())
}

pub async fn list_workspaces(db: &DbPool, tenant: TenantId) -> ApiResult<Vec<WorkspaceDetail>> {
    let workspaces: Vec<Workspace> = db
        .query_all(
            "SELECT * FROM workspaces WHERE tenant_id = $1 ORDER BY name",
            params![tenant],
        )
        .await?;
    let mut out = Vec::with_capacity(workspaces.len());
    for workspace in workspaces {
        let locations = workspace_locations(db, tenant, workspace.id).await?;
        out.push(WorkspaceDetail {
            workspace,
            locations,
        });
    }
    Ok(out)
}

pub async fn get_workspace(
    db: &DbPool,
    tenant: TenantId,
    id: WorkspaceId,
) -> ApiResult<Option<WorkspaceDetail>> {
    let workspace: Option<Workspace> = db
        .query_opt(
            "SELECT * FROM workspaces WHERE tenant_id = $1 AND id = $2",
            params![tenant, id],
        )
        .await?;
    match workspace {
        None => Ok(None),
        Some(workspace) => {
            let locations = workspace_locations(db, tenant, workspace.id).await?;
            Ok(Some(WorkspaceDetail {
                workspace,
                locations,
            }))
        }
    }
}

/// Resolve a workspace by **id or slug** (both unique), falling back to name
/// as a documented convenience that errors on ambiguity rather than silently
/// picking one (MAIN-223 AC-3). The old `slug = $2 OR name = $2` conflated the
/// two and returned an arbitrary row when a name matched several workspaces.
///
/// Moved verbatim out of `mcp_backend` (MAIN-245), `anyhow::Result` and all —
/// converting it to `ApiResult` would be the signature redesign NG-2 forbids.
pub async fn resolve_by_key(
    db: &DbPool,
    tenant: TenantId,
    key: &str,
) -> anyhow::Result<WorkspaceId> {
    // Unique key #1: the id.
    if let Ok(uuid) = key.parse::<uuid::Uuid>() {
        if let Some(id) = db
            .query_scalar_opt::<WorkspaceId>(
                "SELECT id FROM workspaces WHERE tenant_id = $1 AND id = $2",
                params![tenant, WorkspaceId(uuid)],
            )
            .await?
        {
            return Ok(id);
        }
    }
    // Unique key #2: the slug.
    if let Some(id) = db
        .query_scalar_opt::<WorkspaceId>(
            "SELECT id FROM workspaces WHERE tenant_id = $1 AND slug = $2",
            params![tenant, key],
        )
        .await?
    {
        return Ok(id);
    }
    // Convenience: a bare name, which is NOT unique. One match resolves; more
    // than one is an error that names the slugs to disambiguate with.
    let matches: Vec<(WorkspaceId, String)> = db
        .query_all(
            "SELECT id, slug FROM workspaces WHERE tenant_id = $1 AND name = $2 ORDER BY slug",
            params![tenant, key],
        )
        .await?;
    match matches.as_slice() {
        [(id, _)] => Ok(*id),
        [] => Err(anyhow::anyhow!(
            "no workspace with id, slug, or name '{key}'"
        )),
        many => {
            let slugs = many
                .iter()
                .map(|(_, slug)| slug.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            Err(anyhow::anyhow!(
                "'{key}' names {} workspaces — use a unique slug: {slugs}",
                many.len()
            ))
        }
    }
}

/// The path of a workspace's **clone** on one node. Moved verbatim out of
/// `mcp_backend` (MAIN-245).
pub async fn clone_path_on_node(
    db: &DbPool,
    tenant: TenantId,
    workspace_id: WorkspaceId,
    node_id: NodeId,
) -> ApiResult<Option<String>> {
    Ok(db
        .query_scalar_opt(
            // MAIN-222 AC-3: worktree from the CLONE only, deterministically.
            "SELECT path FROM node_workspaces
             WHERE tenant_id = $1 AND workspace_id = $2 AND node_id = $3
               AND kind = 'clone' AND missing_at IS NULL
             ORDER BY discovered_at LIMIT 1",
            params![tenant, workspace_id, node_id],
        )
        .await?)
}
