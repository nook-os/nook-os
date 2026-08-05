use axum::extract::{Path, Query, State};
use axum::Json;
use nook_types::*;

use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::events::{self, EventDraft};
use crate::services::{identity::slugify, workspace_queries};
use crate::state::AppState;
use nook_proto::ControlToNode;

#[utoipa::path(get, path = "/api/v1/workspaces",
    operation_id = "list_workspaces",
    responses((status = 200, body = [WorkspaceDetail])))]
pub async fn list(
    State(state): State<AppState>,
    auth: AuthCtx,
) -> ApiResult<Json<Vec<WorkspaceDetail>>> {
    Ok(Json(
        workspace_queries::list_workspaces(&*state.workspaces, auth.tenant_id).await?,
    ))
}

/// The paged twin of the whole-list read — the table view's endpoint. The
/// whole list stays for the pickers (workspace switcher, dispatch target),
/// which genuinely want everything.
#[utoipa::path(get, path = "/api/v1/workspaces/page",
    operation_id = "workspaces_page",
    params(PageQuery),
    responses((status = 200, body = Page<WorkspaceDetail>)))]
pub async fn page(
    State(state): State<AppState>,
    auth: AuthCtx,
    Query(q): Query<PageQuery>,
) -> ApiResult<Json<Page<WorkspaceDetail>>> {
    Ok(Json(
        workspace_queries::workspaces_page(&*state.workspaces, auth.tenant_id, &q).await?,
    ))
}

#[utoipa::path(get, path = "/api/v1/workspaces/{id}",
    operation_id = "get_workspace",
    params(("id" = String, Path,)),
    responses((status = 200, body = WorkspaceDetail), (status = 404)))]
pub async fn get_one(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<WorkspaceId>,
) -> ApiResult<Json<WorkspaceDetail>> {
    workspace_queries::get_workspace(&*state.workspaces, auth.tenant_id, id)
        .await?
        .map(Json)
        .ok_or(ApiError::NotFound)
}

/// `GET /api/v1/workspaces/{id}/session-spec` — the declared desired session
/// state, or `null` for an unmanaged workspace (MAIN-315 AC-2).
///
/// Tenant-scoped only, like every other workspace read: a workspace belongs to
/// the tenant, not to a person, so there is no owner to gate against here (that
/// distinction is MAIN-314's, where a NODE has an owner).
#[utoipa::path(get, path = "/api/v1/workspaces/{id}/session-spec",
    operation_id = "get_session_spec",
    params(("id" = String, Path,)),
    responses((status = 200, body = Option<SessionSpec>), (status = 404)))]
pub async fn get_session_spec(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<WorkspaceId>,
) -> ApiResult<Json<Option<SessionSpec>>> {
    let ws = state
        .workspaces
        .get(auth.tenant_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    // A spec stored before a field was added should not 404 the whole read, so a
    // value that no longer parses reads as unmanaged rather than as an error.
    Ok(Json(ws.session_spec.and_then(|v| {
        serde_json::from_value::<SessionSpec>(v).ok()
    })))
}

/// `GET /api/v1/workspaces/{id}/reconcile-status` — desired vs actual (MAIN-319).
///
/// Runs the reconciler's OWN planner against the reconciler's own view of the
/// fleet. A second implementation would drift, and the first symptom would be a
/// UI confidently reporting a placement the loop does not agree with.
///
/// Tenant-scoped like every other workspace read (AC-4): a workspace in another
/// tenant is not found, so this cannot report on somebody else's fleet.
#[utoipa::path(get, path = "/api/v1/workspaces/{id}/reconcile-status",
    operation_id = "get_reconcile_status",
    params(("id" = String, Path,)),
    responses((status = 200, body = ReconcileStatus), (status = 404)))]
