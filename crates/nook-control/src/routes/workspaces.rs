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
        .map(|s| recon::Actual {
            session_id: s.id,
            checkout_id: s.checkout_id,
            node_id: s.node_id,
            shard: nook_types::ShardAssignment {
                index: s.managed_shard.max(0) as u32,
                of: s.managed_shards.max(1) as u32,
            },
        })
        .collect();
    // The same question the reconciler asks, asked the same way (MAIN-361), so
    // the number on screen is the number the loop is acting on rather than a
    // second opinion about it.
    let ports = recon::port_safety(&state, auth.tenant_id, id).await?;
    // The WORKSPACE's own declaration counts nodes, not shards — a person's
    // terminal in every checkout is what it asks for (MAIN-446).
    let plan = recon::plan(
        &spec,
        &nodes,
        &checkouts,
        &actual,
        ports,
        recon::Spread::PerCheckout,
    );

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

/// `POST /api/v1/workspaces/{id}/reconcile-preview` — what would this spec do
/// (MAIN-431)?
///
/// Runs the reconciler's OWN planner against a CANDIDATE spec and writes
/// nothing: no session, no spec, no clone, no row of any kind. The same four
/// reads `reconcile_status` makes, the same `plan()`, and per-node explanation
/// through the same `blockers()` the planner itself decides with — a second
/// eligibility implementation here is exactly the drift MAIN-319 closed.
///
/// Tenant-scoped like every other workspace read, and open to anyone who can
/// see the workspace (the `GET /nodes/{id}/placement` precedent): a preview
/// decides nothing, so there is nothing to gate on ownership.
#[utoipa::path(post, path = "/api/v1/workspaces/{id}/reconcile-preview",
    operation_id = "reconcile_preview",
    params(("id" = String, Path,)),
    request_body = ReconcilePreviewRequest,
    responses((status = 200, body = ReconcilePreview), (status = 400), (status = 404)))]
pub async fn reconcile_preview(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<WorkspaceId>,
    Json(req): Json<ReconcilePreviewRequest>,
) -> ApiResult<Json<ReconcilePreview>> {
    use crate::services::session_reconcile as recon;

    if state.workspaces.get(auth.tenant_id, id).await?.is_none() {
        return Err(ApiError::NotFound);
    }
    // The same rejections the saved-spec path applies (MAIN-315 AC-3): a blank
    // runtime, an empty selector key or value, a taint effect that is not
    // NoSchedule. An unknown-but-nonempty runtime is deliberately NOT a 400 —
    // no runtime catalog exists (that is a sibling card), and whether a runtime
    // is launchable is a per-node fact, answered truthfully below as
    // `runtime_unavailable` blockers on each node that reported its list.
    validate_spec(&req.spec)?;

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
        .live_managed(auth.tenant_id, id, Some(ManagedPurpose::Access))
        .await?
        .into_iter()
        .map(|s| recon::Actual {
            session_id: s.id,
            checkout_id: s.checkout_id,
            node_id: s.node_id,
            shard: nook_types::ShardAssignment {
                index: s.managed_shard.max(0) as u32,
                of: s.managed_shards.max(1) as u32,
            },
        })
        .collect();
    let ports = recon::port_safety(&state, auth.tenant_id, id).await?;
    let plan = recon::plan(
        &req.spec,
        &nodes,
        &checkouts,
        &actual,
        ports,
        recon::Spread::PerCheckout,
    );

    let named: std::collections::HashMap<_, _> = state
        .nodes
        .list(auth.tenant_id, None, None)
        .await?
        .into_iter()
        .map(|n| (n.id, n.name))
        .collect();
    let name_of = |id: &nook_types::NodeId| named.get(id).cloned().unwrap_or_default();

    // Classify every node once, with the SAME rule the plan used. Eligible
    // and holding a checkout is `matched`; eligible without one is exactly
    // `plan.needs_clone` (asserted by construction: both sides are
    // `blockers().is_empty()` plus checkout presence); everything else is
    // ineligible, carrying all its grounds.
    let mut matched = Vec::new();
    let mut ineligible = Vec::new();
    for node in &nodes {
        let reasons = recon::blockers(&req.spec, node);
        if reasons.is_empty() {
            if checkouts.iter().any(|c| c.node_id == node.id) {
                matched.push(nook_types::PreviewNode {
                    node_id: node.id,
                    node_name: name_of(&node.id),
                });
            }
            // Eligible without a checkout: reported below from the plan's own
            // list rather than re-derived here.
        } else {
            ineligible.push(nook_types::PreviewBlockedNode {
                node_id: node.id,
                node_name: name_of(&node.id),
                reasons,
            });
        }
    }

    Ok(Json(ReconcilePreview {
        matched,
        needs_clone: plan
            .needs_clone
            .iter()
            .map(|id| ReconcileBlocker {
                node_id: *id,
                node_name: name_of(id),
                reason: nook_types::NodeBlocker::NeedsClone,
            })
            .collect(),
        ineligible,
        desired: plan.desired as u32,
        placed: plan.placed as u32,
        shortfall: plan.shortfall as u32,
        capped: plan.capped,
    }))
}

