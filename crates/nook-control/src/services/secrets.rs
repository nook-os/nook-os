//! Workspace secret sync: encrypted .env files stored in the vault, pushed
//! to every checkout of the workspace on online nodes. Cloning a repo on a
//! new machine brings its secrets along automatically.

use base64::Engine;
use nook_proto::ControlToNode;
use nook_types::{NodeId, SessionId, TenantId, WorkspaceId};

use crate::state::AppState;

fn b64(content: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(content)
}

/// Tell everyone watching that a workspace gained a checkout.
///
/// The control plane can't sync secrets to it: every secret is sealed with a
/// password the server never sees, which is exactly the property we want. So
/// it announces instead, and a browser that is already unlocked replays the
/// unlock — that's what actually writes the file (see `push_one`). Nothing is
/// delivered until a human has proved they hold the password.
pub async fn announce_new_checkout(
    state: &AppState,
    tenant: TenantId,
    workspace: WorkspaceId,
    node_id: NodeId,
    checkout_path: &str,
) {
    let has_secrets: Option<(i64,)> = sqlx::query_as(
        "SELECT count(*) FROM workspace_secrets WHERE tenant_id = $1 AND workspace_id = $2",
    )
    .bind(tenant)
    .bind(workspace)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();
    if !has_secrets.is_some_and(|(n,)| n > 0) {
        return;
    }
    crate::events::record(
        state,
        tenant,
        crate::events::EventDraft::new("workspace.checkout_added")
            .actor("node", node_id.0)
            .workspace(workspace)
            .node(node_id)
            .payload(serde_json::json!({ "path": checkout_path })),
    )
    .await;
}

/// Remove ephemeral secret files from a session's checkout once it ends.
///
/// The point of an ephemeral secret is that plaintext lives on disk only
/// while something is actually using it — the encrypted copy stays in the
/// vault and comes back on the next sync. Other live sessions in the same
/// checkout keep their files.
pub async fn wipe_ephemeral_for_session(state: &AppState, tenant: TenantId, session_id: SessionId) {
    let Ok(Some((workspace_id, node_id))) = sqlx::query_as::<_, (WorkspaceId, NodeId)>(
        "SELECT workspace_id, node_id FROM sessions WHERE id = $1 AND tenant_id = $2",
    )
    .bind(session_id)
    .bind(tenant)
    .fetch_optional(&state.db)
    .await
    else {
        return;
    };

    // Another live session still needs the files.
    let still_live: Option<(i64,)> = sqlx::query_as(
        "SELECT count(*) FROM sessions
         WHERE workspace_id = $1 AND id <> $2
           AND status IN ('starting', 'running', 'detached')",
    )
    .bind(workspace_id)
    .bind(session_id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();
    if still_live.is_some_and(|(n,)| n > 0) {
        return;
    }

    let names: Vec<(String,)> =
        sqlx::query_as("SELECT name FROM workspace_secrets WHERE workspace_id = $1 AND ephemeral")
            .bind(workspace_id)
            .fetch_all(&state.db)
            .await
            .unwrap_or_default();
    if names.is_empty() {
        return;
    }

    let paths: Vec<(String,)> =
        sqlx::query_as("SELECT path FROM node_workspaces WHERE workspace_id = $1 AND node_id = $2")
            .bind(workspace_id)
            .bind(node_id)
            .fetch_all(&state.db)
            .await
            .unwrap_or_default();

    for (path,) in &paths {
        for (name,) in &names {
            // An empty write truncates the file to nothing; the node keeps its
            // 0600 handling and we avoid a delete op that could remove more.
            state.registry.send_to_node(
                node_id,
                nook_proto::ControlToNode::WriteWorkspaceFile {
                    checkout_path: path.clone(),
                    name: name.clone(),
                    content_b64: String::new(),
                },
            );
        }
    }
    tracing::info!(%session_id, secrets = names.len(), "wiped ephemeral secrets");
}

/// Push a single already-decrypted secret to every online checkout. Used
/// after an unlock, since sealed secrets can't ride the automatic sync.
pub async fn push_one(
    state: &AppState,
    tenant: TenantId,
    workspace: WorkspaceId,
    name: &str,
    content: &[u8],
) -> usize {
    let locations: Vec<(NodeId, String)> = sqlx::query_as(
        "SELECT node_id, path FROM node_workspaces WHERE tenant_id = $1 AND workspace_id = $2",
    )
    .bind(tenant)
    .bind(workspace)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let mut pushed = 0;
    for (node_id, path) in locations {
        if state.registry.send_to_node(
            node_id,
            ControlToNode::WriteWorkspaceFile {
                checkout_path: path,
                name: name.to_string(),
                content_b64: b64(content),
            },
        ) {
            pushed += 1;
        }
    }
    pushed
}
