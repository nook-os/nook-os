//! Shared service layer: REST handlers and MCP tools both call into here so
//! the two surfaces can never drift apart.

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
pub mod runtime_auth;
pub mod runtime_auth_flow;
pub mod schedule;
pub mod secrets;
pub mod session_queries;
pub mod session_reconcile;
pub mod tasks;
pub mod taskwork;
pub mod triggers;
pub mod workspace_queries;
pub mod workspace_reaper;