/// `GET /api/v1/workspaces/{id}/gh-token` — does this workspace hold its own
/// forge token (MAIN-456)? Reports ONLY the fact; the token never leaves the
/// vault through any read path.
#[utoipa::path(get, path = "/api/v1/workspaces/{id}/gh-token",
    operation_id = "get_workspace_gh_token",
    params(("id" = String, Path,)),
    responses((status = 200, body = WorkspaceGhTokenState), (status = 404)))]
pub async fn get_gh_token(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<WorkspaceId>,
) -> ApiResult<Json<WorkspaceGhTokenState>> {
    if state.workspaces.get(auth.tenant_id, id).await?.is_none() {
        return Err(ApiError::NotFound);
    }
    Ok(Json(WorkspaceGhTokenState {
        set: state
            .workspaces
            .gh_token_sealed(auth.tenant_id, id)
            .await?
            .is_some(),
    }))
}

/// `PUT /api/v1/workspaces/{id}/gh-token` — set or clear the workspace's own
/// forge token (MAIN-456). Sealed with the same vault the git credentials use.
///
/// Multi-tenant is the reason this exists: one fleet-wide token means every
/// tenant's verdicts post as one identity and the control plane holds a
/// credential with reach into every tenant's forge. The workspace token
/// OUTRANKS the fleet variable everywhere a forge is spoken to.
///
/// **The token is exercised before it is sealed (MAIN-469).** A token that
/// cannot authenticate, cannot see the repository, or cannot perform the writes
/// verdict delivery performs is refused with a 400 naming what is missing —
/// because both under-configurations otherwise fail late and silently: a dead
/// token makes the demand poll read as "no PRs", and a read-only one runs a
/// whole review before dying at `POST issues/comments`.
#[utoipa::path(put, path = "/api/v1/workspaces/{id}/gh-token",
    operation_id = "set_workspace_gh_token",
    params(("id" = String, Path,)),
    request_body = SetWorkspaceGhTokenRequest,
    responses((status = 200, body = WorkspaceGhTokenState), (status = 400), (status = 404)))]
pub async fn set_gh_token(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<WorkspaceId>,
    Json(req): Json<SetWorkspaceGhTokenRequest>,
) -> ApiResult<Json<WorkspaceGhTokenState>> {
    // A person configures a credential; a node token is not a person — the
    // same rule every workspace declaration applies.
    auth.require_user()?;
    let sealed = match req.token.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(t) => {
            check_forge_token(&state, auth.tenant_id, id, t).await?;
            Some(
                state
                    .vault
                    .encrypt(t.as_bytes())
                    .map_err(|_| ApiError::BadRequest("could not seal the token".into()))?,
            )
        }
    };
    let set = sealed.is_some();
    if !state
        .workspaces
        .set_gh_token_sealed(auth.tenant_id, id, sealed)
        .await?
    {
        return Err(ApiError::NotFound);
    }
    // The forge cache may hold an answer fetched with the OLD identity.
    state.review_demand.forget(id);
    Ok(Json(WorkspaceGhTokenState { set }))
}

/// Exercise a pasted token against the workspace's own repository (MAIN-469).
///
/// Nothing to exercise is not a failure: a workspace with no remote, or one
/// whose remote is not a GitHub repository this build can name, has no repo to
/// check the token against — and refusing the paste because we cannot check it
/// would make a local-path workspace unable to hold a token at all.
async fn check_forge_token(
    state: &AppState,
    tenant: nook_types::TenantId,
    id: WorkspaceId,
    token: &str,
) -> ApiResult<()> {
    let Some(ws) = state.workspaces.get(tenant, id).await? else {
        return Err(ApiError::NotFound);
    };
    let Some(repo) = ws
        .git_remote_url
        .as_deref()
        .and_then(crate::services::forge::github_repo)
    else {
        return Ok(());
    };
    crate::services::forge::GithubForge::from_token(token)
        .check_access(&repo)
        .await
        .map_err(|refusal| ApiError::BadRequest(refusal.to_string()))
}

