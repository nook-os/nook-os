//! `/api/v1/operator/*` — read-only, and structurally free of session content.
//!
//! # One prefix, so a dangerous diff is obvious
//!
//! Everything an operator can see lives under this module. There are no session
//! routes here and there must never be: a pull request that adds one to
//! `operator.rs` is visible at a glance in a way that the same route added to
//! `sessions.rs` would not be. That legibility is the point of the prefix.
//!
//! # The projection is a minimum, and policy ADDS to it
//!
//! Every query below names its columns explicitly. There is no `SELECT *` and
//! no shared query with the tenant-facing routes, because a shared query grows
//! a column one day and leaks it here the same afternoon.
//!
//! Policy widens by adding columns to a response, never by filtering fields out
//! of one. A filter that is missed fails OPEN — it returns the thing it was
//! supposed to remove — and on this surface failing open means an operator
//! reading somebody's branch names. Additive fails closed: forget to add, and
//! the field is simply absent.
//!
//! Writes (CA rotation, node revocation) are deliberately absent until the read
//! surface is proven.

use axum::extract::{Path, Query, State};
use axum::Json;
use nook_types::*;
use uuid::Uuid;

use crate::auth::perm::{Permission, Scope};
use crate::auth::AuthCtx;
use crate::error::ApiResult;
use crate::services::operator_queries;
use crate::services::policy::{self, Field};
use crate::state::AppState;

/// Record that somebody looked.
///
/// "Who looked at whose activity" is a question a shared control plane WILL be
/// asked, and the honest answer requires having written it down at the time.
/// Operator reads are audited for the same reason operator writes would be.
async fn audit(state: &AppState, auth: &AuthCtx, what: &str, subject: Option<TenantId>) {
    crate::events::record(
        state,
        auth.tenant_id,
        crate::events::EventDraft::new("operator.read")
            .actor("user", auth.user_id.0)
            .payload(serde_json::json!({
                "surface": what,
                "tenant_read": subject.map(|t| t.0),
            })),
    )
    .await;
}

#[utoipa::path(get, path = "/api/v1/operator/orgs",
    operation_id = "operator_list_orgs",
    responses((status = 200, body = [OperatorOrg]), (status = 403)))]
pub async fn orgs(
    State(state): State<AppState>,
    auth: AuthCtx,
) -> ApiResult<Json<Vec<OperatorOrg>>> {
    auth.require(&state, Permission::OrgView, Scope::Deployment)
        .await?;
    let rows = state.operator.orgs().await?;
    audit(&state, &auth, "orgs", None).await;
    Ok(Json(rows))
}

/// Tenants, at minimum visibility.
///
/// Always visible, per the model: that a tenant exists, its member count, and
/// how many nodes and sessions it runs. Several machines working one task is an
/// audit signal, and an operator who cannot see load cannot run the deployment.
///
/// Never visible here: repository names, branches, worktree paths, task titles.
/// Those are policy-gated and added by `enrich` below — they are not selected
/// and then removed.
#[utoipa::path(get, path = "/api/v1/operator/tenants",
    operation_id = "operator_list_tenants",
    params(PageQuery),
    responses((status = 200, body = Page<OperatorTenant>), (status = 403)))]
pub async fn tenants(
    State(state): State<AppState>,
    auth: AuthCtx,
    Query(q): Query<PageQuery>,
) -> ApiResult<Json<Page<OperatorTenant>>> {
    auth.require(&state, Permission::TenantView, Scope::Deployment)
        .await?;

    let mut page = operator_queries::operator_tenants_page(&*state.operator, &q).await?;

    // Policy ADDS. Absent unless an org has opted in, and absent by default.
    for row in &mut page.rows {
        enrich(&state, row).await?;
    }

    audit(&state, &auth, "tenants", None).await;
    Ok(Json(page))
}

/// How many policy-gated values one tenant row carries. A cap, not a page:
/// the console shows a sample, and an operator who needs the whole list has
/// the tenant's own surfaces for it.
const ENRICH_LIMIT: i64 = 50;

/// Add policy-gated fields, one opt-in at a time.
///
/// Each field is fetched only if its org has enabled it. The default path
/// touches no extra tables at all, which is what makes "off" the cheap case as
/// well as the safe one.
async fn enrich(state: &AppState, row: &mut OperatorTenant) -> ApiResult<()> {
    let Some(org) = row.org_id else { return Ok(()) };

    if policy::enabled(&*state.org_policy, org, Field::RepositoryNames).await? {
        row.repositories = Some(
            state
                .workspaces
                .names_in_tenant(row.id, ENRICH_LIMIT)
                .await?,
        );
    }
    if policy::enabled(&*state.org_policy, org, Field::TaskTitles).await? {
        // The `private` exclusion lives inside `operator_visible_titles`, not
        // here: the policy is ADDITIVE (it adds titles, it does not filter), so
        // a private card must simply never be selected. A filter applied at this
        // end would fail open the first time somebody forgot it (MAIN-76 AC-4).
        row.task_titles = Some(
            state
                .tasks
                .operator_visible_titles(row.id, ENRICH_LIMIT)
                .await?,
        );
    }
    Ok(())
}