pub async fn reconcile_status(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<WorkspaceId>,
) -> ApiResult<Json<ReconcileStatus>> {
    use crate::services::session_reconcile as recon;

    let ws = state
        .workspaces
        .get(auth.tenant_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let enabled = recon::enabled(&*state.settings, auth.tenant_id).await;

    // What the loop reconciles this workspace toward — derived EXACTLY as `pass`
    // does, or the status lies: an explicit spec wins; otherwise an enabled
    // tenant auto-derives the default (MAIN-233 follow-up), and a disabled tenant
    // with no spec is genuinely unmanaged. Reporting "unmanaged" for a workspace
    // the loop is about to place a default session on is the drift this endpoint
    // exists to prevent.
    let explicit = ws
        .session_spec
        .and_then(|v| serde_json::from_value::<SessionSpec>(v).ok());
    let spec = match explicit {
        Some(s) => Some(s),
        None if enabled => Some(recon::default_spec()),
        None => None,
    };
    let Some(spec) = spec else {
        return Ok(Json(ReconcileStatus {
            enabled,
            managed: false,
            desired: 0,
            running: 0,
            shortfall: 0,
            // An unmanaged workspace has no reconciler-run sessions to cap, so
            // the flag is about nothing here — false rather than "unknown".
            port_capped: false,
            blocked: Vec::new(),
            eligible: 0,
        }));
    };

    let nodes = recon::node_facts(&state, auth.tenant_id).await?;
    let checkouts: Vec<recon::CheckoutSlot> = state
        .workspaces
        .present_checkouts(auth.tenant_id, id)
        .await?
        .into_iter()
        .map(|c| recon::CheckoutSlot {
            checkout_id: c.id,
            node_id: c.node_id,
            path: c.path,
        })
        .collect();
    let actual: Vec<recon::Actual> = state
        .sessions
        // The WORKSPACE's declaration, which is what this endpoint reports on.
        // The control plane's review loop (MAIN-326) is not a workspace's to
        // account for, and counting it here would read as a replica nobody
        // asked for.
        .live_managed(auth.tenant_id, id, Some(ManagedPurpose::Access))
        .await?
        .into_iter()
        .map(|(session_id, checkout_id, node_id)| recon::Actual {
            session_id,
            checkout_id,
            node_id,
        })
        .collect();
    // The same question the reconciler asks, asked the same way (MAIN-361), so
    // the number on screen is the number the loop is acting on rather than a
    // second opinion about it.
    let ports = recon::port_safety(&state, auth.tenant_id, id).await?;
    let plan = recon::plan(&spec, &nodes, &checkouts, &actual, ports);

    // Names, so the UI can say "waiting on a clone to dev-box" rather than
    // printing a uuid at somebody.
    let named: std::collections::HashMap<_, _> = state
        .nodes
        // Home-tenant only: this names nodes in a reconcile plan, which is
        // tenant-local work (MAIN-353 NG-2/NG-3).
        .list(auth.tenant_id, None, None)
        .await?
        .into_iter()
        .map(|n| (n.id, n.name))
        .collect();

    Ok(Json(ReconcileStatus {
        enabled,
        managed: true,
        desired: plan.desired as u32,
        running: actual.len() as u32,
        shortfall: plan.shortfall as u32,
        port_capped: plan.capped,
        blocked: plan
            .needs_clone
            .iter()
            .map(|id| ReconcileBlocker {
                node_id: *id,
                node_name: named.get(id).cloned().unwrap_or_default(),
                reason: nook_types::NodeBlocker::NeedsClone,
            })
            .collect(),
        eligible: (plan.desired.saturating_sub(plan.shortfall)) as u32,
    }))
}

/// Reject a spec that cannot mean anything (MAIN-315 AC-3).
///
/// `replicas >= 0` is free — `count` is a `u32`, so a negative never parses.
/// What is NOT free is the empty string: a selector key or runtime of `""`
/// would match nothing and silently strand the workspace, so it is refused
/// rather than stored.
fn validate_spec(spec: &SessionSpec) -> ApiResult<()> {
    if spec.runtime.trim().is_empty() {
        return Err(ApiError::BadRequest(
            "a session spec needs a runtime".into(),
        ));
    }
    for (k, v) in &spec.node_selector {
        if k.trim().is_empty() {
            return Err(ApiError::BadRequest(
                "a node-selector key cannot be empty".into(),
            ));
        }
        if v.trim().is_empty() {
            return Err(ApiError::BadRequest(format!(
                "node-selector {k:?} has an empty value — it would match nothing"
            )));
        }
    }
    for t in &spec.tolerations {
        if t.key.trim().is_empty() {
            return Err(ApiError::BadRequest("a toleration needs a key".into()));
        }
        if t.effect != "NoSchedule" {
            return Err(ApiError::BadRequest(format!(
                "{:?} is not a taint effect — expected NoSchedule",
                t.effect
            )));
        }
    }
    Ok(())
}

/// `PUT /api/v1/workspaces/{id}/session-spec` — declare it, or clear it with
/// `{"spec": null}` to return the workspace to unmanaged (MAIN-315 AC-2).
#[utoipa::path(put, path = "/api/v1/workspaces/{id}/session-spec",
    operation_id = "set_session_spec",
    params(("id" = String, Path,)),
    request_body = SetSessionSpecRequest,
    responses((status = 200, body = Option<SessionSpec>), (status = 400), (status = 404)))]
