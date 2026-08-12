//! Git-powerhouse endpoints: tenant credentials (vault), clone-onto-node,
//! worktrees, and workspace secret files.

use axum::extract::{Path, State};
use axum::Json;
use nook_proto::ControlToNode;
use nook_types::*;

use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::events::{self, EventDraft};
use crate::repo::workspaces::CheckoutRef;
use crate::services::secrets;
use crate::state::AppState;

// ── Tenant git credentials ──────────────────────────────────────────────────

#[utoipa::path(get, path = "/api/v1/git-credentials",
    operation_id = "list_git_credentials",
    responses((status = 200, body = [GitCredential])))]
pub async fn list_credentials(
    State(state): State<AppState>,
    auth: AuthCtx,
) -> ApiResult<Json<Vec<GitCredential>>> {
    let creds = state.git_credentials.list(auth.tenant_id).await?;
    Ok(Json(creds))
}

#[utoipa::path(post, path = "/api/v1/git-credentials",
    operation_id = "create_git_credential",
    request_body = CreateGitCredentialRequest,
    responses((status = 200, body = GitCredential)))]
pub async fn create_credential(
    State(state): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<CreateGitCredentialRequest>,
) -> ApiResult<Json<GitCredential>> {
    let (private_key, public_key) = if req.generate {
        generate_keypair(&req.name).await?
    } else {
        let key = req
            .private_key
            .clone()
            .filter(|k| !k.trim().is_empty())
            .ok_or_else(|| {
                ApiError::BadRequest("provide private_key or set generate:true".into())
            })?;
        // REFUSED, not stored empty. `unwrap_or_default()` here used to accept a
        // key `ssh-keygen -y` could not read and save it with `public_key = ""`.
        // That credential is worse than none: it stores fine, pins fine, and
        // then fails at clone time as an opaque authentication error nobody
        // connects back to this moment — the same "fails an hour later" trap
        // AC-8 exists to close for deletion.
        //
        // The common cause is a PASSPHRASE-PROTECTED key, which cannot work
        // here whatever we do: the node runs git unattended and has nothing to
        // answer the prompt with. Failing at the door names the problem while
        // the person who has the key is still looking at it.
        //
        // It is also what lets the UI promise a public half to copy — without a
        // derivable one there is nothing to authorize on the git host, so the
        // credential could never have been used.
        let public = derive_public_key(&key).await.ok_or_else(|| {
            ApiError::BadRequest(
                "could not read a public key out of that private key. A \
                 passphrase-protected key cannot be used unattended — a node \
                 running git has no way to answer the prompt. Supply an \
                 unencrypted key, or generate one here instead."
                    .into(),
            )
        })?;
        (key, public)
    };

    let enc = state
        .vault
        .encrypt(private_key.as_bytes())
        .map_err(ApiError::Internal)?;

    let cred = state
        .git_credentials
        .create(auth.tenant_id, &req.name, &public_key, enc, auth.user_id)
        .await?;

    events::record(
        &state,
        auth.tenant_id,
        EventDraft::new("git.credential_added")
            .actor("user", auth.user_id.0)
            .payload(serde_json::json!({ "name": cred.name })),
    )
    .await;
    Ok(Json(cred))
}

#[utoipa::path(delete, path = "/api/v1/git-credentials/{id}",
    operation_id = "delete_git_credential",
    params(("id" = String, Path,)),
    responses((status = 204), (status = 404)))]