/// Nodes, always visible. Names, status, resources, owner, session count.
#[utoipa::path(get, path = "/api/v1/operator/nodes",
    operation_id = "operator_list_nodes",
    params(PageQuery),
    responses((status = 200, body = Page<OperatorNode>), (status = 403)))]
pub async fn nodes(
    State(state): State<AppState>,
    auth: AuthCtx,
    Query(q): Query<PageQuery>,
) -> ApiResult<Json<Page<OperatorNode>>> {
    auth.require(&state, Permission::NodeView, Scope::Deployment)
        .await?;
    let page = operator_queries::operator_nodes_page(&*state.operator, &q).await?;
    audit(&state, &auth, "nodes", None).await;
    Ok(Json(page))
}

/// The audit trail, including operator reads themselves — paged and searchable.
#[utoipa::path(get, path = "/api/v1/operator/audit",
    operation_id = "operator_audit",
    params(PageQuery),
    responses((status = 200, body = Page<OperatorAuditEntry>), (status = 403)))]
pub async fn audit_log(
    State(state): State<AppState>,
    auth: AuthCtx,
    Query(q): Query<PageQuery>,
) -> ApiResult<Json<Page<OperatorAuditEntry>>> {
    auth.require(&state, Permission::AuditView, Scope::Deployment)
        .await?;
    // Kinds, actors and times — never payloads. The projection and the
    // prefix filter live in `operator_queries::operator_audit_page`, shared with its tests.
    let page = operator_queries::operator_audit_page(&*state.operator, &q).await?;
    audit(&state, &auth, "audit", None).await;
    Ok(Json(page))
}

/// The current policy for one org, for the operator who may change it.
#[utoipa::path(get, path = "/api/v1/operator/orgs/{id}/policy",
    operation_id = "operator_get_policy", params(("id" = String, Path,)),
    responses((status = 200, body = [PolicyField]), (status = 403)))]
pub async fn get_policy(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Vec<PolicyField>>> {
    auth.require(&state, Permission::PolicyView, Scope::Org(id))
        .await?;
    Ok(Json(policy::current(&*state.org_policy, id).await?))
}

/// Widen or narrow one field. Recorded, and announced to the people it affects.
#[utoipa::path(post, path = "/api/v1/operator/orgs/{id}/policy",
    operation_id = "operator_set_policy", params(("id" = String, Path,)),
    request_body = SetPolicyRequest,
    responses((status = 200, body = [PolicyField]), (status = 403)))]
pub async fn set_policy(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<Uuid>,
    Json(req): Json<SetPolicyRequest>,
) -> ApiResult<Json<Vec<PolicyField>>> {
    auth.require(&state, Permission::PolicyManage, Scope::Org(id))
        .await?;
    policy::set(&state, id, &req.field, req.enabled, auth.user_id.0).await?;
    Ok(Json(policy::current(&*state.org_policy, id).await?))
}

/// Grant or revoke a role binding.
///
/// The one write on this surface, and it is here rather than deferred with the
/// others because a deployment with exactly one operator and no way to make a
/// second is a deployment one lost password away from being unadministrable.
///
/// Requires `org.manage` — an operator can appoint another operator, which is
/// the same authority every root-shaped role has. A tenant admin cannot,
/// because `org.manage` is not in their role.
#[utoipa::path(post, path = "/api/v1/operator/bindings",
    operation_id = "operator_grant", request_body = GrantRequest,
    responses((status = 200), (status = 403)))]
