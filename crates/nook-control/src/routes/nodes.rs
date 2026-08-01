use axum::extract::{Path, State};
use axum::Json;
use nook_types::*;

use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::services::session_queries;
use crate::state::AppState;

/// Which tenant owns a node.
///
/// Authorization for anything done TO a node is scoped to the node's tenant,
/// not the caller's — otherwise an operator could only ever act on machines in
/// their own tenant, which is the opposite of the job.
pub(crate) async fn node_tenant(state: &AppState, id: NodeId) -> ApiResult<nook_types::TenantId> {
    state.nodes.tenant_of(id).await?.ok_or(ApiError::NotFound)
}

/// Which owner a node listing is scoped to (MAIN-132): `Some(person)` restricts
/// it to a member's own machines, `None` is the whole-fleet view an owner/admin
/// gets — and the unchanged view a node token gets (AC-1). Fails closed: if a
/// user's person cannot be resolved, they are scoped to a person that owns
/// nothing rather than shown the fleet.
async fn visibility_scope(state: &AppState, auth: &AuthCtx) -> ApiResult<Option<uuid::Uuid>> {
    if !matches!(auth.principal, crate::auth::Principal::User) {
        return Ok(None); // node token: current behavior, unchanged
    }
    if auth.is_tenant_admin(state.identity.as_ref()).await? {
        return Ok(None);
    }
    Ok(Some(
        crate::auth::person_id_of(state, auth.user_id)
            .await
            .unwrap_or(uuid::Uuid::nil()),
    ))
}

/// The caller's own person, for the cross-org owner leg (MAIN-353) — `None`
/// for a node token, which is a machine and owns nothing.
///
/// Separate from [`visibility_scope`] on purpose. That answers "how much of
/// THIS tenant may they see", and an admin gets `None` there meaning *the whole
/// fleet*. Feeding that `None` into the owner leg would turn "my own machines"
/// into "everyone's", which is exactly the leak AC-3 names.
async fn own_person(state: &AppState, auth: &AuthCtx) -> Option<uuid::Uuid> {
    if !matches!(auth.principal, crate::auth::Principal::User) {
        return None;
    }
    crate::auth::person_id_of(state, auth.user_id).await.ok()
}

/// Label each node that is homed somewhere other than the tenant being acted
/// in, so the UI can say "your machine, over in Acme" rather than showing a
/// stranger's-looking row.
async fn label_foreign_homes(
    state: &AppState,
    acting: TenantId,
    nodes: &mut [Node],
) -> ApiResult<()> {
    let foreign: Vec<TenantId> = nodes
        .iter()
        .map(|n| n.tenant_id)
        .filter(|t| *t != acting)
        .collect();
    if foreign.is_empty() {
        return Ok(());
    }
    let names = state.nodes.tenant_names(&foreign).await?;
    for n in nodes.iter_mut().filter(|n| n.tenant_id != acting) {
        n.home_tenant = names.get(&n.tenant_id.0).cloned();
    }
    Ok(())
}

#[utoipa::path(get, path = "/api/v1/nodes",
    operation_id = "list_nodes", responses((status = 200, body = [Node])))]
pub async fn list(State(state): State<AppState>, auth: AuthCtx) -> ApiResult<Json<Vec<Node>>> {
    let scope = visibility_scope(&state, &auth).await?;
    let mine = own_person(&state, &auth).await;
    let mut nodes = state.nodes.list(auth.tenant_id, scope, mine).await?;
    label_foreign_homes(&state, auth.tenant_id, &mut nodes).await?;
    Ok(Json(nodes))
}

#[utoipa::path(get, path = "/api/v1/nodes/{id}",
    operation_id = "get_node",
    params(("id" = String, Path,)),
    responses((status = 200, body = Node), (status = 404)))]
