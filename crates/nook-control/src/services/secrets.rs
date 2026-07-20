//! Workspace secret sync: encrypted .env files stored in the vault, pushed
//! to every checkout of the workspace on online nodes. Cloning a repo on a
//! new machine brings its secrets along automatically.

use base64::Engine;
use nook_proto::ControlToNode;
use nook_types::{NodeId, TenantId, WorkspaceId};

use crate::error::ApiResult;
use crate::state::AppState;

fn b64(content: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(content)
}

/// Push all of a workspace's secrets to one checkout (fire-and-forget).
pub async fn push_to_location(
    state: &AppState,
    tenant: TenantId,
    workspace: WorkspaceId,
    node_id: NodeId,
    checkout_path: &str,
) -> ApiResult<usize> {
    let rows: Vec<(String, Vec<u8>)> = sqlx::query_as(
        "SELECT name, content_enc FROM workspace_secrets
         WHERE tenant_id = $1 AND workspace_id = $2",
    )
    .bind(tenant)
    .bind(workspace)
    .fetch_all(&state.db)
    .await?;

    let mut pushed = 0;
    for (name, enc) in rows {
        let Ok(content) = state.vault.decrypt(&enc) else {
            tracing::error!(%workspace, name, "secret decryption failed — check SECRETS_KEY");
            continue;
        };
        if state.registry.send_to_node(
            node_id,
            ControlToNode::WriteWorkspaceFile {
                checkout_path: checkout_path.to_string(),
                name,
                content_b64: b64(&content),
            },
        ) {
            pushed += 1;
        }
    }
    Ok(pushed)
}

/// Push all secrets of a workspace to every checkout on online nodes.
pub async fn push_everywhere(
    state: &AppState,
    tenant: TenantId,
    workspace: WorkspaceId,
) -> ApiResult<usize> {
    let locations: Vec<(NodeId, String)> = sqlx::query_as(
        "SELECT node_id, path FROM node_workspaces
         WHERE tenant_id = $1 AND workspace_id = $2",
    )
    .bind(tenant)
    .bind(workspace)
    .fetch_all(&state.db)
    .await?;

    let mut total = 0;
    for (node_id, path) in locations {
        if state.registry.node_online(node_id) {
            total += push_to_location(state, tenant, workspace, node_id, &path).await?;
        }
    }
    Ok(total)
}
