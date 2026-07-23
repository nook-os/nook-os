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

use axum::extract::{Path, State};
use axum::Json;
use nook_types::*;
use uuid::Uuid;

use crate::auth::perm::{Permission, Scope};
use crate::auth::AuthCtx;
use crate::error::ApiResult;
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
    let rows: Vec<OperatorOrg> = sqlx::query_as(
        "SELECT o.id, o.name, o.slug, o.created_at,
                (SELECT count(*) FROM tenants t WHERE t.org_id = o.id) AS tenants
         FROM orgs o ORDER BY o.name",
    )
    .fetch_all(&state.db)
    .await?;
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
    responses((status = 200, body = [OperatorTenant]), (status = 403)))]
pub async fn tenants(
    State(state): State<AppState>,
    auth: AuthCtx,
) -> ApiResult<Json<Vec<OperatorTenant>>> {
    auth.require(&state, Permission::TenantView, Scope::Deployment)
        .await?;

    let mut rows: Vec<OperatorTenant> = sqlx::query_as(
        "SELECT t.id, t.slug, t.org_id, t.created_at,
                (SELECT count(*) FROM users u WHERE u.tenant_id = t.id)    AS members,
                (SELECT count(*) FROM nodes n WHERE n.tenant_id = t.id)    AS nodes,
                (SELECT count(*) FROM sessions s
                  WHERE s.tenant_id = t.id
                    AND s.status IN ('starting','running','detached'))     AS active_sessions,
                (SELECT count(*) FROM workspaces w WHERE w.tenant_id = t.id) AS workspaces
         FROM tenants t ORDER BY t.created_at",
    )
    .fetch_all(&state.db)
    .await?;

    // Policy ADDS. Absent unless an org has opted in, and absent by default.
    for row in &mut rows {
        enrich(&state, row).await?;
    }

    audit(&state, &auth, "tenants", None).await;
    Ok(Json(rows))
}

/// Add policy-gated fields, one opt-in at a time.
///
/// Each field is fetched only if its org has enabled it. The default path
/// touches no extra tables at all, which is what makes "off" the cheap case as
/// well as the safe one.
async fn enrich(state: &AppState, row: &mut OperatorTenant) -> ApiResult<()> {
    let Some(org) = row.org_id else { return Ok(()) };

    if policy::enabled(&state.db, org, Field::RepositoryNames).await? {
        let names: Vec<(String,)> = sqlx::query_as(
            "SELECT name FROM workspaces WHERE tenant_id = $1 ORDER BY name LIMIT 50",
        )
        .bind(row.id)
        .fetch_all(&state.db)
        .await?;
        row.repositories = Some(names.into_iter().map(|(n,)| n).collect());
    }
    if policy::enabled(&state.db, org, Field::TaskTitles).await? {
        let titles: Vec<(String,)> = sqlx::query_as(
            "SELECT title FROM tasks WHERE tenant_id = $1 ORDER BY created_at DESC LIMIT 50",
        )
        .bind(row.id)
        .fetch_all(&state.db)
        .await?;
        row.task_titles = Some(titles.into_iter().map(|(t,)| t).collect());
    }
    Ok(())
}

/// Nodes, always visible. Names, status, resources, owner, session count.
#[utoipa::path(get, path = "/api/v1/operator/nodes",
    operation_id = "operator_list_nodes",
    responses((status = 200, body = [OperatorNode]), (status = 403)))]
pub async fn nodes(
    State(state): State<AppState>,
    auth: AuthCtx,
) -> ApiResult<Json<Vec<OperatorNode>>> {
    auth.require(&state, Permission::NodeView, Scope::Deployment)
        .await?;
    let rows: Vec<OperatorNode> = sqlx::query_as(
        "SELECT n.id, n.name, n.platform, n.status, n.last_seen_at, n.resources,
                n.tenant_id, t.slug AS tenant_slug,
                (SELECT count(*) FROM sessions s
                  WHERE s.node_id = n.id
                    AND s.status IN ('starting','running','detached')) AS active_sessions
         FROM nodes n JOIN tenants t ON t.id = n.tenant_id
         ORDER BY n.name",
    )
    .fetch_all(&state.db)
    .await?;
    audit(&state, &auth, "nodes", None).await;
    Ok(Json(rows))
}

/// The audit trail, including operator reads themselves.
#[utoipa::path(get, path = "/api/v1/operator/audit",
    operation_id = "operator_audit",
    responses((status = 200, body = [OperatorAuditEntry]), (status = 403)))]