pub async fn get_one(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<NodeId>,
) -> ApiResult<Json<Node>> {
    // Read unscoped, then apply the visibility rule here — the caller may own
    // this machine in another of their orgs (MAIN-353), which a tenant-scoped
    // read could not even find. Nothing is returned before the rule runs.
    let mut node = state
        .nodes
        .by_id_any_tenant(id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let mine = own_person(&state, &auth).await;
    let is_mine = mine.is_some() && node.owner_person_id == mine;

    if node.tenant_id != auth.tenant_id {
        // Foreign-home: reachable ONLY by its owner. No admin, operator or
        // teammate of either tenant gets here, and the refusal is a 404 so the
        // node's existence is not disclosed (AC-8).
        if !is_mine {
            return Err(ApiError::NotFound);
        }
    } else if let Some(person) = visibility_scope(&state, &auth).await? {
        // A member sees a node they own OR one the team has been given
        // (`shared`, MAIN-135); anything else is a 404, not a 403, so it is
        // indistinguishable from a node that does not exist (MAIN-132's
        // no-existence-oracle rule).
        if node.owner_person_id != Some(person) && !node.shared {
            return Err(ApiError::NotFound);
        }
    }
    label_foreign_homes(&state, auth.tenant_id, std::slice::from_mut(&mut node)).await?;
    Ok(Json(node))
}

/// POST /api/v1/nodes/{id}/shared — the owner designates their machine as
/// team-usable, or takes it back (MAIN-135).
///
/// Ownership, not role: only the person who owns the node may toggle it — an
/// admin who is not the owner is refused (403), because nobody volunteers
/// someone else's hardware. A node the caller cannot even see is a 404 (no
/// existence oracle, matching `get_one`); a visible node they do not own is a
/// 403; a node with no owner cannot be shared at all (nothing to consent).
///
/// This parallels the `require_person_owns_node` chokepoint (person == owner),
/// but adds the visibility 404-vs-403 distinction the session guard does not
/// need, and carries share-specific messages.
#[utoipa::path(post, path = "/api/v1/nodes/{id}/shared",
    operation_id = "set_node_shared",
    params(("id" = String, Path,)),
    request_body = SetSharedRequest,
    responses((status = 200, body = Node), (status = 403), (status = 404)))]
pub async fn set_shared(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<NodeId>,
    Json(req): Json<SetSharedRequest>,
) -> ApiResult<Json<Node>> {
    // A person designates a machine; a node credential is not a person.
    auth.require_user()?;

    let Some(sharing) = state.nodes.sharing(id, auth.tenant_id).await? else {
        return Err(ApiError::NotFound);
    };
    let (owner_person, is_shared) = (sharing.owner_person_id, sharing.shared);

    let me = crate::auth::person_id_of(&state, auth.user_id).await?;

    // Visibility first: a member who can't see the node (not theirs, not shared)
    // gets the same 404 an unknown id gets — the toggle must not confirm it
    // exists. An admin (unscoped) skips this and is caught by the owner check.
    if let Some(scope_person) = visibility_scope(&state, &auth).await? {
        let visible = owner_person == Some(scope_person) || is_shared;
        if !visible {
            return Err(ApiError::NotFound);
        }
    }

    // Only the OWNER may toggle. An ownerless node has nobody to consent (AC-3).
    match owner_person {
        None => {
            return Err(ApiError::ForbiddenMsg(
                "this machine has no owner, so no one can share it".into(),
            ))
        }
        Some(o) if o != me => {
            return Err(ApiError::ForbiddenMsg(
                "only the machine's owner can share it".into(),
            ))
        }
        Some(_) => {}
    }

    let node = state
        .nodes
        .set_shared(id, auth.tenant_id, req.shared)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(node))
}

/// The labels a node has by virtue of what it IS, not what an operator typed
/// (MAIN-314 AC-1).
///
/// Derived at read time rather than stored, so they cannot drift from the
/// platform the node actually reports. `os` comes from `nodes.platform`, which
/// is `std::env::consts::OS` as the agent saw it — already exactly
/// `linux`/`macos`/`windows`; `arch` comes from the capabilities the agent
/// reports, and is absent if it has not reported any yet.
fn derived_labels(node: &Node) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    if !node.platform.is_empty() {
        out.insert("os".to_string(), node.platform.clone());
    }
    if let Some(arch) = node
        .capabilities
        .get("architecture")
        .and_then(|v| v.as_str())
    {
        out.insert("arch".to_string(), arch.to_string());
    }
    out
}