pub async fn delete_credential(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<GitCredentialId>,
) -> ApiResult<axum::http::StatusCode> {
    // Refuse while anything still depends on it, and say what (MAIN-367). A key
    // that vanishes underneath a workspace does not fail here — it fails an hour
    // later as an authentication error on a clone, which nobody connects back to
    // a deletion. Naming the workspaces makes the fix obvious: unpin them first.
    let used_by = state
        .git_credentials
        .workspaces_using(id, auth.tenant_id)
        .await?;
    if !used_by.is_empty() {
        return Err(ApiError::Conflict(format!(
            "still used by {}: unpin it there first",
            used_by.join(", ")
        )));
    }

    let res = state.git_credentials.delete(id, auth.tenant_id).await?;
    if res == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// Generate an ed25519 keypair server-side (ssh-keygen in a temp dir).
async fn generate_keypair(comment: &str) -> ApiResult<(String, String)> {
    let comment = format!("nookos-{}", crate::services::identity::slugify(comment));
    tokio::task::spawn_blocking(move || {
        let dir = std::env::temp_dir().join(format!("nook-keygen-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).map_err(|e| anyhow::anyhow!(e))?;
        let key = dir.join("id_ed25519");
        let out = std::process::Command::new("ssh-keygen")
            .args(["-t", "ed25519", "-N", "", "-C", &comment, "-f"])
            .arg(&key)
            .output()
            .map_err(|e| anyhow::anyhow!("ssh-keygen unavailable: {e}"))?;
        if !out.status.success() {
            anyhow::bail!(
                "ssh-keygen failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let private = std::fs::read_to_string(&key)?;
        let public = std::fs::read_to_string(key.with_extension("pub"))?
            .trim()
            .to_string();
        let _ = std::fs::remove_dir_all(&dir);
        Ok::<_, anyhow::Error>((private, public))
    })
    .await
    .map_err(|e| ApiError::Internal(e.into()))?
    .map_err(ApiError::Internal)
}

async fn derive_public_key(private_key: &str) -> Option<String> {
    let material = private_key.to_string();
    tokio::task::spawn_blocking(move || {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("nook-pub-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).ok()?;
        let key = dir.join("key");
        let mut m = material.trim_end().to_string();
        m.push('\n');
        std::fs::write(&key, m).ok()?;
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).ok()?;
        let out = std::process::Command::new("ssh-keygen")
            .args(["-y", "-f"])
            .arg(&key)
            .output()
            .ok()?;
        let _ = std::fs::remove_dir_all(&dir);
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    })
    .await
    .ok()
    .flatten()
}

// ── Clone onto a node ───────────────────────────────────────────────────────

#[utoipa::path(post, path = "/api/v1/nodes/{id}/clone",
    operation_id = "clone_repo",
    params(("id" = String, Path,)),
    request_body = CloneRequest,
    responses((status = 200, body = OpResponse)))]
pub async fn clone_repo(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(node_id): Path<NodeId>,
    Json(req): Json<CloneRequest>,
) -> ApiResult<Json<OpResponse>> {
    // Cloning mutates a checkout on that machine, so it needs session-start-grade
    // authorization: that node itself, or a person who owns or shares it (MAIN-227).
    auth.require_node_may_use(&state, node_id).await?;
    // Tenant must own the node.
    if !state
        .workspaces
        .node_in_tenant(node_id, auth.tenant_id)
        .await?
    {
        return Err(ApiError::NotFound);
    }

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

    // Every clone carries a job id so a watcher can correlate the finish
    // event with the request that started it.
    let job_id = uuid::Uuid::now_v7().to_string();
    events::record(
        &state,
        auth.tenant_id,
        EventDraft::new("git.clone_started")
            .actor("user", auth.user_id.0)
            .node(node_id)
            .payload(serde_json::json!({ "url": req.url, "job_id": job_id })),
    )
    .await;

    let url = req.url.clone();
    let tenant_slug = crate::services::tenant_slug(&state, auth.tenant_id).await;
    let rx = state
        .registry
        .request_op(node_id, |request_id| ControlToNode::CloneRepo {
            request_id,
            url: req.url.clone(),
            dest_name: req.name.clone(),
            ssh_key,
            tenant_slug,
        })
        .ok_or_else(|| ApiError::BadRequest("node is offline".into()))?;

    // Fire-and-forget: hand the caller a job id and report completion through
    // the activity stream. Cloning a large repo shouldn't hold a request (or a
    // modal) open for minutes.
    if req.background {
        let state = state.clone();
        let tenant = auth.tenant_id;
        let user = auth.user_id.0;
        let job = job_id.clone();
        let url = url.clone();
        tokio::spawn(async move {
            let outcome = tokio::time::timeout(std::time::Duration::from_secs(900), rx).await;
            let (ok, message) = match outcome {
                Ok(Ok(p)) => (p.ok, p.message),
                Ok(Err(_)) => (false, "node disconnected mid-clone".to_string()),
                Err(_) => (false, "clone timed out".to_string()),
            };
            events::record(
                &state,
                tenant,
                EventDraft::new("git.clone_finished")
                    .actor("user", user)
                    .node(node_id)
                    .payload(serde_json::json!({
                        "url": url, "ok": ok, "message": message, "job_id": job
                    })),
            )
            .await;
        });
        return Ok(Json(OpResponse {
            ok: true,
            path: Some(job_id),
            message: "cloning in the background".into(),
        }));
    }

    let payload = match tokio::time::timeout(std::time::Duration::from_secs(90), rx).await {
        Ok(Ok(p)) => p,
        Ok(Err(_)) => {
            return Err(ApiError::BadRequest("node disconnected mid-clone".into()));
        }
        Err(_) => {
            return Ok(Json(OpResponse {
                ok: false,
                path: None,
                message: "clone still running — watch the activity feed".into(),
            }))
        }
    };

    events::record(
        &state,
        auth.tenant_id,
        EventDraft::new("git.clone_finished")
            .actor("user", auth.user_id.0)
            .node(node_id)
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

// ── Worktrees ───────────────────────────────────────────────────────────────

#[utoipa::path(post, path = "/api/v1/workspaces/{id}/worktrees",
    operation_id = "add_worktree",
    params(("id" = String, Path,)),
    request_body = WorktreeRequest,
    responses((status = 200, body = OpResponse)))]
pub async fn add_worktree(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(workspace_id): Path<WorkspaceId>,
    Json(req): Json<WorktreeRequest>,
) -> ApiResult<Json<OpResponse>> {
    // Adding a worktree mutates a checkout on that machine — session-start-grade
    // authorization (that node, or a person who owns/shares it) (MAIN-227).
    auth.require_node_may_use(&state, req.node_id).await?;
    let Some(repo_path) = state
        .workspaces
        .checkout_path(auth.tenant_id, workspace_id, req.node_id)
        .await?
    else {
        return Err(ApiError::NotFound);
    };

    let branch = req.branch.clone();
    let rx = state
        .registry
        .request_op(req.node_id, |request_id| ControlToNode::AddWorktree {
            request_id,
            repo_path,
            branch: req.branch.clone(),
        })
        .ok_or_else(|| ApiError::BadRequest("node is offline".into()))?;

    let payload = tokio::time::timeout(std::time::Duration::from_secs(30), rx)
        .await
        .map_err(|_| ApiError::BadRequest("node did not answer in time".into()))?
        .map_err(|_| ApiError::BadRequest("node disconnected".into()))?;

    events::record(
        &state,
        auth.tenant_id,
        EventDraft::new("workspace.worktree_added")
            .actor("user", auth.user_id.0)
            .workspace(workspace_id)
            .node(req.node_id)
            .payload(serde_json::json!({ "branch": branch, "ok": payload.ok, "message": payload.message })),
    )
    .await;

    Ok(Json(OpResponse {
        ok: payload.ok,
        path: payload.path,
        message: payload.message,
    }))
}

/// Resolve a workspace's checkout path on a node, refusing anything that isn't
/// a checkout this tenant owns.
async fn checkout_path(
    state: &AppState,
    auth: &AuthCtx,
    workspace_id: WorkspaceId,
    node_id: NodeId,
) -> ApiResult<String> {
    state
        .workspaces
        .checkout_path(auth.tenant_id, workspace_id, node_id)
        .await?
        .ok_or(ApiError::NotFound)
}

/// Stage everything and commit, on the machine that holds the checkout.
///
/// Finishing work shouldn't require dropping into a terminal to type the two
/// commands you were always going to type — you just read the diff in the panel
/// above the button.
#[utoipa::path(post, path = "/api/v1/workspaces/{id}/git/commit",
    operation_id = "git_commit",
    params(("id" = String, Path,)),
    request_body = GitCommitRequest,
    responses((status = 200, body = OpResponse), (status = 404)))]
pub async fn git_commit(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(workspace_id): Path<WorkspaceId>,
    Json(req): Json<GitCommitRequest>,
) -> ApiResult<Json<OpResponse>> {
    // Committing mutates a checkout on that machine — person must own/share the
    // node, or be the node itself (MAIN-227).
    auth.require_node_may_use(&state, req.node_id).await?;
    if req.message.trim().is_empty() {
        return Err(ApiError::BadRequest("a commit needs a message".into()));
    }
    let path = checkout_path(&state, &auth, workspace_id, req.node_id).await?;

    let rx = state
        .registry
        .request_op(req.node_id, |request_id| ControlToNode::GitCommit {
            request_id,
            checkout_path: path,
            message: req.message.clone(),
            paths: req.paths.clone(),
        })
        .ok_or_else(|| ApiError::BadRequest("node is offline".into()))?;
    let payload = tokio::time::timeout(std::time::Duration::from_secs(60), rx)
        .await
        .map_err(|_| ApiError::BadRequest("node did not answer in time".into()))?
        .map_err(|_| ApiError::BadRequest("node disconnected".into()))?;

    events::record(
        &state,
        auth.tenant_id,
        EventDraft::new("workspace.committed")
            .actor("user", auth.user_id.0)
            .workspace(workspace_id)
            .node(req.node_id)
            // The message is the user's own words about their work — it belongs
            // in the activity feed. The diff never leaves the machine.
            .payload(serde_json::json!({
                "message": req.message, "ok": payload.ok, "result": payload.message
            })),
    )
    .await;

    Ok(Json(OpResponse {
        ok: payload.ok,
        path: payload.path,
        message: payload.message,
    }))
}

/// Push the checkout's current branch.
#[utoipa::path(post, path = "/api/v1/workspaces/{id}/git/push",
    operation_id = "git_push",
    params(("id" = String, Path,)),
    request_body = GitPushRequest,
    responses((status = 200, body = OpResponse), (status = 404)))]
