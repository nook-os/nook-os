//! Node reads (MAIN-245). Split verbatim out of `services/core.rs`; see that
//! file's header for why.

use nook_db::{params, Db, DbPool, Postgres, TypeMapping};
use nook_types::*;

use crate::error::ApiResult;

/// List a tenant's nodes, optionally scoped to a single owner person (MAIN-132).
/// `owner = Some(person)` returns that person's own nodes PLUS any node the
/// team has been given — those flagged `shared` (MAIN-135); `owner = None`
/// returns the whole fleet (owner/admin, and node tokens whose view is
/// unchanged). Shared grants visibility only — session-start stays owner-only.
pub async fn list_nodes(
    db: &DbPool,
    tenant: TenantId,
    owner: Option<uuid::Uuid>,
) -> ApiResult<Vec<Node>> {
    Ok(db
        .query_all(
            &format!(
                "SELECT id, tenant_id, name, hostname, platform, capabilities, resources, status,
                last_seen_at, owner_person_id, shared, created_at, updated_at
         FROM nodes
         WHERE tenant_id = $1 AND ({owner} IS NULL OR owner_person_id = $2 OR shared)
         ORDER BY name",
                owner = Postgres.cast("$2", "uuid")
            ),
            params![tenant, owner],
        )
        .await?)
}

/// Every node's id and name in a tenant. Moved verbatim out of `mcp_backend`
/// (MAIN-245); the online filtering that used it stays there, because liveness
/// comes from the registry rather than the database.
pub async fn list_ids_and_names(db: &DbPool, tenant: TenantId) -> ApiResult<Vec<(NodeId, String)>> {
    Ok(db
        .query_all(
            "SELECT id, name FROM nodes WHERE tenant_id = $1",
            params![tenant],
        )
        .await?)
}