/// `GET /api/v1/workspaces/{id}/review-loop` — the ceiling on review loops for
/// this repo (MAIN-445 AC-2).
///
/// Reports the RAW column, unlike `/ports` which resolves its default before
/// answering. The difference is deliberate: a port declaration's default is a
/// fact about what will be leased, while `null` here is a fact about what
/// nobody has decided. Resolving it to `1` would erase exactly the distinction
/// AC-4 needs to print "unset (default 1)".
#[utoipa::path(get, path = "/api/v1/workspaces/{id}/review-loop",
    operation_id = "get_review_loop",
    params(("id" = String, Path,)),
    responses((status = 200, body = ReviewLoopDeclaration), (status = 404)))]
pub async fn get_review_loop(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<WorkspaceId>,
) -> ApiResult<Json<ReviewLoopDeclaration>> {
    let ws = state
        .workspaces
        .get(auth.tenant_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(ReviewLoopDeclaration {
        max_replicas: ws.review_loop_max_replicas,
    }))
}

/// `PUT /api/v1/workspaces/{id}/review-loop` — set it, or clear it back to
/// unset with `{"max_replicas": null}` (MAIN-445 AC-2).
#[utoipa::path(put, path = "/api/v1/workspaces/{id}/review-loop",
    operation_id = "set_review_loop",
    params(("id" = String, Path,)),
    request_body = SetReviewLoopRequest,
    responses((status = 200, body = ReviewLoopDeclaration), (status = 400), (status = 404)))]
pub async fn set_review_loop(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<WorkspaceId>,
    Json(req): Json<SetReviewLoopRequest>,
) -> ApiResult<Json<ReviewLoopDeclaration>> {
    // A person declares desired state; a node credential is not a person — the
    // same rule `set_session_spec` applies, for the same reason.
    auth.require_user()?;
    let max_replicas = parse_max_replicas(&req.max_replicas)?;
    let ws = state
        .workspaces
        .set_review_loop_max_replicas(auth.tenant_id, id, max_replicas)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(ReviewLoopDeclaration {
        max_replicas: ws.review_loop_max_replicas,
    }))
}

/// `GET /api/v1/workspaces/{id}/build-loop` — the ceiling on build runs for
/// this repo (MAIN-461 AC-1), `/review-loop`'s twin. Reports the RAW column
/// for the same reason: `null` is "nobody decided", and resolving it to 1
/// would erase the distinction the CLI prints as "unset (default 1)".
#[utoipa::path(get, path = "/api/v1/workspaces/{id}/build-loop",
    operation_id = "get_build_loop",
    params(("id" = String, Path,)),
    responses((status = 200, body = BuildLoopDeclaration), (status = 404)))]
pub async fn get_build_loop(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<WorkspaceId>,
) -> ApiResult<Json<BuildLoopDeclaration>> {
    let ws = state
        .workspaces
        .get(auth.tenant_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(BuildLoopDeclaration {
        max_replicas: ws.build_max_replicas,
    }))
}

/// `PUT /api/v1/workspaces/{id}/build-loop` — set it, or clear it back to
/// unset with `{"max_replicas": null}` (MAIN-461 AC-1).
#[utoipa::path(put, path = "/api/v1/workspaces/{id}/build-loop",
    operation_id = "set_build_loop",
    params(("id" = String, Path,)),
    request_body = SetBuildLoopRequest,
    responses((status = 200, body = BuildLoopDeclaration), (status = 400), (status = 404)))]
pub async fn set_build_loop(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<WorkspaceId>,
    Json(req): Json<SetBuildLoopRequest>,
) -> ApiResult<Json<BuildLoopDeclaration>> {
    // A person declares desired state; a node credential is not a person — the
    // same rule the review-loop PUT applies, for the same reason.
    auth.require_user()?;
    let max_replicas = parse_max_replicas(&req.max_replicas)?;
    let ws = state
        .workspaces
        .set_build_max_replicas(auth.tenant_id, id, max_replicas)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(BuildLoopDeclaration {
        max_replicas: ws.build_max_replicas,
    }))
}

