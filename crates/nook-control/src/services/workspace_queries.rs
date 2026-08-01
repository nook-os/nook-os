//! Workspace reads (MAIN-245, moved behind [`WorkspaceRepository`] by
//! MAIN-251).
//!
//! What is left here is composition, not data access: a detail view is a
//! workspace plus its locations, and resolving a user-typed key is a decision
//! about which error to raise. The queries themselves live in
//! `crate::repo::workspaces`.

use nook_types::*;

use crate::error::ApiResult;
use crate::repo::workspaces::{KeyMatch, WorkspaceRepository};

pub async fn workspace_locations(
    repo: &dyn WorkspaceRepository,
    tenant: TenantId,
    workspace: WorkspaceId,
) -> ApiResult<Vec<WorkspaceLocation>> {
    repo.locations(tenant, workspace).await
}

pub async fn list_workspaces(
    repo: &dyn WorkspaceRepository,
    tenant: TenantId,
) -> ApiResult<Vec<WorkspaceDetail>> {
    let workspaces = repo.list(tenant).await?;
    let mut out = Vec::with_capacity(workspaces.len());
    for workspace in workspaces {
        let locations = repo.locations(tenant, workspace.id).await?;
        out.push(WorkspaceDetail {
            workspace,
            locations,
        });
    }
    Ok(out)
}

/// The workspaces table, through the pagination contract: searched
/// (name/slug/remote), sorted (name/created), cursor-walked — each page row
/// enriched with its locations, same as the whole-list read.
pub async fn workspaces_page(
    repo: &dyn WorkspaceRepository,
    tenant: TenantId,
    wire: &PageQuery,
) -> ApiResult<Page<WorkspaceDetail>> {
    let args = wire
        .args(crate::repo::workspaces::WORKSPACE_SORTS)
        .map_err(crate::services::operator_queries::bad_page)?;
    let page = repo.page(tenant, &args).await?;
    let mut rows = Vec::with_capacity(page.rows.len());
    for workspace in page.rows {
        let locations = repo.locations(tenant, workspace.id).await?;
        rows.push(WorkspaceDetail {
            workspace,
            locations,
        });
    }
    Ok(Page {
        rows,
        next_cursor: page.next_cursor,
    })
}

pub async fn get_workspace(
    repo: &dyn WorkspaceRepository,
    tenant: TenantId,
    id: WorkspaceId,
) -> ApiResult<Option<WorkspaceDetail>> {
    match repo.get(tenant, id).await? {
        None => Ok(None),
        Some(workspace) => {
            let locations = repo.locations(tenant, workspace.id).await?;
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
/// Keeps `anyhow::Result` — it is the MCP backend's error type, and converting
/// it would be the signature redesign NG-2 forbids.
pub async fn resolve_by_key(
    repo: &dyn WorkspaceRepository,
    tenant: TenantId,
    key: &str,
) -> anyhow::Result<WorkspaceId> {
    match repo.resolve_key(tenant, key).await? {
        KeyMatch::One(id) => Ok(id),
        KeyMatch::None => Err(anyhow::anyhow!(
            "no workspace with id, slug, or name '{key}'"
        )),
        KeyMatch::Ambiguous(slugs) => Err(anyhow::anyhow!(
            "'{key}' names {} workspaces — use a unique slug: {}",
            slugs.len(),
            slugs.join(", ")
        )),
    }
}

/// The path of a workspace's **clone** on one node (MAIN-222 AC-3).
pub async fn clone_path_on_node(
    repo: &dyn WorkspaceRepository,
    tenant: TenantId,
    workspace_id: WorkspaceId,
    node_id: NodeId,
) -> ApiResult<Option<String>> {
    repo.clone_path(tenant, workspace_id, node_id).await
}