pub async fn grant(
    State(state): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<GrantRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    // `rbac.grant`, not `org.manage` — see migration 0018. An operator holds
    // this; a tenant admin never does.
    //
    // Required at DEPLOYMENT scope even when granting over one tenant: handing
    // out a role is the power to hand out that power, so it stays with whoever
    // runs the deployment rather than becoming something each tenant's admin
    // can do for their own.
    auth.require(&state, Permission::RbacGrant, Scope::Deployment)
        .await?;

    let user_id = state
        .identity
        .user_id_by_email(&req.email)
        .await?
        .ok_or(crate::error::ApiError::NotFound)?;

    match (req.revoke, req.tenant_id) {
        (true, None) => {
            state
                .operator
                .revoke_deployment_role(user_id.0, &req.role)
                .await?
        }
        (false, None) => {
            state
                .operator
                .grant_deployment_role(user_id.0, &req.role, auth.user_id.0)
                .await?
        }
        (true, Some(t)) => {
            state
                .operator
                .revoke_tenant_role(user_id.0, &req.role, t)
                .await?
        }
        (false, Some(t)) => {
            state
                .operator
                .grant_tenant_role(user_id.0, &req.role, t, auth.user_id.0)
                .await?
        }
    }

    // Who gained power over this deployment, granted by whom, is the single
    // most audit-worthy thing that happens here.
    crate::events::record(
        &state,
        auth.tenant_id,
        crate::events::EventDraft::new(if req.revoke {
            "rbac.revoked"
        } else {
            "rbac.granted"
        })
        .actor("user", auth.user_id.0)
        .payload(serde_json::json!({
            "subject": req.email,
            "role": req.role,
            "scope": "deployment",
        })),
    )
    .await;

    Ok(Json(serde_json::json!({ "ok": true })))
}

// ── writes ──────────────────────────────────────────────────────────────────
//
// Every one names its target and authorizes against THAT target's scope, not
// the caller's. A tenant admin passes for their own tenant because their
// binding sits there; an operator passes anywhere because theirs sits at
// `deployment` and covers every descendant. One predicate, no branching.
//
// Nothing here reads session content, and nothing here can destroy a tenant's
// work: `operator` does not hold `tenant.manage`, so there is no route to
// delete a tenant, a workspace or a task. Revoking a node stops a machine; it
// does not reach what is on it.

/// Record a write. Separate kind from `operator.read` so "what did they change"
/// is one filter rather than a payload inspection.
async fn audit_write(state: &AppState, auth: &AuthCtx, action: &str, target: serde_json::Value) {
    crate::events::record(
        state,
        auth.tenant_id,
        crate::events::EventDraft::new("operator.write")
            .actor("user", auth.user_id.0)
            .payload(serde_json::json!({ "action": action, "target": target })),
    )
    .await;
}

#[utoipa::path(post, path = "/api/v1/operator/orgs",
    operation_id = "operator_create_org", request_body = CreateOrgRequest,
    responses((status = 200, body = OperatorOrg), (status = 403)))]
pub async fn create_org(
    State(state): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<CreateOrgRequest>,
) -> ApiResult<Json<OperatorOrg>> {
    auth.require(&state, Permission::OrgManage, Scope::Deployment)
        .await?;
    let name = req.name.trim();
    if name.is_empty() {
        return Err(crate::error::ApiError::BadRequest(
            "an org needs a name".into(),
        ));
    }
    let slug = req
        .slug
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .unwrap_or_else(|| slugify(name));

    let row = state.operator.create_org(name, &slug).await?;

    audit_write(
        &state,
        &auth,
        "org.create",
        serde_json::json!({ "slug": slug }),
    )
    .await;
    Ok(Json(row))
}

#[utoipa::path(patch, path = "/api/v1/operator/orgs/{id}",
    operation_id = "operator_rename_org", params(("id" = String, Path,)),
    request_body = RenameOrgRequest,
    responses((status = 200, body = OperatorOrg), (status = 403), (status = 404)))]
pub async fn rename_org(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<Uuid>,
    Json(req): Json<RenameOrgRequest>,
) -> ApiResult<Json<OperatorOrg>> {
    // Scoped to the org itself, so an org admin can rename their own without
    // holding anything at the deployment.
    auth.require(&state, Permission::OrgManage, Scope::Org(id))
        .await?;
    let row = state
        .operator
        .rename_org(id, req.name.trim())
        .await?
        .ok_or(crate::error::ApiError::NotFound)?;
    audit_write(
        &state,
        &auth,
        "org.rename",
        serde_json::json!({ "org": row.slug }),
    )
    .await;
    Ok(Json(row))
}

/// Move a tenant into another org.
///
/// Requires `org.manage` at BOTH ends — the org losing it and the org gaining
/// it. Checking only one would let somebody with authority over a single org
/// pull tenants into it from orgs they have no say over, or push their own
/// tenants somewhere they cannot be followed.
#[utoipa::path(post, path = "/api/v1/operator/tenants/{id}/org",
    operation_id = "operator_move_tenant", params(("id" = String, Path,)),
    request_body = MoveTenantRequest,
    responses((status = 200), (status = 403), (status = 404)))]