pub async fn git_push(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(workspace_id): Path<WorkspaceId>,
    Json(req): Json<GitPushRequest>,
) -> ApiResult<Json<OpResponse>> {
    // Pushing acts on a checkout on that machine — person must own/share the node,
    // or be the node itself (MAIN-227).
    auth.require_node_may_use(&state, req.node_id).await?;
    let path = checkout_path(&state, &auth, workspace_id, req.node_id).await?;

    // Same credential story as clone: decrypted here, written 0600 on the node
    // for the length of the push, deleted after.
    //
    // An explicitly named credential wins; otherwise the WORKSPACE's pinned one
    // is used (MAIN-367). Before that fallback, a push from a caller who did not
    // happen to name a key went out with the node's own — so a repo that cloned
    // fine failed on its first push, with an auth error that looked like a
    // different problem entirely.
    let ssh_key = match req.credential_id {
        None => crate::services::workspace_git_key(&state, auth.tenant_id, workspace_id).await,
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

    let rx = state
        .registry
        .request_op(req.node_id, |request_id| ControlToNode::GitPush {
            request_id,
            checkout_path: path,
            ssh_key_material: ssh_key,
        })
        .ok_or_else(|| ApiError::BadRequest("node is offline".into()))?;
    let payload = tokio::time::timeout(std::time::Duration::from_secs(120), rx)
        .await
        .map_err(|_| ApiError::BadRequest("node did not answer in time".into()))?
        .map_err(|_| ApiError::BadRequest("node disconnected".into()))?;

    events::record(
        &state,
        auth.tenant_id,
        EventDraft::new("workspace.pushed")
            .actor("user", auth.user_id.0)
            .workspace(workspace_id)
            .node(req.node_id)
            .payload(serde_json::json!({ "ok": payload.ok, "result": payload.message })),
    )
    .await;

    Ok(Json(OpResponse {
        ok: payload.ok,
        path: payload.path,
        message: payload.message,
    }))
}

#[utoipa::path(post, path = "/api/v1/workspaces/{id}/worktrees/remove",
    operation_id = "remove_worktree",
    params(("id" = String, Path,)),
    request_body = RemoveWorktreeRequest,
    responses((status = 200, body = OpResponse)))]
