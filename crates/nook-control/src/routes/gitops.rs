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

    events::record(
        &state,
        auth.tenant_id,
        EventDraft::new("git.clone_started")
            .actor("user", auth.user_id.0)
            .node(node_id)
            .payload(serde_json::json!({ "url": req.url })),
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
            .payload(
                serde_json::json!({ "url": url, "ok": payload.ok, "message": payload.message }),
            ),
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

#[utoipa::path(get, path = "/api/v1/workspaces/{id}/secrets",
    operation_id = "list_secrets",
    params(("id" = String, Path,)),
    responses((status = 200, body = [WorkspaceSecret])))]
pub async fn list_secrets(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(workspace_id): Path<WorkspaceId>,
) -> ApiResult<Json<Vec<WorkspaceSecret>>> {
    let rows: Vec<(String, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT name, updated_at FROM workspace_secrets
         WHERE tenant_id = $1 AND workspace_id = $2 ORDER BY name",
    )
    .bind(auth.tenant_id)
    .bind(workspace_id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|(name, updated_at)| WorkspaceSecret {
                name,
                updated_at,
                content: None,
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
    let row: Option<(Vec<u8>, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT content_enc, updated_at FROM workspace_secrets
         WHERE tenant_id = $1 AND workspace_id = $2 AND name = $3",
    )
    .bind(auth.tenant_id)
    .bind(workspace_id)
    .bind(&name)
    .fetch_optional(&state.db)
    .await?;
    let (enc, updated_at) = row.ok_or(ApiError::NotFound)?;
    let content = state
        .vault
        .decrypt_string(&enc)
        .map_err(ApiError::Internal)?;
    Ok(Json(WorkspaceSecret {
        name,
        updated_at,
        content: Some(content),
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
    let enc = state
        .vault
        .encrypt(req.content.as_bytes())
        .map_err(ApiError::Internal)?;
    sqlx::query(
        "INSERT INTO workspace_secrets (id, tenant_id, workspace_id, name, content_enc)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (workspace_id, name)
         DO UPDATE SET content_enc = EXCLUDED.content_enc, updated_at = now()",
    )
    .bind(nook_types::SettingId::new().0)
    .bind(auth.tenant_id)
    .bind(workspace_id)
    .bind(&name)
    .bind(&enc)
    .execute(&state.db)
    .await?;

    // Saving syncs: every online checkout gets the fresh file. Contents are
    // never logged or recorded in events.
    let pushed = secrets::push_everywhere(&state, auth.tenant_id, workspace_id).await?;

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
