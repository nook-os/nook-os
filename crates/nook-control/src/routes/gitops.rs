//! Git-powerhouse endpoints: tenant credentials (vault), clone-onto-node,
//! worktrees, and workspace secret files.

use axum::extract::{Path, State};
use axum::Json;
use nook_proto::ControlToNode;
use nook_types::*;

use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::events::{self, EventDraft};
use crate::services::secrets;
use crate::state::AppState;

/// Stored secret rows: (name/content, updated_at, kdf_salt, ephemeral) and the
/// unlock variant that also carries the verifier.
type SecretMetaRow = (String, chrono::DateTime<chrono::Utc>, Option<Vec<u8>>, bool);
type SecretRow = (
    Vec<u8>,
    chrono::DateTime<chrono::Utc>,
    Option<Vec<u8>>,
    bool,
);
type SealedSecretRow = (
    Vec<u8>,
    chrono::DateTime<chrono::Utc>,
    Option<Vec<u8>>,
    Option<Vec<u8>>,
    bool,
);

// ── Tenant git credentials ──────────────────────────────────────────────────

#[utoipa::path(get, path = "/api/v1/git-credentials",
    operation_id = "list_git_credentials",
    responses((status = 200, body = [GitCredential])))]
pub async fn list_credentials(
    State(state): State<AppState>,
    auth: AuthCtx,
) -> ApiResult<Json<Vec<GitCredential>>> {
    let creds: Vec<GitCredential> = sqlx::query_as(
        "SELECT id, tenant_id, name, kind, public_key, created_at
         FROM git_credentials WHERE tenant_id = $1 ORDER BY name",
    )
    .bind(auth.tenant_id)
    .fetch_all(&state.db)
    .await?;
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
        // Best effort: derive the public key when possible.
        let public = derive_public_key(&key).await.unwrap_or_default();
        (key, public)
    };

    let enc = state
        .vault
        .encrypt(private_key.as_bytes())
        .map_err(ApiError::Internal)?;

    let cred: GitCredential = sqlx::query_as(
        "INSERT INTO git_credentials (id, tenant_id, name, kind, public_key, secret_enc, created_by)
         VALUES ($1, $2, $3, 'ssh_key', $4, $5, $6)
         RETURNING id, tenant_id, name, kind, public_key, created_at",
    )
    .bind(GitCredentialId::new())
    .bind(auth.tenant_id)
    .bind(&req.name)
    .bind(&public_key)
    .bind(&enc)
    .bind(auth.user_id)
    .fetch_one(&state.db)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(d) if d.is_unique_violation() => {
            ApiError::Conflict("a credential with that name already exists".into())
        }
        _ => e.into(),
    })?;

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
    let res = sqlx::query("DELETE FROM git_credentials WHERE id = $1 AND tenant_id = $2")
        .bind(id)
        .bind(auth.tenant_id)
        .execute(&state.db)
        .await?;
    if res.rows_affected() == 0 {
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
    // Cloning runs git on that machine, with its credentials.
    auth.require_node_self(node_id)?;
    // Tenant must own the node.
    let owned: Option<(NodeId,)> =
        sqlx::query_as("SELECT id FROM nodes WHERE id = $1 AND tenant_id = $2")
            .bind(node_id)
            .bind(auth.tenant_id)
            .fetch_optional(&state.db)
            .await?;
    if owned.is_none() {
        return Err(ApiError::NotFound);
    }

    // Decrypt the chosen tenant credential (if any) for transient node use.
    let ssh_key = match req.credential_id {
        None => None,
        Some(cred_id) => {
            let row: Option<(Vec<u8>,)> = sqlx::query_as(
                "SELECT secret_enc FROM git_credentials WHERE id = $1 AND tenant_id = $2",
            )
            .bind(cred_id)
            .bind(auth.tenant_id)
            .fetch_optional(&state.db)
            .await?;
            let (enc,) = row.ok_or(ApiError::NotFound)?;
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
    let rx = state
        .registry
        .request_op(node_id, |request_id| ControlToNode::CloneRepo {
            request_id,
            url: req.url.clone(),
            dest_name: req.name.clone(),
            ssh_key,
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
    // The worktree is created on the node named in the request.
    auth.require_node_self(req.node_id)?;
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT path FROM node_workspaces
         WHERE tenant_id = $1 AND workspace_id = $2 AND node_id = $3",
    )
    .bind(auth.tenant_id)
    .bind(workspace_id)
    .bind(req.node_id)
    .fetch_optional(&state.db)
    .await?;
    let Some((repo_path,)) = row else {
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
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT path FROM node_workspaces
         WHERE tenant_id = $1 AND workspace_id = $2 AND node_id = $3",
    )
    .bind(auth.tenant_id)
    .bind(workspace_id)
    .bind(node_id)
    .fetch_optional(&state.db)
    .await?;
    row.map(|(p,)| p).ok_or(ApiError::NotFound)
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
    // Committing runs git on that machine.
    auth.require_node_self(req.node_id)?;
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
    // Pushing runs git on that machine, with its credentials.
    auth.require_node_self(req.node_id)?;
    let path = checkout_path(&state, &auth, workspace_id, req.node_id).await?;

    // Same credential story as clone: decrypted here, written 0600 on the node
    // for the length of the push, deleted after.
    let ssh_key = match req.credential_id {
        None => None,
        Some(cred_id) => {
            let row: Option<(Vec<u8>,)> = sqlx::query_as(
                "SELECT secret_enc FROM git_credentials WHERE id = $1 AND tenant_id = $2",
            )
            .bind(cred_id)
            .bind(auth.tenant_id)
            .fetch_optional(&state.db)
            .await?;
            let (enc,) = row.ok_or(ApiError::NotFound)?;
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
    // Removing a checkout deletes files on that machine.
    auth.require_node_self(req.node_id)?;
    // The path must be a known checkout of this workspace on that node.
    let owned: Option<(String,)> = sqlx::query_as(
        "SELECT path FROM node_workspaces
         WHERE tenant_id = $1 AND workspace_id = $2 AND node_id = $3 AND path = $4",
    )
    .bind(auth.tenant_id)
    .bind(workspace_id)
    .bind(req.node_id)
    .bind(&req.path)
    .fetch_optional(&state.db)
    .await?;
    if owned.is_none() {
        return Err(ApiError::NotFound);
    }

    let path = req.path.clone();
    let rx = state
        .registry
        .request_op(req.node_id, |request_id| ControlToNode::RemoveWorktree {
            request_id,
            worktree_path: req.path.clone(),
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
    // Same: this writes to a workspace root on that machine.
    auth.require_node_self(node_id)?;
    let owned: Option<(NodeId,)> =
        sqlx::query_as("SELECT id FROM nodes WHERE id = $1 AND tenant_id = $2")
            .bind(node_id)
            .bind(auth.tenant_id)
            .fetch_optional(&state.db)
            .await?;
    if owned.is_none() {
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
    let row: Option<(Vec<u8>, Vec<u8>)> =
        sqlx::query_as("SELECT kdf_salt, verifier FROM user_vaults WHERE user_id = $1")
            .bind(auth.user_id)
            .fetch_optional(&state.db)
            .await?;
    let Some((salt, verifier)) = row else {
        return Err(ApiError::SetupRequired(
            "set an app password before storing secrets".into(),
        ));
    };
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
    sqlx::query(
        "INSERT INTO workspace_secrets
            (id, tenant_id, workspace_id, name, content_enc, kdf_salt, verifier, ephemeral)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (workspace_id, name)
         DO UPDATE SET content_enc = EXCLUDED.content_enc,
                       kdf_salt = EXCLUDED.kdf_salt,
                       verifier = EXCLUDED.verifier,
                       ephemeral = EXCLUDED.ephemeral,
                       updated_at = now()",
    )
    .bind(nook_types::SettingId::new().0)
    .bind(auth.tenant_id)
    .bind(workspace_id)
    .bind(name)
    .bind(&enc)
    .bind(&sealed.salt)
    .bind(&sealed.verifier)
    .bind(ephemeral)
    .execute(&state.db)
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
    let rows: Vec<SecretMetaRow> = sqlx::query_as(
        "SELECT name, updated_at, kdf_salt, ephemeral FROM workspace_secrets
         WHERE tenant_id = $1 AND workspace_id = $2 ORDER BY name",
    )
    .bind(auth.tenant_id)
    .bind(workspace_id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|(name, updated_at, salt, ephemeral)| WorkspaceSecret {
                name,
                updated_at,
                content: None,
                protected: salt.is_some(),
                ephemeral,
            })
            .collect(),
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
    let row: Option<SecretRow> = sqlx::query_as(
        "SELECT content_enc, updated_at, kdf_salt, ephemeral FROM workspace_secrets
             WHERE tenant_id = $1 AND workspace_id = $2 AND name = $3",
    )
    .bind(auth.tenant_id)
    .bind(workspace_id)
    .bind(&name)
    .fetch_optional(&state.db)
    .await?;
    let (_enc, updated_at, salt, ephemeral) = row.ok_or(ApiError::NotFound)?;

    // A GET never returns secret content, sealed or not. The password arrives
    // on the unlock endpoint, which is the only way to read one — including
    // for rows that predate sealing, which unlock re-seals on the way past.
    Ok(Json(WorkspaceSecret {
        name,
        updated_at,
        content: None,
        protected: salt.is_some(),
        ephemeral,
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
    let row: Option<SealedSecretRow> = sqlx::query_as(
        "SELECT content_enc, updated_at, kdf_salt, verifier, ephemeral
             FROM workspace_secrets
             WHERE tenant_id = $1 AND workspace_id = $2 AND name = $3",
    )
    .bind(auth.tenant_id)
    .bind(workspace_id)
    .bind(&name)
    .fetch_optional(&state.db)
    .await?;
    let (enc, updated_at, salt, verifier, ephemeral) = row.ok_or(ApiError::NotFound)?;
    let stored = state.vault.decrypt(&enc).map_err(ApiError::Internal)?;

    let plain = match (salt, verifier) {
        (Some(salt), Some(verifier)) => {
            crate::crypto::open_with_passphrase(&stored, &salt, &verifier, &req.passphrase)
                .map_err(|_| ApiError::Forbidden)?
        }
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
                &stored,
                &req.passphrase,
                ephemeral,
            )
            .await?;
            tracing::info!(name, %workspace_id, "re-sealed a legacy unsealed secret");
            stored
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
    let vaulted: Option<(String,)> = sqlx::query_as(
        "SELECT name FROM workspace_secrets
         WHERE tenant_id = $1 AND workspace_id = $2 AND name = $3",
    )
    .bind(auth.tenant_id)
    .bind(workspace_id)
    .bind(&name)
    .fetch_optional(&state.db)
    .await?;

    let found = read_from_any_checkout(&state, auth.tenant_id, workspace_id, &name)
        .await
        .map(|(path, _)| path);
    Ok(Json(SecretOnDisk {
        found: found.is_some(),
        checkout_path: found,
        in_vault: vaulted.is_some(),
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

    let locations: Vec<(NodeId, String)> = sqlx::query_as(
        "SELECT node_id, path FROM node_workspaces WHERE tenant_id = $1 AND workspace_id = $2",
    )
    .bind(tenant)
    .bind(workspace)
    .fetch_all(&state.db)
    .await
    .ok()?;

    for (node_id, path) in locations {
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