pub async fn remove_worktree(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(workspace_id): Path<WorkspaceId>,
    Json(req): Json<RemoveWorktreeRequest>,
) -> ApiResult<Json<OpResponse>> {
    // Removing a checkout deletes files on that machine — session-start-grade
    // authorization (own/share the node, or be it) (MAIN-227).
    auth.require_node_may_use(&state, req.node_id).await?;
    // The path must be a known checkout of this workspace on that node.
    if state
        .workspaces
        .checkout_at(auth.tenant_id, workspace_id, req.node_id, &req.path)
        .await?
        .is_none()
    {
        return Err(ApiError::NotFound);
    }

    let path = req.path.clone();
    let rx = state
        .registry
        .request_op(req.node_id, |request_id| ControlToNode::RemoveWorktree {
            request_id,
            worktree_path: req.path.clone(),
            // A human removing a checkout said nothing about its branch.
            delete_branch: false,
        })
        .ok_or_else(|| ApiError::BadRequest("node is offline".into()))?;
    let payload = tokio::time::timeout(std::time::Duration::from_secs(30), rx)
        .await
        .map_err(|_| ApiError::BadRequest("node did not answer in time".into()))?
        .map_err(|_| ApiError::BadRequest("node disconnected".into()))?;

    events::record(
        &state,
        auth.tenant_id,
        EventDraft::new("workspace.worktree_removed")
            .actor("user", auth.user_id.0)
            .workspace(workspace_id)
            .node(req.node_id)
            .payload(serde_json::json!({ "path": path, "ok": payload.ok })),
    )
    .await;

    Ok(Json(OpResponse {
        ok: payload.ok,
        path: payload.path,
        message: payload.message,
    }))
}

