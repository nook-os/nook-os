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
    if !state
        .workspace_secrets
        .any(tenant, workspace)
        .await
        .unwrap_or(false)
    {
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
    // An ad-hoc terminal has no workspace, so it has no secrets to wipe — the
    // same outcome the old typed read gave by failing to decode a NULL.
    let Ok(Some(session)) = state.sessions.get(tenant, session_id).await else {
        return;
    };
    let (Some(workspace_id), node_id) = (session.workspace_id, session.node_id) else {
        return;
    };

    // Another live session still needs the files.
    if state
        .sessions
        .live_siblings(workspace_id, session_id)
        .await
        .unwrap_or(0)
        > 0
    {
        return;
    }

    let names = state
        .workspace_secrets
        .ephemeral_names(workspace_id)
        .await
        .unwrap_or_default();
    if names.is_empty() {
        return;
    }

    // Present checkouts of this workspace ON THIS NODE — the tenant scoping the
    // repo method adds is implied by the workspace, so the set is the same one
    // the inline query returned.
    let paths: Vec<String> = state
        .workspaces
        .present_checkouts(tenant, workspace_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|c| c.node_id == node_id)
        .map(|c| c.path)
        .collect();

    for path in &paths {
        for name in &names {
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
    let locations = state
        .workspaces
        .present_checkouts(tenant, workspace)
        .await
        .unwrap_or_default();

    let mut pushed = 0;
    for c in locations {
        if state.registry.send_to_node(
            c.node_id,
            ControlToNode::WriteWorkspaceFile {
                checkout_path: c.path,
                name: name.to_string(),
                content_b64: b64(content),
            },
        ) {
            pushed += 1;
        }
    }
    pushed
}