pub async fn set_session_spec(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<WorkspaceId>,
    Json(req): Json<SetSessionSpecRequest>,
) -> ApiResult<Json<Option<SessionSpec>>> {
    // A person declares desired state; a node credential is not a person.
    auth.require_user()?;
    if let Some(spec) = &req.spec {
        validate_spec(spec)?;
    }
    let stored =
        match &req.spec {
            Some(spec) => Some(serde_json::to_value(spec).map_err(|_| {
                ApiError::BadRequest("that session spec could not be stored".into())
            })?),
            None => None,
        };
    let ws = state
        .workspaces
        .set_session_spec(auth.tenant_id, id, stored)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(ws.session_spec.and_then(|v| {
        serde_json::from_value::<SessionSpec>(v).ok()
    })))
}

/// `GET /api/v1/workspaces/{id}/ports` — the port requirements in force.
///
/// Always answers with what WILL be leased, not with the raw column: an
/// undeclared workspace reports the default listener rather than an empty list,
/// because "you get NOOK_PORT" and "you get nothing" are different states and a
/// UI showing an empty table for the first would be lying.
#[utoipa::path(get, path = "/api/v1/workspaces/{id}/ports",
    operation_id = "get_port_requirements",
    params(("id" = String, Path,)),
    responses((status = 200, body = [PortRequirement]), (status = 404)))]
pub async fn get_port_requirements(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<WorkspaceId>,
) -> ApiResult<Json<Vec<PortRequirement>>> {
    state
        .workspaces
        .get(auth.tenant_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(
        crate::services::port_leases::requirements_of(&state, auth.tenant_id, Some(id)).await?,
    ))
}

/// How long a checkout may carry `missing_at` and still yield its key.
///
/// Discovery re-reports every node's checkouts on a 300s `DISCOVERY_INTERVAL`,
/// so a genuinely transient `missing_at` — a node restarting, a scan that ran
/// while a directory was briefly unreadable — clears within one cycle. Three
/// cycles of slack absorbs a missed one without opening the window further.
///
/// The alternative shapes are both wrong. Ignoring `missing_at` entirely (what
/// this route did before) means pruning a workspace from a node never revokes
/// its key access: the reaper that finally deletes the row runs at
/// `workspace_missing_retention_secs` (default 7 days) and is itself gated on
/// `loops.enabled`, which ships OFF — so in practice the access never ends,
/// while "prune" reads to an operator as a revocation gesture. Refusing on any
/// `missing_at` is the other extreme, and it is what MAIN-363 was hurt by: a
/// clone landing outside the scan roots was flagged missing while sitting
/// perfectly on disk, and refusing here turned that into an authentication
/// failure two layers away.
const CHECKOUT_MISSING_GRACE_SECS: i64 = 15 * 60;