pub async fn move_tenant(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<TenantId>,
    Json(req): Json<MoveTenantRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let (from, slug) = state
        .operator
        .tenant_org_and_slug(id)
        .await?
        .ok_or(crate::error::ApiError::NotFound)?;

    if let Some(from) = from {
        auth.require(&state, Permission::OrgManage, Scope::Org(from))
            .await?;
    }
    auth.require(&state, Permission::OrgManage, Scope::Org(req.org_id))
        .await?;

    state.operator.move_tenant_to_org(id, req.org_id).await?;

    audit_write(
        &state,
        &auth,
        "tenant.move_org",
        serde_json::json!({ "tenant": slug, "from": from, "to": req.org_id }),
    )
    .await;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// Stage a new CA for a tenant.
///
/// Two steps, not one. Staging distributes the new authority so machines pick
/// it up on their next renewal; promoting makes it sign. A single "rotate"
/// button that did both would strand every node that had not renewed in
/// between — which is the reason the tenant-facing route has always been two
/// calls, and not a limitation worth papering over here.
///
/// Delegates to the same mechanism rather than reimplementing it: there must be
/// exactly one way a CA is created, or the two drift and one is wrong.
#[utoipa::path(post, path = "/api/v1/operator/tenants/{id}/ca",
    operation_id = "operator_stage_ca", params(("id" = String, Path,)),
    responses((status = 200, body = TenantCaSummary), (status = 403), (status = 404)))]
pub async fn stage_ca(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<TenantId>,
) -> ApiResult<Json<TenantCaSummary>> {
    crate::routes::tenant_ca::gate_tenant(&state, &auth, id, "operator_stage").await?;
    let summary = crate::routes::tenant_ca::stage_for(&state, &auth, id).await?;
    audit_write(
        &state,
        &auth,
        "ca.stage",
        serde_json::json!({ "tenant": id.0, "fingerprint": summary.fingerprint }),
    )
    .await;
    Ok(Json(summary))
}

/// Promote a staged CA to signer. The previous signer keeps being trusted.
#[utoipa::path(post, path = "/api/v1/operator/tenants/{id}/ca/{ca}/promote",
    operation_id = "operator_promote_ca",
    params(("id" = String, Path,), ("ca" = String, Path,)),
    responses((status = 204), (status = 403), (status = 404)))]
pub async fn promote_ca(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path((id, ca)): Path<(TenantId, String)>,
) -> ApiResult<axum::http::StatusCode> {
    crate::routes::tenant_ca::gate_tenant(&state, &auth, id, "operator_promote").await?;
    crate::routes::tenant_ca::promote_for(&state, &auth, id, &ca).await?;
    audit_write(
        &state,
        &auth,
        "ca.promote",
        serde_json::json!({ "tenant": id.0, "ca": ca }),
    )
    .await;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[utoipa::path(post, path = "/api/v1/operator/nodes/{id}/revoke",
    operation_id = "operator_revoke_node", params(("id" = String, Path,)),
    responses((status = 204), (status = 403), (status = 404)))]
pub async fn revoke_node(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<NodeId>,
) -> ApiResult<axum::http::StatusCode> {
    let tenant = crate::routes::nodes::node_tenant(&state, id).await?;
    auth.require(&state, Permission::NodeManage, Scope::Tenant(tenant))
        .await?;
    // `tenant` was just resolved FROM this node, so the repository's
    // tenant-scoped write reaches exactly the same row (MAIN-252 owns `nodes`).
    state.nodes.revoke(id, tenant).await?;
    audit_write(
        &state,
        &auth,
        "node.revoke",
        serde_json::json!({ "node": id.0 }),
    )
    .await;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[utoipa::path(delete, path = "/api/v1/operator/nodes/{id}",
    operation_id = "operator_remove_node", params(("id" = String, Path,)),
    responses((status = 204), (status = 403), (status = 404)))]
pub async fn remove_node(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<NodeId>,
) -> ApiResult<axum::http::StatusCode> {
    let tenant = crate::routes::nodes::node_tenant(&state, id).await?;
    auth.require(&state, Permission::NodeManage, Scope::Tenant(tenant))
        .await?;
    state.nodes.delete(tenant, id).await?;
    audit_write(
        &state,
        &auth,
        "node.remove",
        serde_json::json!({ "node": id.0 }),
    )
    .await;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// Who holds what. Needed before granting is meaningful — you cannot revoke a
/// binding you cannot see.
#[utoipa::path(get, path = "/api/v1/operator/bindings",
    operation_id = "operator_list_bindings",
    params(PageQuery),
    responses((status = 200, body = Page<BindingRow>), (status = 403)))]
pub async fn bindings(
    State(state): State<AppState>,
    auth: AuthCtx,
    Query(q): Query<PageQuery>,
) -> ApiResult<Json<Page<BindingRow>>> {
    auth.require(&state, Permission::RbacGrant, Scope::Deployment)
        .await?;
    let page = operator_queries::operator_bindings_page(&*state.operator, &q).await?;
    audit(&state, &auth, "bindings", None).await;
    Ok(Json(page))
}

fn slugify(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        "org".into()
    } else {
        s
    }
}