// ── New empty project ───────────────────────────────────────────────────────

#[utoipa::path(post, path = "/api/v1/nodes/{id}/projects",
    operation_id = "init_project",
    params(("id" = String, Path,)),
    request_body = InitProjectRequest,
    responses((status = 200, body = OpResponse)))]
pub async fn init_project(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(node_id): Path<NodeId>,
    Json(req): Json<InitProjectRequest>,
) -> ApiResult<Json<OpResponse>> {
    // Initialising a project writes a checkout on that machine — session-start-grade
    // authorization (own/share the node, or be it) (MAIN-227).
    auth.require_node_may_use(&state, node_id).await?;
    if !state
        .workspaces
        .node_in_tenant(node_id, auth.tenant_id)
        .await?
    {
        return Err(ApiError::NotFound);
    }

    let name = req.name.clone();
    let rx = state
        .registry
        .request_op(node_id, |request_id| ControlToNode::InitProject {
            request_id,
            name: req.name.clone(),
        })
        .ok_or_else(|| ApiError::BadRequest("node is offline".into()))?;
    let payload = tokio::time::timeout(std::time::Duration::from_secs(30), rx)
        .await
        .map_err(|_| ApiError::BadRequest("node did not answer in time".into()))?
        .map_err(|_| ApiError::BadRequest("node disconnected".into()))?;

    events::record(
        &state,
        auth.tenant_id,
        EventDraft::new("workspace.project_created")
            .actor("user", auth.user_id.0)
            .node(node_id)
            .payload(serde_json::json!({ "name": name, "ok": payload.ok })),
    )
    .await;

    Ok(Json(OpResponse {
        ok: payload.ok,
        path: payload.path,
        message: payload.message,
    }))
}

// ── Workspace secrets (.env vault) ──────────────────────────────────────────

/// Check a passphrase against the user's app password before it seals or opens
/// anything.
///
/// Two things this buys us. A typo can no longer create a secret that nothing
/// can ever open — every secret is sealed with the one password the user
/// actually has. And a user who has never set one gets `428` rather than a
/// confusing failure, which is the UI's cue to walk them through setting it.
async fn require_app_password(state: &AppState, auth: &AuthCtx, passphrase: &str) -> ApiResult<()> {
    if passphrase.is_empty() {
        return Err(ApiError::SetupRequired(
            "a .env has to be sealed with your app password".into(),
        ));
    }
    let Some(challenge) = state.vaults.app_password_challenge(auth.user_id).await? else {
        return Err(ApiError::SetupRequired(
            "set an app password before storing secrets".into(),
        ));
    };
    let (salt, verifier) = (challenge.kdf_salt, challenge.verifier);
    if !crate::crypto::verify_passphrase(passphrase, &salt, &verifier) {
        return Err(ApiError::Forbidden);
    }
    Ok(())
}