/// `GET /api/v1/nodes/{node_id}/workspaces/{id}/git-key` — the workspace's ssh
/// key, as material for ONE ssh invocation (MAIN-367).
///
/// The deliberate delivery channel, and the only one. It exists because git
/// authenticates by forking `ssh`, so a shim on the machine has to be able to
/// obtain a key; `nook get workspace git-ssh` is that shim and this is what it
/// calls.
///
/// Keyed on NODE and WORKSPACE rather than on a session, which is the whole
/// point of its shape. A git credential is workspace data, not session content:
/// anchoring it to a session forced it through `session_for_content` and made a
/// cross-tenant node widen that guard for all nine session routes — far more
/// than fetching a key needs.
///
/// **MACHINE-ONLY, and that is the load-bearing part** (owner's ruling on
/// MAIN-367, 2026-08-03). This route hands back a decrypted private key, so a
/// human credential must not reach it. `require_node_may_use` was the wrong
/// guard: its user leg admits any member of a tenant the node is SHARED with,
/// and a shared operator node running other tenants' workspaces is the normal
/// case — so a session cookie was enough to read any key off it. Cross-tenant
/// placement (MAIN-353) still works, because a machine asking about itself never
/// consults tenants at all.
///
/// AC-7 holds as ruled: the key reaches no browser, no log and no event payload.
/// Absence from the OpenAPI surface is NOT what makes that true — the guard is.
/// 204 when the workspace pins nothing, which is the ordinary case — the shim
/// then execs plain ssh and the node's own key applies.
#[utoipa::path(get, path = "/api/v1/nodes/{node_id}/workspaces/{id}/git-key",
    operation_id = "workspace_git_key",
    params(("node_id" = String, Path,), ("id" = String, Path,)),
    responses((status = 200), (status = 204), (status = 404)))]
pub async fn git_key(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path((node_id, id)): Path<(NodeId, WorkspaceId)>,
) -> ApiResult<axum::response::Response> {
    use axum::response::IntoResponse;

    // Delivering key material, so: this machine, about itself, and no human.
    auth.require_node_is_self(node_id)?;

    // The workspace must actually be checked out ON that node. Without this a
    // node could name any workspace id and be handed its key; with it, the node
    // may only ask about repos it demonstrably holds.
    let owner = state
        .workspaces
        .checkout_owner_at_node(node_id, id, CHECKOUT_MISSING_GRACE_SECS)
        .await?
        .ok_or(ApiError::NotFound)?;

    match crate::services::workspace_git_key(&state, owner, id).await {
        Some(material) => Ok(material.into_response()),
        None => Ok(axum::http::StatusCode::NO_CONTENT.into_response()),
    }
}

/// `PUT /api/v1/workspaces/{id}/credential` — pin the ssh key this repo clones
/// and fetches with, or unpin it with `null` (MAIN-367).
///
/// Returns the workspace, never the key: the private half leaves this process
/// only as transient material delivered to a node for one git command, and an
/// endpoint a browser can reach must never be a way to read it back (AC-7).
#[utoipa::path(put, path = "/api/v1/workspaces/{id}/credential",
    operation_id = "set_workspace_credential",
    params(("id" = String, Path,)),
    request_body = SetWorkspaceCredentialRequest,
    responses((status = 200, body = Workspace), (status = 404)))]
pub async fn set_credential(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<WorkspaceId>,
    Json(req): Json<SetWorkspaceCredentialRequest>,
) -> ApiResult<Json<Workspace>> {
    // A person chooses a key; a node credential is not a person.
    auth.require_user()?;
    // Resolve it in THIS tenant before storing, so a credential id from another
    // tenant is a 404 here rather than a foreign key error later.
    if let Some(cred) = req.credential_id {
        if state
            .git_credentials
            .sealed_secret(cred, auth.tenant_id)
            .await?
            .is_none()
        {
            return Err(ApiError::NotFound);
        }
    }
    let ws = state
        .workspaces
        .set_git_credential(auth.tenant_id, id, req.credential_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    events::record(
        &state,
        auth.tenant_id,
        EventDraft::new("workspace.credential_set")
            .actor("user", auth.user_id.0)
            .workspace(id)
            // The id, never the key.
            .payload(serde_json::json!({ "credential_id": req.credential_id })),
    )
    .await;
    Ok(Json(ws))
}

/// `PUT /api/v1/workspaces/{id}/ports` — declare what this repo binds.
///
/// The declaration is the workspace's, which is the whole point of MAIN-301's
/// second cut: the control plane leases numbers and never learns what `PORT`
/// means to anybody.
#[utoipa::path(put, path = "/api/v1/workspaces/{id}/ports",
    operation_id = "set_port_requirements",
    params(("id" = String, Path,)),
    request_body = SetPortRequirementsRequest,
    responses((status = 200, body = [PortRequirement]), (status = 400), (status = 404)))]