// ── per-tenant switches, from outside the tenant (QOL 4) ────────────────────
//
// `settings::put` writes to `auth.tenant_id`, so turning loops on for another
// team meant switching into it first. A brand-new team therefore starts with
// `loops.enabled` off (MAIN-239's shipped default), its promoted tickets sit
// queued forever, and nothing anywhere says which switch is off. That is what
// "the loops did not fire for my PM" looks like from the operator's side.

/// The tenant's display name, and the 404 for one that does not exist. Reuses
/// the node repository's lookup rather than adding a second one.
async fn tenant_name(state: &AppState, id: TenantId) -> ApiResult<String> {
    state
        .nodes
        .tenant_names(&[id])
        .await?
        .remove(&id.0)
        .ok_or(crate::error::ApiError::NotFound)
}

/// The two switches, resolved for one tenant.
fn switch_key(switch: &str) -> Option<&'static str> {
    // A closed set, deliberately: this endpoint must never become a way to
    // write arbitrary settings into somebody else's tenant.
    match switch {
        "loops" => Some(crate::services::loops::KEY),
        "reconcile" => Some(crate::services::session_reconcile::KEY),
        _ => None,
    }
}

async fn read_switches(
    state: &AppState,
    tenant: TenantId,
    name: String,
) -> ApiResult<TenantSwitches> {
    let on = |v: Option<serde_json::Value>| {
        matches!(v, Some(serde_json::Value::Bool(true)))
            || matches!(v.as_ref().and_then(|x| x.as_str()), Some("true"))
    };
    Ok(TenantSwitches {
        tenant_id: tenant,
        tenant_name: name,
        loops_enabled: on(state
            .settings
            .tenant_value(tenant, crate::services::loops::KEY)
            .await?),
        reconcile_enabled: on(state
            .settings
            .tenant_value(tenant, crate::services::session_reconcile::KEY)
            .await?),
    })
}

#[utoipa::path(get, path = "/api/v1/operator/tenants/{id}/switches",
    operation_id = "tenant_switches",
    params(("id" = String, Path,)),
    responses((status = 200, body = TenantSwitches), (status = 403), (status = 404)))]
pub async fn tenant_switches(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<TenantId>,
) -> ApiResult<Json<TenantSwitches>> {
    // Scoped to the TARGET tenant, so a deployment operator passes and a tenant
    // admin passes for their own — and nobody reads a team they have no say in.
    auth.require(&state, Permission::TenantManage, Scope::Tenant(id))
        .await?;
    let name = tenant_name(&state, id).await?;
    Ok(Json(read_switches(&state, id, name).await?))
}

#[utoipa::path(post, path = "/api/v1/operator/tenants/{id}/switches",
    operation_id = "set_tenant_switch",
    params(("id" = String, Path,)),
    request_body = SetTenantSwitchRequest,
    responses((status = 200, body = TenantSwitches), (status = 400), (status = 403), (status = 404)))]
pub async fn set_tenant_switch(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<TenantId>,
    Json(req): Json<SetTenantSwitchRequest>,
) -> ApiResult<Json<TenantSwitches>> {
    auth.require(&state, Permission::TenantManage, Scope::Tenant(id))
        .await?;
    let key = switch_key(&req.switch).ok_or_else(|| {
        crate::error::ApiError::BadRequest("switch must be `loops` or `reconcile`".into())
    })?;
    let name = tenant_name(&state, id).await?;

    state
        .settings
        .put(crate::repo::admin::SettingWrite {
            tenant: id,
            scope: "tenant".into(),
            user: None,
            key: key.to_string(),
            value: serde_json::Value::Bool(req.enabled),
        })
        .await?;

    // Throwing a switch in a tenant you are not standing in is exactly the kind
    // of act that should leave a trace in that tenant's own feed.
    crate::events::record(
        &state,
        id,
        crate::events::EventDraft::new("tenant.switch_changed")
            .actor("user", auth.user_id.0)
            .payload(serde_json::json!({
                "switch": req.switch,
                "enabled": req.enabled,
                "key": key,
            })),
    )
    .await;

    Ok(Json(read_switches(&state, id, name).await?))
}
