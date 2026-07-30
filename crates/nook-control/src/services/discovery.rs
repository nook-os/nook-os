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
use crate::repo::workspaces::{CheckoutUpsert, PathMove, WorkspaceRepository};
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
            // checkout lookup is the fallback for rows written before the
            // identity was recorded there.
            Some(norm) => match state
                .workspaces
                .find_by_normalized_remote(tenant, norm)
                .await?
            {
                Some(id) => Some(id),
                None => {
                    state
                        .workspaces
                        .find_by_checkout_remote(tenant, norm)
                        .await?
                }
            },
            None => {
                state
                    .workspaces
                    .find_by_slug(tenant, &slugify(&d.name))
                    .await?
            }
        };

        let workspace_id = match workspace_id {
            Some(id) => {
                // Qualify a bare name once the remote tells us the owner:
                // "services" → "acme/services". Deliberately narrow — only
                // when the discovered name is the same repo with an owner
                // prefix, so a hand-picked name is never clobbered.
                if let Some(norm) = &normalized {
                    state.workspaces.set_normalized_remote(id, norm).await?;
                }
                if let Some((_, repo)) = d.name.split_once('/') {
                    // MAIN-220 AC-4: relabel the display name only — the slug is
                    // stable after creation. Rewriting the slug on a bare-name
                    // match broke every client holding the old slug mid-scan, and
                    // could collide with the unique-slug constraint and abort the
                    // whole reconcile. The name may still be qualified in place.
                    state.workspaces.qualify_name(id, &d.name, repo).await?;
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

        // Keep the workspace's own repo identity fresh (MAIN-223): adopt this
        // checkout's raw URL when the workspace carries none yet. NULL-guarded, so
        // an agreed or hand-set value is never clobbered by one checkout.
        if let Some(raw) = &d.git_remote_url {
            state.workspaces.adopt_remote_url(workspace_id, raw).await?;
        }

        let known = state
            .workspaces
            .checkout_id_at_path(node_id, &d.path)
            .await?;
        let is_new_checkout = known.is_none();

        state
            .workspaces
            .upsert_checkout(CheckoutUpsert {
                tenant,
                node_id,
                workspace_id,
                path: d.path.clone(),
                git_remote_url: d.git_remote_url.clone(),
                git_remote_normalized: normalized.clone(),
                branch: d.branch.clone(),
                git_status: serde_json::json!({ "dirty": d.dirty, "worktree": d.worktree }),
                // MAIN-222 AC-1: the node's report drives `kind` directly. The
                // jsonb flag stays for compat but nothing new reads it.
                kind: if d.worktree { "worktree" } else { "clone" }.to_string(),
            })
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

    // Checkouts that disappeared from this node are TOMBSTONED, never deleted
    // (MAIN-220). Mark newly-missing rows with the moment they went missing, and
    // leave already-missing rows' timestamps alone (`missing_at IS NULL` guard)
    // so the retention clock does not reset on every scan. A later re-report
    // heals the row above (missing_at = NULL, SAME id); hard deletion is the
    // retention sweep's job.
    if reported_paths.is_empty() {
        // An empty report — an unmounted root, or a scan that panicked into
        // `unwrap_or_default()` — vacuously matches every row. Marking instead
        // of deleting makes that recoverable, but it is still worth shouting
        // about: zero checkouts is almost always a scan fault, not reality.
        tracing::warn!(
            %node_id,
            "node reported zero checkouts — marking all of its checkouts missing (not deleting)"
        );
    }
    state
        .workspaces
        .tombstone_checkouts_except(node_id, &reported_paths)
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

/// Coordinated path rewrite for `nook migrate-workspaces` (MAIN-107 AC-4).
///
/// The counterpart to [`reconcile`]: where reconcile is destructive by path —
/// a checkout that stops being reported is DELETED and its new location is a
/// brand-new row — this rewrites the two durable path records in place, in one
/// transaction, preserving row identity. That is the whole point of a migration
/// that renames directories: no checkout should look new (which would announce
/// a fresh `.env` delivery), and no task's `worktree_path` should go stale.
///
/// Every write is pinned to `node_id`, so a caller can only ever move rows that
/// belong to the node it is acting as. And an `old` path that is not currently
/// a checkout of this node is refused BEFORE any write — a mistake, or an
/// attempt to name another node's paths, aborts the whole request rather than
/// silently updating nothing.
pub async fn migrate_paths(
    repo: &dyn WorkspaceRepository,
    node_id: NodeId,
    pairs: &[nook_types::MigratePathPair],
) -> ApiResult<nook_types::MigratePathsResponse> {
    // Nothing to do is success (the CLI's already-migrated node sends none).
    if pairs.is_empty() {
        return Ok(nook_types::MigratePathsResponse {
            node_workspaces_updated: 0,
            tasks_updated: 0,
        });
    }

    // Belonging check, before any write.
    let olds: Vec<String> = pairs.iter().map(|p| p.old.clone()).collect();
    let mine: std::collections::HashSet<String> = repo
        .existing_paths(node_id, &olds)
        .await?
        .into_iter()
        .collect();
    if let Some(stray) = olds.iter().find(|p| !mine.contains(*p)) {
        return Err(crate::error::ApiError::BadRequest(format!(
            "{stray} is not a checkout of this node — refusing to rewrite paths \
             that do not belong to it"
        )));
    }

    // One transaction inside the repository: every path record moves together
    // or none does. `path` on node_workspaces and `worktree_path` on tasks are
    // the only two durable on-disk path records (0001_init). Row ids are never
    // touched.
    let moves: Vec<PathMove> = pairs
        .iter()
        .map(|p| PathMove {
            old: p.old.clone(),
            new: p.new.clone(),
        })
        .collect();
    let moved = repo.migrate_checkout_paths(node_id, &moves).await?;

    Ok(nook_types::MigratePathsResponse {
        node_workspaces_updated: moved.checkouts,
        tasks_updated: moved.tasks,
    })
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
        if let Some(id) = state
            .workspaces
            .insert_discovered(tenant, name, &slug, git_remote_normalized)
            .await?
        {
            return Ok(id);
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
