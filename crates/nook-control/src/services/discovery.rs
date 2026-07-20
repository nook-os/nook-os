//! Workspace discovery reconciliation.
//!
//! Identity rule (M1, deliberately simple): a repository's identity is its
//! normalized remote URL (host/path, no scheme/credentials/`.git`). Same
//! normalized remote on two nodes ⇒ same workspace. No remote ⇒ fall back to
//! directory-name slug. Never auto-merge two workspaces with different
//! remotes. Git worktrees are out of scope for M1.

use nook_proto::{DiscoveredWorkspace, UiEvent};
use nook_types::*;
use rand::distr::Alphanumeric;
use rand::Rng;

use crate::error::ApiResult;
use crate::events::{self, EventDraft};
use crate::services::identity::slugify;
use crate::state::AppState;

pub fn normalize_remote(url: &str) -> String {
    let mut s = url.trim().to_lowercase();
    // scp-style: git@github.com:org/repo.git → github.com/org/repo
    if let Some(rest) = s.strip_prefix("git@") {
        s = rest.replacen(':', "/", 1);
    }
    for prefix in ["https://", "http://", "ssh://", "git://"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest.to_string();
            break;
        }
    }
    // strip credentials
    if let Some(at) = s.find('@') {
        if s[..at].find('/').is_none() {
            s = s[at + 1..].to_string();
        }
    }
    s.trim_end_matches('/').trim_end_matches(".git").to_string()
}

pub async fn reconcile(
    state: &AppState,
    tenant: TenantId,
    node_id: NodeId,
    discovered: Vec<DiscoveredWorkspace>,
) -> ApiResult<()> {
    let mut reported_paths: Vec<String> = Vec::with_capacity(discovered.len());

    for d in &discovered {
        reported_paths.push(d.path.clone());
        let normalized = d.git_remote_url.as_deref().map(normalize_remote);

        // Find the workspace this checkout belongs to.
        let workspace_id: Option<WorkspaceId> = match &normalized {
            // The remote on the workspace itself is authoritative; the
            // node_workspaces lookup is the fallback for rows written before
            // the identity was recorded there.
            Some(norm) => match sqlx::query_as::<_, (WorkspaceId,)>(
                "SELECT id FROM workspaces
                     WHERE tenant_id = $1 AND git_remote_normalized = $2 LIMIT 1",
            )
            .bind(tenant)
            .bind(norm)
            .fetch_optional(&state.db)
            .await?
            {
                Some((id,)) => Some(id),
                None => sqlx::query_as::<_, (WorkspaceId,)>(
                    "SELECT workspace_id FROM node_workspaces
                         WHERE tenant_id = $1 AND git_remote_normalized = $2 LIMIT 1",
                )
                .bind(tenant)
                .bind(norm)
                .fetch_optional(&state.db)
                .await?
                .map(|(id,)| id),
            },
            None => sqlx::query_as::<_, (WorkspaceId,)>(
                "SELECT id FROM workspaces WHERE tenant_id = $1 AND slug = $2",
            )
            .bind(tenant)
            .bind(slugify(&d.name))
            .fetch_optional(&state.db)
            .await?
            .map(|(id,)| id),
        };

        let workspace_id = match workspace_id {
            Some(id) => {
                // Qualify a bare name once the remote tells us the owner:
                // "services" → "acme/services". Deliberately narrow — only
                // when the discovered name is the same repo with an owner
                // prefix, so a hand-picked name is never clobbered.
                if let Some(norm) = &normalized {
                    sqlx::query(
                        "UPDATE workspaces SET git_remote_normalized = $2
                         WHERE id = $1 AND git_remote_normalized IS DISTINCT FROM $2",
                    )
                    .bind(id)
                    .bind(norm)
                    .execute(&state.db)
                    .await?;
                }
                if let Some((_, repo)) = d.name.split_once('/') {
                    sqlx::query(
                        "UPDATE workspaces SET name = $2, slug = $3, updated_at = now()
                         WHERE id = $1 AND name = $4",
                    )
                    .bind(id)
                    .bind(&d.name)
                    .bind(slugify(&d.name))
                    .bind(repo)
                    .execute(&state.db)
                    .await?;
                }
                id
            }
            None => {
                let id =
                    create_workspace_for(state, tenant, &d.name, normalized.as_deref()).await?;
                let event = events::record(
                    state,
                    tenant,
                    EventDraft::new("workspace.discovered")
                        .actor("node", node_id.0)
                        .workspace(id)
                        .node(node_id)
                        .payload(serde_json::json!({
                            "name": d.name,
                            "path": d.path,
                            "remote": d.git_remote_url,
                        })),
                )
                .await;
                let _ = event;
                id
            }
        };

        let known: Option<(NodeWorkspaceId,)> =
            sqlx::query_as("SELECT id FROM node_workspaces WHERE node_id = $1 AND path = $2")
                .bind(node_id)
                .bind(&d.path)
                .fetch_optional(&state.db)
                .await?;
        let is_new_checkout = known.is_none();

        sqlx::query(
            "INSERT INTO node_workspaces
               (id, tenant_id, node_id, workspace_id, path, git_remote_url,
                git_remote_normalized, git_branch, git_status)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT (node_id, path) DO UPDATE SET
               workspace_id = EXCLUDED.workspace_id,
               git_remote_url = EXCLUDED.git_remote_url,
               git_remote_normalized = EXCLUDED.git_remote_normalized,
               git_branch = EXCLUDED.git_branch,
               git_status = EXCLUDED.git_status,
               last_scanned_at = now()",
        )
        .bind(NodeWorkspaceId::new())
        .bind(tenant)
        .bind(node_id)
        .bind(workspace_id)
        .bind(&d.path)
        .bind(&d.git_remote_url)
        .bind(&normalized)
        .bind(&d.branch)
        .bind(serde_json::json!({ "dirty": d.dirty, "worktree": d.worktree }))
        .execute(&state.db)
        .await?;

        // A new checkout wants the workspace's .env, but the control plane
        // cannot read a sealed secret on its own — that's the whole point. So
        // it announces the checkout instead, and an unlocked browser replays
        // the unlock, which is what actually delivers the file.
        if is_new_checkout {
            crate::services::secrets::announce_new_checkout(
                state,
                tenant,
                workspace_id,
                node_id,
                &d.path,
            )
            .await;
        }
    }

    // Checkouts that disappeared from this node.
    sqlx::query("DELETE FROM node_workspaces WHERE node_id = $1 AND path != ALL($2)")
        .bind(node_id)
        .bind(&reported_paths)
        .execute(&state.db)
        .await?;

    state.registry.publish(
        tenant,
        UiEvent::NodeStatus {
            node_id,
            name: String::new(),
            status: "online".into(),
        },
    );
    Ok(())
}