/// The stored custom labels, ignoring anything that is not a string value —
/// the column is jsonb and an operator could have written a number through the
/// API before this route existed.
fn custom_labels(node: &Node) -> std::collections::BTreeMap<String, String> {
    node.labels
        .as_object()
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// `pub(crate)` because the reconciler reads it (MAIN-316) — this is the
/// "inputs placement will read" MAIN-314 was written to supply.
pub(crate) fn placement_of(node: &Node) -> NodePlacement {
    let custom = custom_labels(node);
    // Derived wins: a node cannot be relabelled into another operating system.
    let mut labels = custom.clone();
    labels.extend(derived_labels(node));
    NodePlacement {
        labels,
        custom_labels: custom,
        taints: serde_json::from_value(node.taints.clone()).unwrap_or_default(),
    }
}

/// `GET /api/v1/nodes/{id}/placement` — the labels and taints placement reads
/// (MAIN-314 AC-3). Visible to anyone who can see the node.
#[utoipa::path(get, path = "/api/v1/nodes/{id}/placement",
    operation_id = "get_node_placement",
    params(("id" = String, Path,)),
    responses((status = 200, body = NodePlacement), (status = 404)))]
pub async fn get_placement(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<NodeId>,
) -> ApiResult<Json<NodePlacement>> {
    let node = visible_node(&state, &auth, id).await?;
    Ok(Json(placement_of(&node)))
}

/// `PUT /api/v1/nodes/{id}/placement` — set them (MAIN-314 AC-3).
///
/// Owner-gated exactly as sharing is: an operator-set label steers where work
/// lands, so it is the machine owner's call and not any teammate's. An
/// ownerless node has nobody to ask.
#[utoipa::path(put, path = "/api/v1/nodes/{id}/placement",
    operation_id = "set_node_placement",
    params(("id" = String, Path,)),
    request_body = SetNodePlacementRequest,
    responses((status = 200, body = NodePlacement), (status = 403), (status = 404)))]
pub async fn set_placement(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<NodeId>,
    Json(req): Json<SetNodePlacementRequest>,
) -> ApiResult<Json<NodePlacement>> {
    // A person labels a machine; a node credential is not a person.
    auth.require_user()?;
    let node = visible_node(&state, &auth, id).await?;

    let me = crate::auth::person_id_of(&state, auth.user_id).await?;
    match node.owner_person_id {
        None => {
            return Err(ApiError::ForbiddenMsg(
                "this machine has no owner, so no one can set its placement".into(),
            ))
        }
        Some(o) if o != me => {
            return Err(ApiError::ForbiddenMsg(
                "only the machine's owner can set its labels and taints".into(),
            ))
        }
        Some(_) => {}
    }

    for t in &req.taints {
        if t.key.trim().is_empty() {
            return Err(ApiError::BadRequest("a taint needs a key".into()));
        }
        if t.effect != "NoSchedule" {
            return Err(ApiError::BadRequest(format!(
                "{:?} is not a taint effect — expected NoSchedule",
                t.effect
            )));
        }
    }
    // Derived labels are computed, never stored: accepting one would let it
    // drift from the platform the node reports.
    for k in req.labels.keys() {
        if k == "os" || k == "arch" {
            return Err(ApiError::BadRequest(format!(
                "{k:?} is derived from what the node reports and cannot be set"
            )));
        }
        if k.trim().is_empty() {
            return Err(ApiError::BadRequest("a label needs a key".into()));
        }
    }

    let node = state
        .nodes
        .set_placement(
            id,
            auth.tenant_id,
            serde_json::to_value(&req.labels).unwrap_or_else(|_| serde_json::json!({})),
            serde_json::to_value(&req.taints).unwrap_or_else(|_| serde_json::json!([])),
        )
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(placement_of(&node)))
}