/// Seal content under the app password and store it, replacing whatever was
/// there. Shared by the editor's save and the importer.
async fn store_sealed(
    state: &AppState,
    auth: &AuthCtx,
    workspace_id: WorkspaceId,
    name: &str,
    content: &[u8],
    passphrase: &str,
    ephemeral: bool,
) -> ApiResult<()> {
    // Seal first, then let the app key wrap the already-sealed payload: a
    // database dump plus SECRETS_KEY still reveals nothing.
    let sealed =
        crate::crypto::seal_with_passphrase(content, passphrase).map_err(ApiError::Internal)?;
    let enc = state
        .vault
        .encrypt(&sealed.ciphertext)
        .map_err(ApiError::Internal)?;
    state
        .workspace_secrets
        .store(
            auth.tenant_id,
            workspace_id,
            name,
            enc,
            sealed.salt,
            sealed.verifier,
            ephemeral,
        )
        .await?;
    Ok(())
}

#[utoipa::path(get, path = "/api/v1/workspaces/{id}/secrets",
    operation_id = "list_secrets",
    params(("id" = String, Path,)),
    responses((status = 200, body = [WorkspaceSecret])))]
pub async fn list_secrets(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(workspace_id): Path<WorkspaceId>,
) -> ApiResult<Json<Vec<WorkspaceSecret>>> {
    Ok(Json(
        state
            .workspace_secrets
            .list(auth.tenant_id, workspace_id)
            .await?,
    ))
}

#[utoipa::path(get, path = "/api/v1/workspaces/{id}/secrets/{name}",
    operation_id = "get_secret",
    params(("id" = String, Path,), ("name" = String, Path,)),
    responses((status = 200, body = WorkspaceSecret), (status = 404)))]
pub async fn get_secret(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path((workspace_id, name)): Path<(WorkspaceId, String)>,
) -> ApiResult<Json<WorkspaceSecret>> {
    let stored = state
        .workspace_secrets
        .get(auth.tenant_id, workspace_id, &name)
        .await?
        .ok_or(ApiError::NotFound)?;

    // A GET never returns secret content, sealed or not. The password arrives
    // on the unlock endpoint, which is the only way to read one — including
    // for rows that predate sealing, which unlock re-seals on the way past.
    Ok(Json(WorkspaceSecret {
        name,
        updated_at: stored.updated_at,
        content: None,
        protected: stored.kdf_salt.is_some(),
        ephemeral: stored.ephemeral,
    }))
}

/// Unlock a passphrase-sealed secret. The passphrase is never stored; a wrong
/// one is reported as such rather than as a decryption error.
#[utoipa::path(post, path = "/api/v1/workspaces/{id}/secrets/{name}/open",
    operation_id = "open_secret",
    params(("id" = String, Path,), ("name" = String, Path,)),
    request_body = OpenSecretRequest,
    responses((status = 200, body = WorkspaceSecret), (status = 403), (status = 404)))]