pub async fn set_port_requirements(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<WorkspaceId>,
    Json(req): Json<SetPortRequirementsRequest>,
) -> ApiResult<Json<Vec<PortRequirement>>> {
    // A person declares desired state; a node credential is not a person.
    auth.require_user()?;
    if let Some(reqs) = &req.requirements {
        validate_requirements(reqs)?;
    }
    let stored = match &req.requirements {
        Some(reqs) => Some(serde_json::to_value(reqs).map_err(|_| {
            ApiError::BadRequest("those port requirements could not be stored".into())
        })?),
        None => None,
    };
    state
        .workspaces
        .set_port_requirements(auth.tenant_id, id, stored)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(
        crate::services::port_leases::requirements_of(&state, auth.tenant_id, Some(id)).await?,
    ))
}

/// Refuse a declaration that could not be honoured, at the point somebody typed
/// it — not at the point a session fails to start on a machine they are not
/// looking at.
fn validate_requirements(reqs: &[PortRequirement]) -> ApiResult<()> {
    let mut seen: std::collections::BTreeSet<&str> = Default::default();
    for r in reqs {
        if r.name.trim().is_empty() || r.env.trim().is_empty() {
            return Err(ApiError::BadRequest(
                "every port requirement needs a name and an env var".into(),
            ));
        }
        // The env var is spliced into the session's environment by the node, so
        // it has to BE an environment variable name. A value with `=` or a
        // space in it would either be silently dropped or corrupt its
        // neighbours, and neither failure would point back here.
        if !r.env.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            || r.env.starts_with(|c: char| c.is_ascii_digit())
        {
            return Err(ApiError::BadRequest(format!(
                "`{}` is not a usable environment variable name (letters, digits and underscore, not starting with a digit)",
                r.env
            )));
        }
        if !matches!(r.protocol.as_str(), "tcp" | "udp") {
            return Err(ApiError::BadRequest(format!(
                "`{}` is not a protocol this leases — tcp or udp",
                r.protocol
            )));
        }
        // Names key the leases, so a duplicate is not a redundancy: the second
        // would overwrite the first's lease and the workspace would end up with
        // fewer ports than it declared.
        if !seen.insert(r.name.as_str()) {
            return Err(ApiError::BadRequest(format!(
                "two requirements are both called `{}`",
                r.name
            )));
        }
    }
    Ok(())
}

#[derive(serde::Deserialize, utoipa::IntoParams)]
pub struct GitQuery {
    pub node_id: NodeId,
}

/// Live git status + working-tree diff, relayed from the node.
#[utoipa::path(get, path = "/api/v1/workspaces/{id}/git",
    operation_id = "workspace_git_status",
    params(("id" = String, Path,), GitQuery),
    responses((status = 200, body = GitStatusResponse), (status = 404)))]
pub async fn git_status(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<WorkspaceId>,
    axum::extract::Query(q): axum::extract::Query<GitQuery>,
) -> ApiResult<Json<GitStatusResponse>> {
    let Some(path) = state
        .workspaces
        .checkout_path(auth.tenant_id, id, q.node_id)
        .await?
    else {
        return Err(ApiError::NotFound);
    };

    let rx = state
        .registry
        .request_git_status(q.node_id, path)
        .ok_or_else(|| ApiError::BadRequest("node is offline".into()))?;
    let payload = tokio::time::timeout(std::time::Duration::from_secs(10), rx)
        .await
        .map_err(|_| ApiError::BadRequest("node did not answer in time".into()))?
        .map_err(|_| ApiError::BadRequest("node disconnected".into()))?;

    Ok(Json(GitStatusResponse {
        is_repo: payload.is_repo,
        branch: payload.branch,
        dirty: !payload.files.is_empty(),
        files: payload.files,
        diff: payload.diff,
    }))
}

/// Clone this workspace's **stored** remote onto a node (MAIN-223 AC-2).
///
/// The caller names only the node — the URL comes off the workspace — and the
/// resulting checkout is associated with THIS workspace id, not re-derived from
/// the remote by the next discovery scan. Authorization is the same
/// person-based rule sessions use: the node must be the caller's own or shared.
#[utoipa::path(post, path = "/api/v1/workspaces/{id}/clone",
    operation_id = "clone_workspace_to_node",
    params(("id" = String, Path,)),
    request_body = WorkspaceCloneRequest,
    responses((status = 200, body = OpResponse), (status = 400), (status = 403), (status = 404)))]