/// The node, if this caller may see it at all — the same visibility rule the
/// sharing toggle applies, so an invisible node 404s rather than confirming it
/// exists.
async fn visible_node(state: &AppState, auth: &AuthCtx, id: NodeId) -> ApiResult<Node> {
    let node = state
        .nodes
        .get(auth.tenant_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if let Some(scope_person) = visibility_scope(state, auth).await? {
        if node.owner_person_id != Some(scope_person) && !node.shared {
            return Err(ApiError::NotFound);
        }
    }
    Ok(node)
}

/// `POST /api/v1/nodes/{id}/authorize` — launch a runtime's login flow in a
/// session on the node so it can be device-authorized from the UI (MAIN-126).
///
/// Node-scoped authorization: a PERSONAL node may be authorized only by its
/// owner; a SHARED / operator node only by a node-manager (an org admin) —
/// authorizing a shared machine makes that runtime credential available to
/// every workload already permitted to run there, so it is not one person's
/// call. The node, not the caller, chooses the login command to run.
#[utoipa::path(post, path = "/api/v1/nodes/{id}/authorize",
    operation_id = "authorize_runtime",
    params(("id" = String, Path,)),
    request_body = AuthorizeRuntimeRequest,
    responses((status = 200, body = Session), (status = 403), (status = 404)))]
pub async fn authorize(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<NodeId>,
    Json(req): Json<AuthorizeRuntimeRequest>,
) -> ApiResult<Json<Session>> {
    use crate::auth::perm::{Permission, Scope};

    // Unknown/invisible node is a 404, not a 403 — no existence oracle.
    let Some((sharing, caps)) = state
        .nodes
        .sharing_and_capabilities(id, auth.tenant_id)
        .await?
    else {
        return Err(ApiError::NotFound);
    };
    let shared = sharing.shared;
    let is_operator = caps
        .get("shared_operator")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if shared || is_operator {
        // Shared substrate → an admin, not one person (`node.manage`).
        auth.require(
            &state,
            Permission::NodeManage,
            Scope::Tenant(auth.tenant_id),
        )
        .await?;
    } else {
        // A personal node → only its owner.
        crate::auth::require_person_owns_node(&state, auth.tenant_id, Some(auth.user_id), id)
            .await?;
    }

    let session = session_queries::create_auth_session(
        &state,
        auth.tenant_id,
        Some(auth.user_id),
        id,
        &req.runtime,
    )
    .await?;
    Ok(Json(session))
}

#[utoipa::path(delete, path = "/api/v1/nodes/{id}",
    operation_id = "delete_node",
    params(("id" = String, Path,)),
    responses((status = 204), (status = 404)))]
pub async fn delete(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<NodeId>,
) -> ApiResult<axum::http::StatusCode> {
    // `node.manage` on the node's OWN tenant, not merely "is a person".
    // `require_user` let any signed-in account delete a node in its tenant,
    // which was fine when a tenant was one person and is not a permission
    // model. A tenant admin holds this for their own tenant; an operator holds
    // it everywhere, which is how they can evict a machine they do not own.
    let tenant = node_tenant(&state, id).await?;
    auth.require(
        &state,
        crate::auth::perm::Permission::NodeManage,
        crate::auth::perm::Scope::Tenant(tenant),
    )
    .await?;
    let res = state.nodes.delete(tenant, id).await?;
    if res == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// Ask a node to rescan its workspace roots now, instead of waiting for the
/// periodic sweep. Backs `nook import`.
#[utoipa::path(post, path = "/api/v1/nodes/{id}/rescan",
    operation_id = "rescan_node",
    params(("id" = String, Path,)),
    responses((status = 202), (status = 404)))]
pub async fn rescan(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<NodeId>,
) -> ApiResult<axum::http::StatusCode> {
    // A node rescans itself; asking another to is an instruction, not a report.
    auth.require_node_self(id)?;
    if !state.nodes.exists_in_tenant(id, auth.tenant_id).await? {
        return Err(ApiError::NotFound);
    }
    if !state
        .registry
        .send_to_node(id, nook_proto::ControlToNode::RescanWorkspaces)
    {
        return Err(ApiError::BadRequest("node is offline".into()));
    }
    Ok(axum::http::StatusCode::ACCEPTED)
}