pub async fn open_secret(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path((workspace_id, name)): Path<(WorkspaceId, String)>,
    Json(req): Json<OpenSecretRequest>,
) -> ApiResult<Json<WorkspaceSecret>> {
    let stored = state
        .workspace_secrets
        .get(auth.tenant_id, workspace_id, &name)
        .await?
        .ok_or(ApiError::NotFound)?;
    let (updated_at, ephemeral) = (stored.updated_at, stored.ephemeral);
    let plaintext_at_rest = state
        .vault
        .decrypt(&stored.content_enc)
        .map_err(ApiError::Internal)?;

    let plain = match (stored.kdf_salt, stored.verifier) {
        (Some(salt), Some(verifier)) => crate::crypto::open_with_passphrase(
            &plaintext_at_rest,
            &salt,
            &verifier,
            &req.passphrase,
        )
        .map_err(|_| ApiError::Forbidden)?,
        // A secret from before sealing was mandatory: the app key alone still
        // opens it. Check the password properly, then re-seal it in place, so
        // the next read goes through the same door as everything else.
        _ => {
            require_app_password(&state, &auth, &req.passphrase).await?;
            store_sealed(
                &state,
                &auth,
                workspace_id,
                &name,
                &plaintext_at_rest,
                &req.passphrase,
                ephemeral,
            )
            .await?;
            tracing::info!(name, %workspace_id, "re-sealed a legacy unsealed secret");
            plaintext_at_rest
        }
    };
    // Unlocking is also how a sealed secret reaches the checkouts — the
    // automatic sync can't carry it, because the server can't read it.
    let synced = secrets::push_one(&state, auth.tenant_id, workspace_id, &name, &plain).await;
    tracing::debug!(name, synced, "unlocked secret synced to checkouts");
    Ok(Json(WorkspaceSecret {
        name,
        updated_at,
        content: Some(String::from_utf8_lossy(&plain).to_string()),
        protected: true,
        ephemeral,
    }))
}

#[utoipa::path(put, path = "/api/v1/workspaces/{id}/secrets/{name}",
    operation_id = "put_secret",
    params(("id" = String, Path,), ("name" = String, Path,)),
    request_body = PutSecretRequest,
    responses((status = 200, body = OpResponse)))]
pub async fn put_secret(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path((workspace_id, name)): Path<(WorkspaceId, String)>,
    Json(req): Json<PutSecretRequest>,
) -> ApiResult<Json<OpResponse>> {
    if name.contains('/') || name.contains("..") || name.is_empty() {
        return Err(ApiError::BadRequest("invalid secret file name".into()));
    }
    require_app_password(&state, &auth, &req.passphrase).await?;
    store_sealed(
        &state,
        &auth,
        workspace_id,
        &name,
        req.content.as_bytes(),
        &req.passphrase,
        req.ephemeral,
    )
    .await?;

    // Saving syncs: every online checkout gets the fresh file. The server
    // can't read a sealed secret on its own, so the push rides this request,
    // which is holding the plaintext legitimately. Contents are never logged
    // or recorded in events.
    let pushed = secrets::push_one(
        &state,
        auth.tenant_id,
        workspace_id,
        &name,
        req.content.as_bytes(),
    )
    .await;

    events::record(
        &state,
        auth.tenant_id,
        EventDraft::new("workspace.secret_saved")
            .actor("user", auth.user_id.0)
            .workspace(workspace_id)
            .payload(serde_json::json!({ "name": name, "synced_files": pushed })),
    )
    .await;

    Ok(Json(OpResponse {
        ok: true,
        path: None,
        message: format!("saved · synced {pushed} file(s) to online checkouts"),
    }))
}

/// Does this workspace have a `.env` sitting in a checkout that the vault
/// doesn't know about? Asked right after an import, so we only interrupt
/// someone for their password when there's actually something to seal.
#[utoipa::path(get, path = "/api/v1/workspaces/{id}/secrets/{name}/on-disk",
    operation_id = "secret_on_disk",
    params(("id" = String, Path,), ("name" = String, Path,)),
    responses((status = 200, body = SecretOnDisk)))]
pub async fn secret_on_disk(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path((workspace_id, name)): Path<(WorkspaceId, String)>,
) -> ApiResult<Json<SecretOnDisk>> {
    let in_vault = state
        .workspace_secrets
        .exists(auth.tenant_id, workspace_id, &name)
        .await?;

    let found = read_from_any_checkout(&state, auth.tenant_id, workspace_id, &name)
        .await
        .map(|(path, _)| path);
    Ok(Json(SecretOnDisk {
        found: found.is_some(),
        checkout_path: found,
        in_vault,
    }))
}