/// `GET /api/v1/workspaces/{id}/build-loop-settings` — the per-workspace build
/// loop switch (MAIN-385 AC-8): is the control plane firing runs for this repo
/// by itself, where are they pinned, how many at once, and who said so.
#[utoipa::path(get, path = "/api/v1/workspaces/{id}/build-loop-settings",
    operation_id = "get_build_loop_settings",
    params(("id" = String, Path,)),
    responses((status = 200, body = BuildLoopSettings), (status = 404)))]
pub async fn get_build_loop_settings(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<WorkspaceId>,
) -> ApiResult<Json<BuildLoopSettings>> {
    let ws = state
        .workspaces
        .get(auth.tenant_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    settings_of(&state, auth.tenant_id, &ws).await.map(Json)
}

/// `PUT /api/v1/workspaces/{id}/build-loop-settings` — turn the loop on or
/// off, pin or unpin a node, set the concurrency (MAIN-385 AC-8).
///
/// Every field is optional and an absent one leaves that setting alone, so
/// `{"enabled": true}` is a complete request. Turning it ON records the CALLER
/// as the identity auto-fired runs are requested by (AC-2) — which is why a
/// node credential is refused here: a job requested by a machine resolves to
/// no person, and no node would ever be eligible for it.
#[utoipa::path(put, path = "/api/v1/workspaces/{id}/build-loop-settings",
    operation_id = "set_build_loop_settings",
    params(("id" = String, Path,)),
    request_body = SetBuildLoopSettingsRequest,
    responses((status = 200, body = BuildLoopSettings), (status = 400), (status = 404)))]
pub async fn set_build_loop_settings(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<WorkspaceId>,
    Json(req): Json<SetBuildLoopSettingsRequest>,
) -> ApiResult<Json<BuildLoopSettings>> {
    auth.require_user()?;
    let actor = auth.user_id;
    let ws = state
        .workspaces
        .get(auth.tenant_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;

    // Read-modify-write: the request names only what it wants changed, and the
    // repository stores the whole triple (see `set_build_loop`).
    //
    // The three states are ABSENT / null / value, and they are three because
    // unpinning has to be expressible: a caller who cannot clear a pin cannot
    // undo a machine that has since been retired.
    let enabled = req.enabled.unwrap_or(ws.build_loop_enabled);
    let node = match &req.node {
        None => ws.build_loop_node_id,
        Some(None) => None,
        Some(Some(ident)) => Some(resolve_pin(&state, auth.tenant_id, ident).await?),
    };
    // The enabler is stamped whenever the switch is on — including a caller who
    // changes a pin on a loop somebody else enabled, because they are the
    // person now answerable for the runs it fires.
    let enabled_by = if enabled {
        Some(actor)
    } else {
        ws.build_loop_enabled_by
    };

    if let Some(concurrency) = req.concurrency {
        if concurrency.is_some_and(|n| n < 0) {
            return Err(ApiError::BadRequest(
                "concurrency must be a non-negative integer, or null to unset".into(),
            ));
        }
        state
            .workspaces
            .set_build_max_replicas(auth.tenant_id, id, concurrency)
            .await?
            .ok_or(ApiError::NotFound)?;
    }
    let ws = state
        .workspaces
        .set_build_loop(auth.tenant_id, id, enabled, node, enabled_by)
        .await?
        .ok_or(ApiError::NotFound)?;

    // Enabling is itself an occasion to evaluate (AC-6's reason, applied to the
    // switch): a repo enabled with ready cards on the board should not wait a
    // sweep interval to prove it works.
    if ws.build_loop_enabled {
        let (bg, tenant, ws) = (state.clone(), auth.tenant_id, ws.clone());
        tokio::spawn(async move {
            if crate::services::loops::enabled(&*bg.settings, tenant).await {
                if let Err(e) = crate::services::build_loop::evaluate(&bg, tenant, &ws).await {
                    tracing::warn!(workspace = %ws.id, error = %e, "build loop enable-nudge failed");
                }
            }
        });
    }
    settings_of(&state, auth.tenant_id, &ws).await.map(Json)
}

/// The response both halves answer with. The pin's NAME is joined here rather
/// than by the caller, so a terminal can print "pinned to azul" from one read.
async fn settings_of(
    state: &AppState,
    tenant: TenantId,
    ws: &nook_types::Workspace,
) -> ApiResult<BuildLoopSettings> {
    let node_name = match ws.build_loop_node_id {
        Some(node) => state.nodes.get(tenant, node).await?.map(|n| n.name),
        None => None,
    };
    Ok(BuildLoopSettings {
        enabled: ws.build_loop_enabled,
        node_id: ws.build_loop_node_id,
        node_name,
        // `converge_builds`' own reading, so the number reported is the number
        // acted on: unset is one, and 0 is this repo's kill switch.
        concurrency: ws.build_max_replicas.unwrap_or(1).max(0) as u32,
        enabled_by: ws.build_loop_enabled_by,
    })
}

/// A pin by node id or by name, within this tenant. Online is deliberately NOT
/// required: pinning a machine that is currently down is a legitimate thing to
/// say, and what happens next — the job queues with a reason naming it — is
/// exactly AC-4's contract.
async fn resolve_pin(state: &AppState, tenant: TenantId, ident: &str) -> ApiResult<NodeId> {
    let ident = ident.trim();
    let nodes = state.nodes.list_ids_and_names(tenant).await?;
    if let Ok(id) = ident.parse::<uuid::Uuid>() {
        if let Some((id, _)) = nodes.iter().find(|(n, _)| n.0 == id) {
            return Ok(*id);
        }
    }
    nodes
        .into_iter()
        .find(|(_, name)| name == ident)
        .map(|(id, _)| id)
        .ok_or_else(|| ApiError::BadRequest(format!("no node named {ident:?} in this tenant")))
}

/// `GET /api/v1/workspaces/{id}/build-loop-status` — desired versus
/// DELIVERABLE for the build loop (MAIN-495 AC-1), `/review-loop-status`'s
/// twin.
///
/// The one it cannot borrow is the planner. Reviews resolve a desired number
/// through the reconciler's `plan_now`; builds have no such thing — what is
/// owed is decided from the board at the moment `converge_builds` runs. So
/// `desired` here is the DECLARATION, and the question this endpoint exists to
/// answer is whether the number somebody typed can be honoured by the machines
/// they own at all: a ceiling of three against one node's two slots changes
/// nothing observable, and the third run simply queues forever.
///
/// Advisory, never a gate (AC-5). `PUT /build-loop` still takes any valid
/// number, because fleet capacity changes without warning and a refusal
/// correct at write time is wrong an hour later.
#[utoipa::path(get, path = "/api/v1/workspaces/{id}/build-loop-status",
    operation_id = "get_build_loop_status",
    params(("id" = String, Path,)),
    responses((status = 200, body = BuildLoopStatus), (status = 404)))]