async fn create_workspace_for(
    state: &AppState,
    tenant: TenantId,
    name: &str,
    git_remote_normalized: Option<&str>,
) -> ApiResult<WorkspaceId> {
    let base_slug = slugify(name);
    for attempt in 0..3 {
        let slug = if attempt == 0 {
            base_slug.clone()
        } else {
            let suffix: String = rand::rng()
                .sample_iter(&Alphanumeric)
                .take(4)
                .map(char::from)
                .collect();
            format!("{base_slug}-{}", suffix.to_lowercase())
        };
        let res: Result<(WorkspaceId,), sqlx::Error> = sqlx::query_as(
            "INSERT INTO workspaces (id, tenant_id, name, slug, git_remote_normalized)
             VALUES ($1, $2, $3, $4, $5) RETURNING id",
        )
        .bind(WorkspaceId::new())
        .bind(tenant)
        .bind(name)
        .bind(&slug)
        .bind(git_remote_normalized)
        .fetch_one(&state.db)
        .await;
        match res {
            Ok((id,)) => return Ok(id),
            Err(sqlx::Error::Database(d)) if d.is_unique_violation() => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(crate::error::ApiError::Conflict(
        "could not allocate workspace slug".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::normalize_remote;

    #[test]
    fn normalizes_equivalent_remotes_to_one_identity() {
        for url in [
            "https://github.com/NookOS/Widgets.git",
            "http://github.com/nookos/widgets",
            "git@github.com:nookos/widgets.git",
            "ssh://github.com/nookos/widgets/",
            "https://user:pass@github.com/nookos/widgets.git",
        ] {
            assert_eq!(normalize_remote(url), "github.com/nookos/widgets", "{url}");
        }
    }

    #[test]
    fn different_repos_stay_different() {
        assert_ne!(
            normalize_remote("https://github.com/a/one.git"),
            normalize_remote("https://github.com/a/two.git"),
        );
    }
}