/// Read `name` from the first online checkout that has it.
pub(crate) async fn read_from_any_checkout(
    state: &AppState,
    tenant: TenantId,
    workspace: WorkspaceId,
    name: &str,
) -> Option<(String, Vec<u8>)> {
    use base64::Engine;

    let locations = state
        .workspaces
        .checkouts_in_tenant(tenant, workspace)
        .await
        .ok()?;

    for CheckoutRef { node_id, path } in locations {
        if !state.registry.node_online(node_id) {
            continue;
        }
        let rx =
            state
                .registry
                .request_op(node_id, |request_id| ControlToNode::ReadWorkspaceFile {
                    request_id,
                    checkout_path: path.clone(),
                    name: name.to_string(),
                });
        let Some(rx) = rx else { continue };
        let Ok(Ok(payload)) = tokio::time::timeout(std::time::Duration::from_secs(15), rx).await
        else {
            continue;
        };
        if !payload.ok {
            continue; // no such file here — the ordinary case
        }
        if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&payload.message) {
            return Some((path, bytes));
        }
    }
    None
}

/// Adopt a checkout's existing `.env` into the vault, sealed with the app
/// password.
///
/// Importing a repo that already carries a `.env` is the common case, and
/// leaving that file outside the vault meant it never travelled to the user's
/// other machines — and never got encrypted. This is the on-ramp: read it off
/// disk once, seal it, and from then on it syncs like any other secret.
#[utoipa::path(post, path = "/api/v1/workspaces/{id}/secrets/{name}/import",
    operation_id = "import_secret_from_checkout",
    params(("id" = String, Path,), ("name" = String, Path,)),
    request_body = ImportSecretRequest,
    responses((status = 200, body = OpResponse), (status = 403), (status = 428)))]
pub async fn import_secret(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path((workspace_id, name)): Path<(WorkspaceId, String)>,
    Json(req): Json<ImportSecretRequest>,
) -> ApiResult<Json<OpResponse>> {
    if name.contains('/') || name.contains("..") || name.is_empty() {
        return Err(ApiError::BadRequest("invalid secret file name".into()));
    }
    require_app_password(&state, &auth, &req.passphrase).await?;

    let Some((path, content)) =
        read_from_any_checkout(&state, auth.tenant_id, workspace_id, &name).await
    else {
        return Ok(Json(OpResponse {
            ok: false,
            path: None,
            message: format!("no {name} found in any online checkout"),
        }));
    };

    store_sealed(
        &state,
        &auth,
        workspace_id,
        &name,
        &content,
        &req.passphrase,
        req.ephemeral,
    )
    .await?;
    // Now that it's sealed, give every other checkout the same file.
    let pushed = secrets::push_one(&state, auth.tenant_id, workspace_id, &name, &content).await;

    events::record(
        &state,
        auth.tenant_id,
        EventDraft::new("workspace.secret_imported")
            .actor("user", auth.user_id.0)
            .workspace(workspace_id)
            .payload(serde_json::json!({ "name": name, "from": path, "synced_files": pushed })),
    )
    .await;

    Ok(Json(OpResponse {
        ok: true,
        path: Some(path),
        message: format!("imported {name} · sealed and synced to {pushed} checkout(s)"),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The door `create_credential` now refuses at.
    ///
    /// `derive_public_key` returning `None` is the whole condition — before this
    /// it was `unwrap_or_default()`, so an unreadable key stored with
    /// `public_key = ""`. That credential pinned fine and then failed at clone
    /// time as an opaque authentication error, which is the failure mode AC-8
    /// already refuses to create for deletion.
    ///
    /// Asserts only the REFUSAL direction on purpose. The happy path needs a
    /// real `ssh-keygen` on PATH, and a test that silently inverts its meaning
    /// when a binary is missing is worse than no test — this one reads the same
    /// either way.
    #[tokio::test]
    async fn a_key_ssh_keygen_cannot_read_yields_no_public_half() {
        assert!(derive_public_key("not a private key at all")
            .await
            .is_none());
        assert!(derive_public_key("").await.is_none());
        assert!(
            derive_public_key("-----BEGIN OPENSSH PRIVATE KEY-----\ntruncated\n")
                .await
                .is_none(),
            "a well-formed header with a corrupt body must not pass — that is \
             exactly the shape a bad paste takes"
        );
    }
}