/// Coordinated path rewrite after `nook migrate-workspaces` moves a node's
/// checkouts into the per-control-plane slugged root (MAIN-107 AC-4).
///
/// The node has already moved the directories on disk and now needs the two
/// durable path records — `node_workspaces.path` and `tasks.worktree_path` —
/// pointed at the new locations. Doing that through ordinary discovery would be
/// destructive: the reconcile deletes rows whose path is no longer reported and
/// re-inserts the new paths as brand-new checkouts, which announces a fresh
/// `.env` delivery and drops the `worktree_path` a running task depends on. So
/// this rewrites the paths in place, in ONE transaction, preserving row
/// identity: nothing looks new, nothing is re-delivered, nothing goes stale.
///
/// The pairs are `{old, new}` on-disk paths. Every UPDATE is pinned to this
/// node (`node_id`/`worktree_node_id = $id`), so a caller cannot rewrite
/// another node's rows even if it names their paths — and any `old` path that
/// does not currently belong to this node is refused outright rather than
/// silently doing nothing.
#[utoipa::path(post, path = "/api/v1/nodes/{id}/migrate-paths",
    operation_id = "migrate_node_paths",
    params(("id" = String, Path,)),
    request_body = MigratePathsRequest,
    responses(
        (status = 200, body = MigratePathsResponse),
        (status = 400, description = "a path does not belong to this node"),
        (status = 404)))]
pub async fn migrate_paths(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<NodeId>,
    Json(req): Json<MigratePathsRequest>,
) -> ApiResult<Json<MigratePathsResponse>> {
    // A node migrates its OWN paths; a user driving the fleet may do it too, but
    // only for a node in their tenant — exactly the `rescan` authorization. A
    // node token that named a peer would be rewriting another machine's rows.
    auth.require_node_self(id)?;
    if !state.nodes.exists_in_tenant(id, auth.tenant_id).await? {
        return Err(ApiError::NotFound);
    }

    // The rewrite itself — belonging check + one transaction — lives beside the
    // reconcile it deliberately bypasses, so the two path strategies are read
    // together (crate::services::discovery).
    let result =
        crate::services::discovery::migrate_paths(&*state.workspaces, id, &req.pairs).await?;

    crate::events::record(
        &state,
        auth.tenant_id,
        crate::events::EventDraft::new("node.paths_migrated")
            .actor(
                match auth.principal {
                    crate::auth::Principal::Node(_) => "node",
                    crate::auth::Principal::User => "user",
                },
                auth.user_id.0,
            )
            .node(id)
            .payload(serde_json::json!({
                "pairs": req.pairs.len(),
                "node_workspaces_updated": result.node_workspaces_updated,
                "tasks_updated": result.tasks_updated,
            })),
    )
    .await;

    Ok(Json(result))
}

/// POST /api/v1/nodes/{id}/update — tell a node to replace its agent and restart.
///
/// A person asking, rather than the automatic path: a node already updates
/// itself on reconnect when its version differs from what this control plane
/// expects. This is for the case where you do not want to wait for a
/// reconnect, or where the automatic path declined and you want to see why.
///
/// The node decides whether it can. It knows whether anything would restart it
/// and refuses if not, because an agent that replaces its binary and exits
/// unsupervised simply goes offline — and doing that across a fleet takes every
/// machine at once.
#[utoipa::path(post, path = "/api/v1/nodes/{id}/update",
    operation_id = "update_node_agent",
    params(("id" = String, Path,)),
    responses((status = 202, description = "asked"), (status = 400, description = "node is offline")))]
pub async fn update(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<NodeId>,
) -> ApiResult<axum::http::StatusCode> {
    // Replacing a machine's binary is an act on the fleet, not a self-report:
    // `node.manage` on the node's own tenant, so a node token cannot trigger it
    // on a peer and an operator can trigger it anywhere.
    let tenant = node_tenant(&state, id).await?;
    auth.require(
        &state,
        crate::auth::perm::Permission::NodeManage,
        crate::auth::perm::Scope::Tenant(tenant),
    )
    .await?;
    if !state.nodes.exists_in_tenant(id, tenant).await? {
        return Err(ApiError::NotFound);
    }
    if !state
        .registry
        .send_to_node(id, nook_proto::ControlToNode::UpdateAgent)
    {
        return Err(ApiError::BadRequest(
            "that node is offline — it will update itself when it reconnects".into(),
        ));
    }
    crate::events::record(
        &state,
        auth.tenant_id,
        crate::events::EventDraft::new("node.update_requested")
            .actor("user", auth.user_id.0)
            .node(id),
    )
    .await;
    Ok(axum::http::StatusCode::ACCEPTED)
}