pub async fn build_loop_status(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<WorkspaceId>,
) -> ApiResult<Json<BuildLoopStatus>> {
    let ws = state
        .workspaces
        .get(auth.tenant_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;

    // `converge_builds`' own reading of the column, so the ceiling reported is
    // the ceiling acted on: unset is the default of one, and an explicit 0 is
    // the repo's kill switch.
    let desired = ws.build_max_replicas.unwrap_or(1).max(0) as u32;
    let running = state
        .jobs
        .live_build_states(auth.tenant_id, id)
        .await?
        .iter()
        .filter(|s| s.as_str() != "queued")
        .count() as u32;

    // The VIEWER's own eligible nodes (AC-2): node ownership keys on the
    // person, and a tenant-wide total would report the fleet's size while a job
    // waits behind one machine's two slots.
    //
    // The scope is the nodes routes' own, resolved here because it is an access
    // decision: this response NAMES nodes, and a member may not learn the
    // identity of a machine `/api/v1/nodes` would not have shown them. It moves
    // no number — every build candidate is the viewer's own — only which
    // blocked machines are named.
    let scope = crate::routes::nodes::visibility_scope(&state, &auth).await?;
    let cap =
        crate::services::jobs::build_capacity(&state, auth.tenant_id, auth.user_id, scope).await?;

    Ok(Json(BuildLoopStatus {
        desired,
        running,
        // Against CAPACITY, not against what is busy right now: a ceiling the
        // fleet can reach is healthy however little of it is in use this
        // second, and a ceiling it cannot reach is short even while idle.
        shortfall: desired.saturating_sub(cap.capacity),
        capacity: cap.capacity,
        eligible_nodes: cap.eligible,
        blocked: cap.blocked,
    }))
}

/// `null` clears; anything else must be a non-negative integer that fits the
/// column (AC-2). Every rejection names the field, because the caller's next
/// move is to fix that key and a message that does not say which one costs a
/// round trip.
fn parse_max_replicas(v: &serde_json::Value) -> ApiResult<Option<i32>> {
    if v.is_null() {
        return Ok(None);
    }
    let n = v.as_i64().ok_or_else(|| {
        ApiError::BadRequest("max_replicas must be a non-negative integer".into())
    })?;
    if n < 0 {
        return Err(ApiError::BadRequest(
            "max_replicas must be a non-negative integer".into(),
        ));
    }
    i32::try_from(n)
        .map(Some)
        .map_err(|_| ApiError::BadRequest("max_replicas is too large".into()))
}

/// `GET /api/v1/workspaces/{id}/review-loop-status` — desired vs actual for the
/// review loop (MAIN-447 AC-4).
///
/// The sibling of `/reconcile-status`, and separate from it on purpose: that
/// one reports the workspace's own `SessionSpec` and filters this purpose out,
/// because two declarations converge per workspace and neither should be able
/// to describe the other.
///
/// Both tenant gates are reported rather than one `enabled`, so the UI can name
/// the switch that is off (AC-5). `pass()` skips the workspace entirely when
/// `loops.enabled` is off, so a plan reported with that gate down is what WOULD
/// converge, not what is converging.
#[utoipa::path(get, path = "/api/v1/workspaces/{id}/review-loop-status",
    operation_id = "get_review_loop_status",
    params(("id" = String, Path,)),
    responses((status = 200, body = ReviewLoopStatus), (status = 404)))]