pub async fn clone_to_node(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<WorkspaceId>,
    Json(req): Json<WorkspaceCloneRequest>,
) -> ApiResult<Json<OpResponse>> {
    // The workspace must exist in this tenant and carry a stored URL.
    let url = state
        .workspaces
        .git_remote_url(id, auth.tenant_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let url = url.ok_or_else(|| {
        ApiError::BadRequest(
            "this workspace has no stored git remote URL — clone it once with an \
             explicit URL first, and the workspace will remember it"
                .into(),
        )
    })?;

    // Same person-based authorization as starting a session: own or shared node.
    crate::auth::require_person_may_use_node(
        &state,
        auth.tenant_id,
        Some(auth.user_id),
        req.node_id,
    )
    .await?;

    // Decrypt the chosen tenant credential (if any) for transient node use.
    let ssh_key = match req.credential_id {
        None => None,
        Some(cred_id) => {
            let enc = state
                .git_credentials
                .sealed_secret(cred_id, auth.tenant_id)
                .await?
                .ok_or(ApiError::NotFound)?;
            Some(
                state
                    .vault
                    .decrypt_string(&enc)
                    .map_err(ApiError::Internal)?,
            )
        }
    };

    let job_id = uuid::Uuid::now_v7().to_string();
    events::record(
        &state,
        auth.tenant_id,
        EventDraft::new("git.clone_started")
            .actor("user", auth.user_id.0)
            .node(req.node_id)
            .workspace(id)
            .payload(serde_json::json!({ "url": url, "job_id": job_id })),
    )
    .await;

    // Let the node derive the directory from the URL, exactly like a manual clone.
    let tenant_slug = crate::services::tenant_slug(&state, auth.tenant_id).await;
    let rx = state
        .registry
        .request_op(req.node_id, |request_id| ControlToNode::CloneRepo {
            request_id,
            url: url.clone(),
            dest_name: None,
            ssh_key,
            tenant_slug,
        })
        .ok_or_else(|| ApiError::BadRequest("node is offline".into()))?;

    let payload = match tokio::time::timeout(std::time::Duration::from_secs(90), rx).await {
        Ok(Ok(p)) => p,
        Ok(Err(_)) => return Err(ApiError::BadRequest("node disconnected mid-clone".into())),
        Err(_) => {
            return Ok(Json(OpResponse {
                ok: false,
                path: None,
                message: "clone still running — watch the activity feed".into(),
            }))
        }
    };

    // Associate the fresh checkout with THIS workspace id — no name/remote
    // re-derivation roulette.
    if payload.ok {
        if let Some(path) = payload.path.as_deref() {
            associate_cloned_checkout(&state, auth.tenant_id, req.node_id, id, path, &url).await?;
        }
    }

    events::record(
        &state,
        auth.tenant_id,
        EventDraft::new("git.clone_finished")
            .actor("user", auth.user_id.0)
            .node(req.node_id)
            .workspace(id)
            .payload(serde_json::json!({
                "url": url, "ok": payload.ok, "message": payload.message, "job_id": job_id
            })),
    )
    .await;

    Ok(Json(OpResponse {
        ok: payload.ok,
        path: payload.path,
        message: payload.message,
    }))
}

/// Pin a freshly-cloned checkout to `workspace_id` (MAIN-223 AC-2), bypassing the
/// normalized-remote matching discovery would otherwise use. `ON CONFLICT
/// (node_id, path)` heals a re-clone in place; `kind = 'clone'` so worktrees and
/// clone-only picks resolve it. Also adopts the remote onto the workspace when it
/// had none, keeping its identity fresh.
pub async fn associate_cloned_checkout(
    state: &AppState,
    tenant: TenantId,
    node_id: NodeId,
    workspace_id: WorkspaceId,
    path: &str,
    url: &str,
) -> ApiResult<()> {
    let normalized = crate::services::discovery::normalize_remote(url);
    state
        .workspaces
        .associate_clone(tenant, node_id, workspace_id, path, url, &normalized)
        .await?;
    // Adopt the normalized remote onto the workspace when it has none — but only
    // if no OTHER workspace already owns it. `workspaces_remote_idx` is unique per
    // (tenant, normalized), so an unguarded UPDATE would abort the whole clone on a
    // repo that is already known under a different workspace. The checkout is
    // pinned by id regardless (that is the point), so skipping the adoption here
    // costs nothing.
    state
        .workspaces
        .adopt_normalized_remote(workspace_id, tenant, &normalized)
        .await?;
    Ok(())
}

#[utoipa::path(post, path = "/api/v1/workspaces",
    operation_id = "create_workspace",
    request_body = CreateWorkspaceRequest,
    responses((status = 200, body = Workspace)))]