pub async fn audit_log(
    State(state): State<AppState>,
    auth: AuthCtx,
) -> ApiResult<Json<Vec<OperatorAuditEntry>>> {
    auth.require(&state, Permission::AuditView, Scope::Deployment)
        .await?;
    // Kinds, actors and times — never payloads. An event payload can carry a
    // branch name or a task title, which is exactly what this surface must not
    // hand over without policy.
    let rows: Vec<OperatorAuditEntry> = sqlx::query_as(
        "SELECT e.id, e.kind, e.actor_type, e.actor_id, e.tenant_id,
                t.slug AS tenant_slug, e.occurred_at
         FROM events e JOIN tenants t ON t.id = e.tenant_id
         WHERE e.kind LIKE 'operator.%' OR e.kind LIKE 'rbac.%'
            OR e.kind LIKE 'node.%'     OR e.kind LIKE 'user.%'
         ORDER BY e.occurred_at DESC LIMIT 200",
    )
    .fetch_all(&state.db)
    .await?;
    audit(&state, &auth, "audit", None).await;
    Ok(Json(rows))
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
    Ok(Json(policy::current(&state.db, id).await?))
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
    Ok(Json(policy::current(&state.db, id).await?))
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
    auth.require(&state, Permission::RbacGrant, Scope::Deployment)
        .await?;

    let user: Option<(uuid::Uuid,)> =
        sqlx::query_as("SELECT id FROM users WHERE lower(email) = lower($1)")
            .bind(&req.email)
            .fetch_optional(&state.db)
            .await?;
    let (user_id,) = user.ok_or_else(|| crate::error::ApiError::NotFound)?;

    if req.revoke {
        sqlx::query(
            "DELETE FROM role_bindings
             WHERE subject_id = $1 AND role_key = $2 AND scope_type = 'deployment'",
        )
        .bind(user_id)
        .bind(&req.role)
        .execute(&state.db)
        .await?;
    } else {
        sqlx::query(
            "INSERT INTO role_bindings (id, subject_type, subject_id, role_key, scope_type, scope_id, created_by)
             VALUES ($1, 'user', $2, $3, 'deployment', NULL, $4)
             ON CONFLICT DO NOTHING",
        )
        .bind(uuid::Uuid::now_v7())
        .bind(user_id)
        .bind(&req.role)
        .bind(auth.user_id.0)
        .execute(&state.db)
        .await?;
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

    let row: OperatorOrg = sqlx::query_as(
        "INSERT INTO orgs (id, name, slug) VALUES ($1, $2, $3)
         RETURNING id, name, slug, created_at, 0::bigint AS tenants",
    )
    .bind(Uuid::now_v7())
    .bind(name)
    .bind(&slug)
    .fetch_one(&state.db)
    .await?;

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
    let row: Option<OperatorOrg> = sqlx::query_as(
        "UPDATE orgs SET name = $2, updated_at = now() WHERE id = $1
         RETURNING id, name, slug, created_at,
                   (SELECT count(*) FROM tenants t WHERE t.org_id = orgs.id) AS tenants",
    )
    .bind(id)
    .bind(req.name.trim())
    .fetch_optional(&state.db)
    .await?;
    let row = row.ok_or(crate::error::ApiError::NotFound)?;
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
    let current: Option<(Option<Uuid>, String)> =
        sqlx::query_as("SELECT org_id, slug FROM tenants WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.db)
            .await?;
    let (from, slug) = current.ok_or(crate::error::ApiError::NotFound)?;

    if let Some(from) = from {
        auth.require(&state, Permission::OrgManage, Scope::Org(from))
            .await?;
    }
    auth.require(&state, Permission::OrgManage, Scope::Org(req.org_id))
        .await?;

    sqlx::query("UPDATE tenants SET org_id = $2, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(req.org_id)
        .execute(&state.db)
        .await?;

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
    sqlx::query("UPDATE nodes SET revoked_at = now(), updated_at = now() WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;
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
    sqlx::query("DELETE FROM nodes WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;
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
    responses((status = 200, body = [BindingRow]), (status = 403)))]
pub async fn bindings(
    State(state): State<AppState>,
    auth: AuthCtx,
) -> ApiResult<Json<Vec<BindingRow>>> {
    auth.require(&state, Permission::RbacGrant, Scope::Deployment)
        .await?;
    let rows: Vec<BindingRow> = sqlx::query_as(
        "SELECT b.id, u.email, u.display_name, b.role_key, b.scope_type, b.scope_id,
                COALESCE(o.slug, t.slug) AS scope_label, b.created_at
         FROM role_bindings b
         JOIN users u ON u.id = b.subject_id
         LEFT JOIN orgs o    ON b.scope_type = 'org'    AND o.id = b.scope_id
         LEFT JOIN tenants t ON b.scope_type = 'tenant' AND t.id = b.scope_id
         ORDER BY b.scope_type, u.email",
    )
    .fetch_all(&state.db)
    .await?;
    audit(&state, &auth, "bindings", None).await;
    Ok(Json(rows))
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