pub async fn review_loop_status(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<WorkspaceId>,
) -> ApiResult<Json<ReviewLoopStatus>> {
    use crate::services::session_reconcile as recon;

    let ws = state
        .workspaces
        .get(auth.tenant_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;

    // The forge's own count, from the cache the reconcile pass reads (MAIN-448)
    // — not a second lookup. The comment below anticipated this card, and its
    // point stands: reporting the ceiling while the loop converges on
    // `min(open_prs, ceiling)` would be the drift it warns about.
    let gh_token = crate::services::workspace_gh_token(&state, auth.tenant_id, id).await;
    let spec = recon::review_declaration(&state, &ws, gh_token.as_deref()).await;
    // The node's ceiling is spent across the tenant's workspaces in ascending
    // id order (MAIN-452), so reporting on one of them means replaying what the
    // ones before it took. Without this a workspace on a full node reports the
    // plan it would get if it had the fleet to itself.
    let taken = recon::review_budget_before(&state, auth.tenant_id, id).await?;
    // The same call `pass()` makes, with the same purpose and the same slots —
    // so this reports the plan the loop acts on rather than a second opinion
    // that drifts when `review_loop_spec` changes.
    let (plan, actual) = recon::plan_now(
        &state,
        auth.tenant_id,
        id,
        &spec,
        ManagedPurpose::ReviewLoop,
        recon::Slots::ClonesOnly,
        // The review loop divides by SHARD, not by checkout (MAIN-446). Passing
        // the pass's own spread is what keeps this a report rather than a
        // second opinion.
        recon::Spread::Sharded,
        &taken,
    )
    .await?;

    let named: std::collections::HashMap<_, _> = state
        .nodes
        .list(auth.tenant_id, None, None)
        .await?
        .into_iter()
        .map(|n| (n.id, n.name))
        .collect();

    Ok(Json(ReviewLoopStatus {
        reconcile_enabled: recon::enabled(&*state.settings, auth.tenant_id).await,
        loops_enabled: crate::services::loops::enabled(&*state.settings, auth.tenant_id).await,
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
        // Read AFTER the plan, because planning is what performs the poll this
        // reports on — read first, a cold status call would always say the
        // forge was fine (MAIN-469).
        forge_trouble: state.review_demand.trouble(id),
    }))
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
    for m in &managed {
        if !state
            .registry
            .send_to_node(m.node_id, ControlToNode::KillSession { session_id: m.id })
        {
            stranded += 1;
            continue;
        }
        if let Err(e) = state.sessions.mark_ended(auth.tenant_id, m.id).await {
            tracing::warn!(workspace = %id, session = %m.id, error = %e, "could not end a managed session while deleting its workspace");
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