pub async fn create(
    State(state): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<CreateWorkspaceRequest>,
) -> ApiResult<Json<Workspace>> {
    let workspace = state
        .workspaces
        .create(
            auth.tenant_id,
            &req.name,
            &slugify(&req.name),
            req.description.clone(),
            req.git_remote_url
                .as_deref()
                .filter(|u| !u.trim().is_empty()),
        )
        .await?;
    Ok(Json(workspace))
}

/// Rename a workspace — the label only.
///
/// Deliberately does NOT touch the slug, the checkouts on disk, or the git
/// remote: those are the workspace's identity, and rediscovery matches on
/// them. So calling a clone of `acme/services` "the flaky one" costs nothing
/// and breaks nothing — no directory moves, no session loses its path, and
/// the next heartbeat won't create a duplicate workspace.
#[utoipa::path(patch, path = "/api/v1/workspaces/{id}",
    operation_id = "rename_workspace",
    params(("id" = String, Path,)),
    request_body = RenameWorkspaceRequest,
    responses((status = 200, body = Workspace), (status = 400), (status = 404)))]
pub async fn rename(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<WorkspaceId>,
    Json(req): Json<RenameWorkspaceRequest>,
) -> ApiResult<Json<Workspace>> {
    let name = req.name.trim();
    if name.is_empty() {
        return Err(ApiError::BadRequest("a workspace needs a name".into()));
    }
    if name.chars().count() > 120 {
        return Err(ApiError::BadRequest(
            "workspace name must be 120 characters or fewer".into(),
        ));
    }

    // Read the old name first: the event carries both ends of the rename, and a
    // missing workspace is a 404 rather than an UPDATE that silently matches
    // nothing.
    let previous = state
        .workspaces
        .get(auth.tenant_id, id)
        .await?
        .ok_or(ApiError::NotFound)?
        .name;

    let workspace = state
        .workspaces
        .rename(id, auth.tenant_id, name)
        .await?
        .ok_or(ApiError::NotFound)?;

    // A `workspace.*` event is what makes every other open tab redraw the new
    // name without a refresh.
    events::record(
        &state,
        auth.tenant_id,
        EventDraft::new("workspace.renamed")
            .actor("user", auth.user_id.0)
            .workspace(id)
            .payload(serde_json::json!({ "from": previous, "to": name })),
    )
    .await;

    Ok(Json(workspace))
}

#[utoipa::path(delete, path = "/api/v1/workspaces/{id}",
    operation_id = "delete_workspace",
    params(("id" = String, Path,)),
    request_body = DeleteWorkspaceRequest,
    responses(
        (status = 200, body = DeleteWorkspaceResponse),
        (status = 404),
        (status = 409, description = "the workspace still has live sessions")))]
