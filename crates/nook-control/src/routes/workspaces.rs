use axum::extract::{Path, State};
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
    let rx = state
        .registry
        .request_op(req.node_id, |request_id| ControlToNode::CloneRepo {
            request_id,
            url: url.clone(),
            dest_name: None,
            ssh_key,
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
    // orphaned on the node — make the caller deal with them first.
    let live = state.workspaces.live_session_count(id).await?;
    if live > 0 {
        return Err(ApiError::Conflict(format!(
            "{live} live session(s) — kill them first"
        )));
    }

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
    let message = if remaining > 0 {
        format!(
            "deleted '{}' — {remaining} checkout(s) still on disk and will be \
             rediscovered until removed",
            workspace.name
        )
    } else {
        format!("deleted '{}'", workspace.name)
    };
    Ok(Json(DeleteWorkspaceResponse {
        deleted: true,
        checkouts_removed: removed,
        checkouts_remaining: remaining,
        message,
    }))
}
