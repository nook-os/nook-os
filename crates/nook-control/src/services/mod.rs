//! Shared service layer: REST handlers and MCP tools both call into here so
//! the two surfaces can never drift apart.

/// The private key a workspace's git operations should use, decrypted for
/// transient delivery to a node (MAIN-367).
///
/// `None` when the workspace pins no credential, which is the ordinary case and
/// must stay ordinary: the node then falls back to its own generated key, which
/// is what public repos and local paths have always used. `None` is also the
/// answer when the credential has gone or cannot be decrypted — a clone that
/// then fails on authentication is a better outcome than one that fails to be
/// attempted, and the node reports the git error either way.
///
/// The material returned here is the ONLY form in which a private key leaves
/// this process, and it exists to be written to a node's disk for the life of a
/// single git command. It must never be logged, recorded on an event, or
/// returned from an API that a browser can reach.
pub async fn workspace_git_key(
    state: &crate::state::AppState,
    tenant: nook_types::TenantId,
    workspace: nook_types::WorkspaceId,
) -> Option<String> {
    let ws = state.workspaces.get(tenant, workspace).await.ok()??;
    let credential = ws.git_credential_id?;
    let sealed = state
        .git_credentials
        .sealed_secret(credential, tenant)
        .await
        .ok()??;
    state.vault.decrypt_string(&sealed).ok()
}

/// The slug naming a tenant's checkout tree on a node.
///
/// Every `CloneRepo` carries this, because the node cannot derive it: it knows
/// only its own home tenant, and cross-tenant placement (MAIN-353) means the
/// tenant that asked for a clone is routinely a different one. Sending it makes
/// the requesting tenant the thing the path is scoped by, which is what stops
/// two tenants' copies of the same repo landing in one directory (MAIN-363).
///
/// `None` on a lookup failure rather than an error: a clone that lands in the
/// node's default tree is a worse path, not a failed operation.
pub async fn tenant_slug(
    state: &crate::state::AppState,
    tenant: nook_types::TenantId,
) -> Option<String> {
    state
        .operator
        .tenant_org_and_slug(tenant)
        .await
        .ok()
        .flatten()
        .map(|(_, slug)| slug)
}

pub mod activity_queries;
pub mod claim_reaper;
pub mod discovery;
pub mod forge;
pub mod identity;
pub mod interactions;
pub mod job_dispatch;
pub mod job_reaper;
pub mod jobs;
pub mod kanban;
pub mod local_auth;
pub mod loops;
pub mod notebook_queries;
pub mod notify;
pub mod operator_queries;
pub mod overview_queries;
pub mod policy;
pub mod port_leases;
pub mod queue;
pub mod repo_settings;
pub mod review_sweep;
pub mod run_reconcile;
pub mod runtime_auth;
pub mod runtime_auth_flow;
pub mod schedule;
pub mod secrets;
pub mod session_queries;
pub mod session_reaper;
pub mod session_reconcile;
pub mod tasks;
pub mod taskwork;
pub mod triggers;
pub mod work_source;
pub mod workspace_queries;
pub mod workspace_reaper;