pub async fn delete(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<WorkspaceId>,
    body: Option<Json<DeleteWorkspaceRequest>>,
) -> ApiResult<Json<DeleteWorkspaceResponse>> {
    let Json(req) = body.unwrap_or_default();

    let workspace = state
        .workspaces
        .get(auth.tenant_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;

    // Live sessions would be killed by the cascade with their tmux left
    // orphaned on the node, so they have to be dealt with first — but only the
    // ones a PERSON started. A managed session exists solely because this
    // workspace declares it, and telling the caller to go kill those was an
    // unwinnable instruction: the reconciler restarts them within a pass, so
    // "kill them first" could never be satisfied and the workspace could not be
    // deleted at all (MAIN-363). Deleting the declaration is the only way to
    // stop them, so deletion is exactly what may.
    // EVERY purpose, not just the workspace's own: a review loop is equally
    // something deleting the declaration is the only way to stop, so counting it
    // as a session "somebody started" would make the workspace undeletable.
    let managed = state
        .sessions
        .live_managed(auth.tenant_id, id, None)
        .await?;
    let live = state.workspaces.live_session_count(id).await?;
    let unmanaged = live.saturating_sub(managed.len() as i64).max(0);
    if unmanaged > 0 {
        return Err(ApiError::Conflict(format!(
            "{unmanaged} live session(s) somebody started — kill them first"
        )));
    }

    let mut stranded = 0usize;

    let checkouts: Vec<(NodeId, String)> = state
        .workspaces
        .checkouts_of(id)
        .await?
        .into_iter()
        .map(|c| (c.node_id, c.path))
        .collect();
    let total = checkouts.len();
    let mut removed = 0usize;

    if req.delete_files {
        // Worktrees first: removing a primary clone out from under its linked
        // worktrees would leave them dangling.
        let mut ordered = checkouts.clone();
        ordered.sort_by_key(|(_, path)| path.matches('/').count());
        ordered.reverse();
        for (node_id, path) in ordered {
            let Some(rx) =
                state
                    .registry
                    .request_op(node_id, |request_id| ControlToNode::RemoveCheckout {
                        request_id,
                        path: path.clone(),
                    })
            else {
                continue; // node offline — the checkout stays
            };
            if let Ok(Ok(payload)) =
                tokio::time::timeout(std::time::Duration::from_secs(30), rx).await
            {
                if payload.ok {
                    removed += 1;
                } else {
                    tracing::warn!(%node_id, error = %payload.message, "checkout removal failed");
                }
            }
        }
    }

    // Stop the managed sessions ourselves, kill-then-mark like the reconciler:
    // end the row alone and the tmux session keeps running with nothing left
    // that knows about it. A node that is offline cannot be told, and the
    // workspace is going away regardless — so that is reported rather than
    // treated as a reason to refuse, which would put us back in the deadlock.
    //
    // Immediately before the delete, NOT before the file removal above: that
    // part waits on node ops, and a reconcile pass landing in the gap would
    // start fresh managed sessions for a workspace we are about to cascade —
    // which is the orphaned-tmux case this whole ordering exists to avoid.
    for (session, _checkout, node) in &managed {
        if !state.registry.send_to_node(
            *node,
            ControlToNode::KillSession {
                session_id: *session,
            },
        ) {
            stranded += 1;
            continue;
        }
        if let Err(e) = state.sessions.mark_ended(auth.tenant_id, *session).await {
            tracing::warn!(workspace = %id, session = %session, error = %e, "could not end a managed session while deleting its workspace");
        }
    }

    // Cascades node_workspaces, sessions, notes and secrets; tasks and events
    // keep their history with a null workspace.
    state.workspaces.delete(id, auth.tenant_id).await?;

    events::record(
        &state,
        auth.tenant_id,
        EventDraft::new("workspace.deleted")
            .actor("user", auth.user_id.0)
            .payload(serde_json::json!({
                "name": workspace.name,
                "checkouts_removed": removed,
                "deleted_files": req.delete_files,
            })),
    )
    .await;

    let remaining = total - removed;
    let mut message = if remaining > 0 {
        format!(
            "deleted '{}' — {remaining} checkout(s) still on disk and will be \
             rediscovered until removed",
            workspace.name
        )
    } else {
        format!("deleted '{}'", workspace.name)
    };
    if stranded > 0 {
        // Said out loud: the row is gone, so nothing will retry this, and a
        // tmux session nobody now tracks is exactly the thing an operator wants
        // to hear about at the moment it happens.
        message.push_str(&format!(
            " — {stranded} managed session(s) could not be stopped (node offline); \
             their tmux may still be running"
        ));
    }
    Ok(Json(DeleteWorkspaceResponse {
        deleted: true,
        checkouts_removed: removed,
        checkouts_remaining: remaining,
        message,
    }))
}
