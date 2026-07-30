//! Workspace, checkout, git-credential and workspace-secret data access
//! (MAIN-251).
//!
//! A workspace is a repo; a **checkout** (`node_workspaces`) is one clone or
//! worktree of it on one node. The two are one aggregate — a location has no
//! meaning without the workspace it locates — so [`WorkspaceRepository`] owns
//! both tables rather than splitting them across traits that would have to be
//! kept in step. Git credentials and workspace secrets are workspace-scoped
//! satellites with their own lifecycle and their own callers, so they get
//! sibling traits in this file: [`GitCredentialRepository`] and
//! [`WorkspaceSecretRepository`].
//!
//! Two boundary decisions worth stating, because both look like leaks and
//! neither is:
//!
//! - **`migrate_checkout_paths` writes `tasks`.** Renaming a directory has to
//!   move `node_workspaces.path` and `tasks.worktree_path` together or not at
//!   all — they are the only two durable on-disk path records, and a partial
//!   move leaves a task pointing at a directory that no longer exists. That is
//!   one transaction, so it is one method here (AC-1: no cross-repo transaction
//!   semantics). Splitting it across two repositories would mean either two
//!   transactions or a transaction handle in a trait signature; both are worse.
//! - **A few narrow reads answer about `nodes` and `sessions`.**
//!   Those aggregates have no repository yet (MAIN-252/253). They are
//!   named for the workspace-level question they answer, not the table they
//!   read, and each carries a note pointing at the card that will reclaim it.
//!   The chain already works this way — `repo/identity.rs` reads `nodes`,
//!   `repo/tasks.rs` reads `node_workspaces` — because the boundary these cards
//!   draw is the owned file, not the table.
//!
//! Methods are intent-named and coarse; no `sqlx` type appears in any
//! signature, and row mapping lives inside the impl (AC-2).

use async_trait::async_trait;
use nook_db::{params, Db, DbPool, Postgres, TimeMath, TypeMapping};
use nook_types::*;

use crate::error::{ApiError, ApiResult};

/// One checkout's location, as the delete and secret-sync paths need it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutRef {
    pub node_id: NodeId,
    pub path: String,
}

/// A checkout the reaper reclaimed, kept so the reference log can be built
/// after the row is gone.
#[derive(Debug, Clone)]
pub struct ReapedCheckout {
    pub id: NodeWorkspaceId,
    pub node_id: NodeId,
    pub path: String,
}

/// Everything a scan reports about one checkout. One struct rather than ten
/// positional arguments, because the upsert takes all of them at once.
#[derive(Debug, Clone)]
pub struct CheckoutUpsert {
    pub tenant: TenantId,
    pub node_id: NodeId,
    pub workspace_id: WorkspaceId,
    pub path: String,
    pub git_remote_url: Option<String>,
    pub git_remote_normalized: Option<String>,
    pub branch: Option<String>,
    pub git_status: serde_json::Value,
    /// `clone` or `worktree` — the node's report drives this directly
    /// (MAIN-222 AC-1).
    pub kind: String,
}

/// A path rename, both ends.
#[derive(Debug, Clone)]
pub struct PathMove {
    pub old: String,
    pub new: String,
}

/// How many rows each half of a path migration moved.
#[derive(Debug, Clone, Copy, Default)]
pub struct MigratedPaths {
    pub checkouts: u32,
    pub tasks: u32,
}

/// Resolving a user-supplied workspace key. `Ambiguous` carries the slugs to
/// disambiguate with, so the caller can write the error message without going
/// back to the database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyMatch {
    One(WorkspaceId),
    None,
    Ambiguous(Vec<String>),
}

#[async_trait]
pub trait WorkspaceRepository: Send + Sync {
    // ── the workspaces table ────────────────────────────────────────────────

    async fn list(&self, tenant: TenantId) -> ApiResult<Vec<Workspace>>;

    async fn get(&self, tenant: TenantId, id: WorkspaceId) -> ApiResult<Option<Workspace>>;

    /// Create a named workspace. A duplicate name is a `Conflict`, not a
    /// database error — the mapping lives here so every caller reports it the
    /// same way.
    async fn create(
        &self,
        tenant: TenantId,
        name: &str,
        slug: &str,
        description: Option<String>,
    ) -> ApiResult<Workspace>;

    /// Insert a workspace discovered by a scan. `Ok(None)` means the slug was
    /// taken — the caller retries with a fresh one rather than this method
    /// inventing slugs, so slug policy stays in the service.
    async fn insert_discovered(
        &self,
        tenant: TenantId,
        name: &str,
        slug: &str,
        git_remote_normalized: Option<&str>,
    ) -> ApiResult<Option<WorkspaceId>>;

    async fn rename(
        &self,
        id: WorkspaceId,
        tenant: TenantId,
        name: &str,
    ) -> ApiResult<Option<Workspace>>;

    async fn delete(&self, id: WorkspaceId, tenant: TenantId) -> ApiResult<u64>;

    /// The stored remote URL. The outer `Option` is "no such workspace"; the
    /// inner one is "no URL recorded" — the clone path reports those
    /// differently (404 vs a message telling you to clone with a URL once).
    async fn git_remote_url(
        &self,
        id: WorkspaceId,
        tenant: TenantId,
    ) -> ApiResult<Option<Option<String>>>;

    /// Resolve by id, then slug, then name (MAIN-223 AC-3). Both unique keys
    /// are tried before the ambiguous one, so a name that happens to look like
    /// a slug still resolves the same way every time.
    async fn resolve_key(&self, tenant: TenantId, key: &str) -> ApiResult<KeyMatch>;

    async fn find_by_normalized_remote(
        &self,
        tenant: TenantId,
        normalized: &str,
    ) -> ApiResult<Option<WorkspaceId>>;

    /// The fallback identity lookup: a checkout may carry the normalized remote
    /// even when the workspace row predates it being recorded there.
    async fn find_by_checkout_remote(
        &self,
        tenant: TenantId,
        normalized: &str,
    ) -> ApiResult<Option<WorkspaceId>>;

    async fn find_by_slug(&self, tenant: TenantId, slug: &str) -> ApiResult<Option<WorkspaceId>>;

    /// Record the normalized remote, unconditionally (the scan's own identity
    /// is authoritative once it knows it).
    async fn set_normalized_remote(&self, id: WorkspaceId, normalized: &str) -> ApiResult<u64>;

    /// Adopt a normalized remote only when the workspace has none **and** no
    /// other workspace in the tenant already owns it. `workspaces_remote_idx`
    /// is unique per (tenant, normalized), so an unguarded UPDATE would abort
    /// the whole clone on a repo already known under a different workspace.
    async fn adopt_normalized_remote(
        &self,
        id: WorkspaceId,
        tenant: TenantId,
        normalized: &str,
    ) -> ApiResult<u64>;

    /// Adopt a raw remote URL only when the workspace carries none, so an
    /// agreed or hand-set value is never clobbered by one checkout (MAIN-223).
    async fn adopt_remote_url(&self, id: WorkspaceId, url: &str) -> ApiResult<u64>;

    /// Qualify a bare display name once the remote names its owner
    /// ("services" → "acme/services"), and ONLY if the name is still the bare
    /// one — a hand-picked name is never clobbered. The slug is deliberately
    /// untouched (MAIN-220 AC-4).
    async fn qualify_name(
        &self,
        id: WorkspaceId,
        new_name: &str,
        only_if_named: &str,
    ) -> ApiResult<u64>;

    // ── checkouts (node_workspaces) ─────────────────────────────────────────

    /// Every location of a workspace, joined to its node for display.
    async fn locations(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
    ) -> ApiResult<Vec<WorkspaceLocation>>;

    /// A workspace's checkout path on one node — any kind.
    async fn checkout_path(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
        node: NodeId,
    ) -> ApiResult<Option<String>>;

    /// The path of a workspace's **clone** on one node, deterministically:
    /// worktrees branch from the clone, never from another worktree
    /// (MAIN-222 AC-3).
    async fn clone_path(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
        node: NodeId,
    ) -> ApiResult<Option<String>>;

    /// Is `path` a known checkout of this workspace on this node? The guard
    /// that stops a caller naming an arbitrary directory for removal.
    async fn checkout_at(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
        node: NodeId,
        path: &str,
    ) -> ApiResult<Option<String>>;

    async fn checkouts_of(&self, workspace: WorkspaceId) -> ApiResult<Vec<CheckoutRef>>;

    async fn checkouts_in_tenant(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
    ) -> ApiResult<Vec<CheckoutRef>>;

    /// Whether this node already knows a checkout at `path`. The scan uses it
    /// to tell a brand-new checkout (which wants the workspace's `.env`
    /// announced) from one it is merely re-reporting.
    async fn checkout_id_at_path(
        &self,
        node: NodeId,
        path: &str,
    ) -> ApiResult<Option<NodeWorkspaceId>>;

    // ── present-only lookups, for starting a session in one (MAIN-253) ──────
    //
    // A session must never be started in a checkout that has been tombstoned:
    // the directory is gone, so the shell would land nowhere. These are the
    // `missing_at IS NULL` variants of the reads above, kept distinct rather
    // than adding a flag — the guard is the whole point of them.

    /// A named checkout of this workspace on this node, if it is still present.
    async fn present_checkout_path(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
        node: NodeId,
        path: &str,
    ) -> ApiResult<Option<String>>;

    /// The row id for a path on a node, present rows only — what binds a
    /// session to the checkout it actually runs in (MAIN-222 AC-2).
    async fn present_checkout_id_at(
        &self,
        node: NodeId,
        path: &str,
    ) -> ApiResult<Option<NodeWorkspaceId>>;

    /// A checkout's path by row id, if it is still present. `None` means the
    /// session's original checkout has gone.
    async fn present_checkout_path_by_id(&self, id: NodeWorkspaceId) -> ApiResult<Option<String>>;

    /// The workspace's clone on a node, id and path — where a restart falls
    /// back to when the session's own checkout has gone.
    async fn present_clone(
        &self,
        workspace: WorkspaceId,
        node: NodeId,
    ) -> ApiResult<Option<(NodeWorkspaceId, String)>>;

    /// A workspace's display name and normalized remote — what the feedback
    /// surface shows for its configured target (MAIN-256).
    async fn name_and_remote(
        &self,
        id: WorkspaceId,
        tenant: TenantId,
    ) -> ApiResult<Option<(String, Option<String>)>>;

    /// Any node holding a checkout of this workspace — where a feedback
    /// session gets started when there is no live one (MAIN-256).
    async fn any_checkout_node(&self, workspace: WorkspaceId) -> ApiResult<Option<NodeId>>;

    /// A checkout as the UI shows it beside a session (MAIN-222 AC-5).
    async fn checkout_summary(&self, id: NodeWorkspaceId) -> ApiResult<Option<CheckoutSummary>>;

    /// The remote URL and branch of a workspace's checkout on a node — what a
    /// loop job hands its executor so the run knows which repo it is in
    /// (MAIN-255).
    async fn checkout_repo_and_branch(
        &self,
        workspace: WorkspaceId,
        node: NodeId,
    ) -> ApiResult<Option<(Option<String>, Option<String>)>>;

    /// Any checkout of this workspace that records a remote — the fallback when
    /// the executor node holds no usable row of its own (MAIN-255).
    async fn any_checkout_repo_and_branch(
        &self,
        workspace: WorkspaceId,
    ) -> ApiResult<Option<(Option<String>, Option<String>)>>;

    /// Record a scanned checkout, healing a tombstone in place. `ON CONFLICT
    /// (node_id, path)` keeps the row id stable across a re-report.
    async fn upsert_checkout(&self, checkout: CheckoutUpsert) -> ApiResult<()>;

    /// Pin a freshly-cloned checkout to a workspace id, bypassing the
    /// normalized-remote matching a scan would use (MAIN-223 AC-2).
    async fn associate_clone(
        &self,
        tenant: TenantId,
        node: NodeId,
        workspace: WorkspaceId,
        path: &str,
        url: &str,
        normalized: &str,
    ) -> ApiResult<()>;

    /// Tombstone every checkout of this node that the scan did NOT report,
    /// leaving already-missing rows' timestamps alone so the retention clock
    /// does not reset on every scan (MAIN-220).
    async fn tombstone_checkouts_except(
        &self,
        node: NodeId,
        reported_paths: &[String],
    ) -> ApiResult<u64>;

    /// Which of these paths this node actually holds. The belonging check that
    /// runs before any path rewrite.
    async fn existing_paths(&self, node: NodeId, paths: &[String]) -> ApiResult<Vec<String>>;

    /// Rewrite checkout paths **and the task worktree paths that reference
    /// them**, in one transaction, preserving row identity (MAIN-107 AC-4).
    /// Both tables move together or neither does — see the module note.
    async fn migrate_checkout_paths(
        &self,
        node: NodeId,
        moves: &[PathMove],
    ) -> ApiResult<MigratedPaths>;

    /// Hard-delete every checkout tombstoned longer than `retention_secs`,
    /// returning what was reclaimed. Delete-and-return in one statement so a
    /// row healed between a separate select and delete cannot be wrongly
    /// reclaimed.
    async fn reap_tombstoned(&self, retention_secs: i64) -> ApiResult<Vec<ReapedCheckout>>;

    // ── narrow reads of neighbouring aggregates ─────────────────────────────

    /// Does this tenant own that node? MAIN-252 reclaims this.
    async fn node_in_tenant(&self, node: NodeId, tenant: TenantId) -> ApiResult<bool>;

    /// Sessions that would be killed by deleting this workspace, leaving their
    /// tmux orphaned on the node. MAIN-253 reclaims this.
    async fn live_session_count(&self, workspace: WorkspaceId) -> ApiResult<i64>;
}

#[async_trait]
pub trait GitCredentialRepository: Send + Sync {
    async fn list(&self, tenant: TenantId) -> ApiResult<Vec<GitCredential>>;

    /// A duplicate name is a `Conflict`, mapped here so both callers agree.
    async fn create(
        &self,
        tenant: TenantId,
        name: &str,
        public_key: &str,
        secret_enc: Vec<u8>,
        created_by: UserId,
    ) -> ApiResult<GitCredential>;

    async fn delete(&self, id: GitCredentialId, tenant: TenantId) -> ApiResult<u64>;

    /// The encrypted private key, for transient use on a node. Named
    /// `sealed_secret` rather than `secret` because what comes back is still
    /// wrapped by the app vault — the caller must decrypt it.
    async fn sealed_secret(
        &self,
        id: GitCredentialId,
        tenant: TenantId,
    ) -> ApiResult<Option<Vec<u8>>>;
}

/// A stored secret exactly as it sits at rest: still sealed, still wrapped.
#[derive(Debug, Clone)]
pub struct StoredSecret {
    pub content_enc: Vec<u8>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub kdf_salt: Option<Vec<u8>>,
    pub verifier: Option<Vec<u8>>,
    pub ephemeral: bool,
}

#[async_trait]
pub trait WorkspaceSecretRepository: Send + Sync {
    /// Store a sealed secret, replacing whatever was there.
    #[allow(clippy::too_many_arguments)]
    async fn store(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
        name: &str,
        content_enc: Vec<u8>,
        kdf_salt: Vec<u8>,
        verifier: Vec<u8>,
        ephemeral: bool,
    ) -> ApiResult<()>;

    /// Metadata only — never content. The listing is not an unlock.
    async fn list(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
    ) -> ApiResult<Vec<WorkspaceSecret>>;

    /// One secret at rest. Both the metadata read and the unlock path use this;
    /// they differ in what they do with it, not in what they need.
    async fn get(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
        name: &str,
    ) -> ApiResult<Option<StoredSecret>>;

    async fn exists(&self, tenant: TenantId, workspace: WorkspaceId, name: &str)
        -> ApiResult<bool>;
}

// ── the DbPool implementations ──────────────────────────────────────────────

pub struct DbWorkspaceRepository {
    db: DbPool,
}

impl DbWorkspaceRepository {
    pub fn new(db: DbPool) -> Self {
        Self { db }
    }
}

#[async_trait]
impl WorkspaceRepository for DbWorkspaceRepository {
    async fn list(&self, tenant: TenantId) -> ApiResult<Vec<Workspace>> {
        Ok(self
            .db
            .query_all(
                "SELECT * FROM workspaces WHERE tenant_id = $1 ORDER BY name",
                params![tenant],
            )
            .await?)
    }

    async fn get(&self, tenant: TenantId, id: WorkspaceId) -> ApiResult<Option<Workspace>> {
        Ok(self
            .db
            .query_opt(
                "SELECT * FROM workspaces WHERE tenant_id = $1 AND id = $2",
                params![tenant, id],
            )
            .await?)
    }

    async fn create(
        &self,
        tenant: TenantId,
        name: &str,
        slug: &str,
        description: Option<String>,
    ) -> ApiResult<Workspace> {
        self.db
            .query_one(
                "INSERT INTO workspaces (id, tenant_id, name, slug, description)
                 VALUES ($1, $2, $3, $4, $5) RETURNING *",
                params![WorkspaceId::new(), tenant, name, slug, description],
            )
            .await
            .map_err(|e| match &e {
                sqlx::Error::Database(d) if d.is_unique_violation() => {
                    ApiError::Conflict("a workspace with that name already exists".into())
                }
                _ => e.into(),
            })
    }

    async fn insert_discovered(
        &self,
        tenant: TenantId,
        name: &str,
        slug: &str,
        git_remote_normalized: Option<&str>,
    ) -> ApiResult<Option<WorkspaceId>> {
        let res: Result<WorkspaceId, sqlx::Error> = self
            .db
            .query_scalar(
                "INSERT INTO workspaces (id, tenant_id, name, slug, git_remote_normalized)
                 VALUES ($1, $2, $3, $4, $5) RETURNING id",
                params![
                    WorkspaceId::new(),
                    tenant,
                    name,
                    slug,
                    git_remote_normalized
                ],
            )
            .await;
        match res {
            Ok(id) => Ok(Some(id)),
            Err(sqlx::Error::Database(d)) if d.is_unique_violation() => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn rename(
        &self,
        id: WorkspaceId,
        tenant: TenantId,
        name: &str,
    ) -> ApiResult<Option<Workspace>> {
        Ok(self
            .db
            .query_opt(
                &format!(
                    "UPDATE workspaces SET name = $3, updated_at = {}
                     WHERE id = $1 AND tenant_id = $2 RETURNING *",
                    Postgres.now()
                ),
                params![id, tenant, name],
            )
            .await?)
    }

    async fn delete(&self, id: WorkspaceId, tenant: TenantId) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                "DELETE FROM workspaces WHERE id = $1 AND tenant_id = $2",
                params![id, tenant],
            )
            .await?)
    }

    async fn git_remote_url(
        &self,
        id: WorkspaceId,
        tenant: TenantId,
    ) -> ApiResult<Option<Option<String>>> {
        let row: Option<(Option<String>,)> = self
            .db
            .query_opt(
                "SELECT git_remote_url FROM workspaces WHERE id = $1 AND tenant_id = $2",
                params![id, tenant],
            )
            .await?;
        Ok(row.map(|(url,)| url))
    }

    async fn resolve_key(&self, tenant: TenantId, key: &str) -> ApiResult<KeyMatch> {
        // Unique key #1: the id.
        if let Ok(uuid) = key.parse::<uuid::Uuid>() {
            if let Some(id) = self
                .db
                .query_scalar_opt::<WorkspaceId>(
                    "SELECT id FROM workspaces WHERE tenant_id = $1 AND id = $2",
                    params![tenant, WorkspaceId(uuid)],
                )
                .await?
            {
                return Ok(KeyMatch::One(id));
            }
        }
        // Unique key #2: the slug.
        if let Some(id) = self
            .db
            .query_scalar_opt::<WorkspaceId>(
                "SELECT id FROM workspaces WHERE tenant_id = $1 AND slug = $2",
                params![tenant, key],
            )
            .await?
        {
            return Ok(KeyMatch::One(id));
        }
        // Convenience: a bare name, which is NOT unique.
        let matches: Vec<(WorkspaceId, String)> = self
            .db
            .query_all(
                "SELECT id, slug FROM workspaces WHERE tenant_id = $1 AND name = $2 ORDER BY slug",
                params![tenant, key],
            )
            .await?;
        Ok(match matches.as_slice() {
            [(id, _)] => KeyMatch::One(*id),
            [] => KeyMatch::None,
            many => KeyMatch::Ambiguous(many.iter().map(|(_, slug)| slug.clone()).collect()),
        })
    }

    async fn find_by_normalized_remote(
        &self,
        tenant: TenantId,
        normalized: &str,
    ) -> ApiResult<Option<WorkspaceId>> {
        Ok(self
            .db
            .query_scalar_opt::<WorkspaceId>(
                "SELECT id FROM workspaces
                 WHERE tenant_id = $1 AND git_remote_normalized = $2 LIMIT 1",
                params![tenant, normalized],
            )
            .await?)
    }

    async fn find_by_checkout_remote(
        &self,
        tenant: TenantId,
        normalized: &str,
    ) -> ApiResult<Option<WorkspaceId>> {
        Ok(self
            .db
            .query_scalar_opt::<WorkspaceId>(
                "SELECT workspace_id FROM node_workspaces
                 WHERE tenant_id = $1 AND git_remote_normalized = $2 LIMIT 1",
                params![tenant, normalized],
            )
            .await?)
    }

    async fn find_by_slug(&self, tenant: TenantId, slug: &str) -> ApiResult<Option<WorkspaceId>> {
        Ok(self
            .db
            .query_scalar_opt::<WorkspaceId>(
                "SELECT id FROM workspaces WHERE tenant_id = $1 AND slug = $2",
                params![tenant, slug],
            )
            .await?)
    }

    async fn set_normalized_remote(&self, id: WorkspaceId, normalized: &str) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                "UPDATE workspaces SET git_remote_normalized = $2
                 WHERE id = $1 AND git_remote_normalized IS DISTINCT FROM $2",
                params![id, normalized],
            )
            .await?)
    }

    async fn adopt_normalized_remote(
        &self,
        id: WorkspaceId,
        tenant: TenantId,
        normalized: &str,
    ) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                "UPDATE workspaces SET git_remote_normalized = $2
                 WHERE id = $1 AND git_remote_normalized IS NULL
                   AND NOT EXISTS (
                       SELECT 1 FROM workspaces
                       WHERE tenant_id = $3 AND git_remote_normalized = $2
                   )",
                params![id, normalized, tenant],
            )
            .await?)
    }

    async fn adopt_remote_url(&self, id: WorkspaceId, url: &str) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                "UPDATE workspaces SET git_remote_url = $2
                 WHERE id = $1 AND git_remote_url IS NULL",
                params![id, url],
            )
            .await?)
    }

    async fn qualify_name(
        &self,
        id: WorkspaceId,
        new_name: &str,
        only_if_named: &str,
    ) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                &format!(
                    "UPDATE workspaces SET name = $2, updated_at = {}
                     WHERE id = $1 AND name = $3",
                    Postgres.now()
                ),
                params![id, new_name, only_if_named],
            )
            .await?)
    }

    async fn locations(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
    ) -> ApiResult<Vec<WorkspaceLocation>> {
        let rows: Vec<(
            NodeId,
            String,
            String,
            String,
            Option<String>,
            serde_json::Value,
        )> = self
            .db
            .query_all(
                "SELECT n.id, n.name, n.status, nw.path, nw.git_branch, nw.git_status
                 FROM node_workspaces nw
                 JOIN nodes n ON n.id = nw.node_id
                 WHERE nw.tenant_id = $1 AND nw.workspace_id = $2
                 ORDER BY n.name",
                params![tenant, workspace],
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(
                |(node_id, node_name, node_status, path, git_branch, git_status)| {
                    WorkspaceLocation {
                        node_id,
                        node_name,
                        node_status,
                        path,
                        git_branch,
                        dirty: git_status
                            .get("dirty")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false),
                        worktree: git_status
                            .get("worktree")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false),
                    }
                },
            )
            .collect())
    }

    async fn checkout_path(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
        node: NodeId,
    ) -> ApiResult<Option<String>> {
        Ok(self
            .db
            .query_scalar_opt(
                "SELECT path FROM node_workspaces
                 WHERE tenant_id = $1 AND workspace_id = $2 AND node_id = $3",
                params![tenant, workspace, node],
            )
            .await?)
    }

    async fn clone_path(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
        node: NodeId,
    ) -> ApiResult<Option<String>> {
        Ok(self
            .db
            .query_scalar_opt(
                "SELECT path FROM node_workspaces
                 WHERE tenant_id = $1 AND workspace_id = $2 AND node_id = $3
                   AND kind = 'clone' AND missing_at IS NULL
                 ORDER BY discovered_at LIMIT 1",
                params![tenant, workspace, node],
            )
            .await?)
    }

    async fn checkout_at(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
        node: NodeId,
        path: &str,
    ) -> ApiResult<Option<String>> {
        Ok(self
            .db
            .query_scalar_opt(
                "SELECT path FROM node_workspaces
                 WHERE tenant_id = $1 AND workspace_id = $2 AND node_id = $3 AND path = $4",
                params![tenant, workspace, node, path],
            )
            .await?)
    }

    async fn checkouts_of(&self, workspace: WorkspaceId) -> ApiResult<Vec<CheckoutRef>> {
        let rows: Vec<(NodeId, String)> = self
            .db
            .query_all(
                "SELECT node_id, path FROM node_workspaces WHERE workspace_id = $1",
                params![workspace],
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|(node_id, path)| CheckoutRef { node_id, path })
            .collect())
    }

    async fn checkouts_in_tenant(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
    ) -> ApiResult<Vec<CheckoutRef>> {
        let rows: Vec<(NodeId, String)> = self
            .db
            .query_all(
                "SELECT node_id, path FROM node_workspaces
                 WHERE tenant_id = $1 AND workspace_id = $2",
                params![tenant, workspace],
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|(node_id, path)| CheckoutRef { node_id, path })
            .collect())
    }

    async fn checkout_id_at_path(
        &self,
        node: NodeId,
        path: &str,
    ) -> ApiResult<Option<NodeWorkspaceId>> {
        Ok(self
            .db
            .query_scalar_opt(
                "SELECT id FROM node_workspaces WHERE node_id = $1 AND path = $2",
                params![node, path],
            )
            .await?)
    }

    async fn present_checkout_path(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
        node: NodeId,
        path: &str,
    ) -> ApiResult<Option<String>> {
        Ok(self
            .db
            .query_scalar_opt(
                "SELECT path FROM node_workspaces
                 WHERE tenant_id = $1 AND node_id = $2 AND workspace_id = $3 AND path = $4
                   AND missing_at IS NULL",
                params![tenant, node, workspace, path],
            )
            .await?)
    }

    async fn present_checkout_id_at(
        &self,
        node: NodeId,
        path: &str,
    ) -> ApiResult<Option<NodeWorkspaceId>> {
        Ok(self
            .db
            .query_scalar_opt(
                "SELECT id FROM node_workspaces
                 WHERE node_id = $1 AND path = $2 AND missing_at IS NULL",
                params![node, path],
            )
            .await?)
    }

    async fn present_checkout_path_by_id(&self, id: NodeWorkspaceId) -> ApiResult<Option<String>> {
        Ok(self
            .db
            .query_scalar_opt(
                "SELECT path FROM node_workspaces WHERE id = $1 AND missing_at IS NULL",
                params![id],
            )
            .await?)
    }

    async fn present_clone(
        &self,
        workspace: WorkspaceId,
        node: NodeId,
    ) -> ApiResult<Option<(NodeWorkspaceId, String)>> {
        Ok(self
            .db
            .query_opt(
                "SELECT id, path FROM node_workspaces
                 WHERE workspace_id = $1 AND node_id = $2
                   AND kind = 'clone' AND missing_at IS NULL
                 ORDER BY discovered_at LIMIT 1",
                params![workspace, node],
            )
            .await?)
    }

    async fn checkout_repo_and_branch(
        &self,
        workspace: WorkspaceId,
        node: NodeId,
    ) -> ApiResult<Option<(Option<String>, Option<String>)>> {
        Ok(self
            .db
            .query_opt(
                "SELECT git_remote_url, git_branch FROM node_workspaces
                 WHERE workspace_id = $1 AND node_id = $2",
                params![workspace, node],
            )
            .await?)
    }

    async fn any_checkout_repo_and_branch(
        &self,
        workspace: WorkspaceId,
    ) -> ApiResult<Option<(Option<String>, Option<String>)>> {
        Ok(self
            .db
            .query_opt(
                "SELECT git_remote_url, git_branch FROM node_workspaces
                 WHERE workspace_id = $1 AND git_remote_url IS NOT NULL
                 LIMIT 1",
                params![workspace],
            )
            .await?)
    }

    async fn name_and_remote(
        &self,
        id: WorkspaceId,
        tenant: TenantId,
    ) -> ApiResult<Option<(String, Option<String>)>> {
        Ok(self
            .db
            .query_opt(
                "SELECT w.name, w.git_remote_normalized FROM workspaces w
                 WHERE w.id = $1 AND w.tenant_id = $2",
                params![id, tenant],
            )
            .await?)
    }

    async fn any_checkout_node(&self, workspace: WorkspaceId) -> ApiResult<Option<NodeId>> {
        Ok(self
            .db
            .query_scalar_opt(
                "SELECT node_id FROM node_workspaces WHERE workspace_id = $1 LIMIT 1",
                params![workspace],
            )
            .await?)
    }

    async fn checkout_summary(&self, id: NodeWorkspaceId) -> ApiResult<Option<CheckoutSummary>> {
        Ok(self
            .db
            .query_opt(
                "SELECT nw.id, nw.path, nw.git_branch AS branch, nw.kind, n.name AS node_name
                 FROM node_workspaces nw JOIN nodes n ON n.id = nw.node_id
                 WHERE nw.id = $1",
                params![id],
            )
            .await?)
    }

    async fn upsert_checkout(&self, c: CheckoutUpsert) -> ApiResult<()> {
        self.db
            .exec(
                &format!(
                    "INSERT INTO node_workspaces
                       (id, tenant_id, node_id, workspace_id, path, git_remote_url,
                        git_remote_normalized, git_branch, git_status, kind)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
                     ON CONFLICT (node_id, path) DO UPDATE SET
                       workspace_id = EXCLUDED.workspace_id,
                       git_remote_url = EXCLUDED.git_remote_url,
                       git_remote_normalized = EXCLUDED.git_remote_normalized,
                       git_branch = EXCLUDED.git_branch,
                       git_status = EXCLUDED.git_status,
                       kind = EXCLUDED.kind,
                       missing_at = NULL,
                       last_scanned_at = {}",
                    Postgres.now()
                ),
                params![
                    NodeWorkspaceId::new(),
                    c.tenant,
                    c.node_id,
                    c.workspace_id,
                    c.path,
                    c.git_remote_url,
                    c.git_remote_normalized,
                    c.branch,
                    c.git_status,
                    c.kind
                ],
            )
            .await?;
        Ok(())
    }

    async fn associate_clone(
        &self,
        tenant: TenantId,
        node: NodeId,
        workspace: WorkspaceId,
        path: &str,
        url: &str,
        normalized: &str,
    ) -> ApiResult<()> {
        self.db
            .exec(
                &format!(
                    "INSERT INTO node_workspaces
                       (id, tenant_id, node_id, workspace_id, path, git_remote_url,
                        git_remote_normalized, git_status, kind)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, '{{}}'::jsonb, 'clone')
                     ON CONFLICT (node_id, path) DO UPDATE SET
                       workspace_id = EXCLUDED.workspace_id,
                       git_remote_url = EXCLUDED.git_remote_url,
                       git_remote_normalized = EXCLUDED.git_remote_normalized,
                       kind = EXCLUDED.kind,
                       missing_at = NULL,
                       last_scanned_at = {}",
                    Postgres.now()
                ),
                params![
                    NodeWorkspaceId::new(),
                    tenant,
                    node,
                    workspace,
                    path,
                    url,
                    normalized
                ],
            )
            .await?;
        Ok(())
    }

    async fn tombstone_checkouts_except(
        &self,
        node: NodeId,
        reported_paths: &[String],
    ) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                &format!(
                    "UPDATE node_workspaces SET missing_at = {}
                     WHERE node_id = $1 AND path != ALL($2) AND missing_at IS NULL",
                    Postgres.now()
                ),
                params![node, reported_paths.to_vec()],
            )
            .await?)
    }

    async fn existing_paths(&self, node: NodeId, paths: &[String]) -> ApiResult<Vec<String>> {
        Ok(self
            .db
            .query_scalar_all(
                "SELECT path FROM node_workspaces WHERE node_id = $1 AND path = ANY($2)",
                params![node, paths.to_vec()],
            )
            .await?)
    }

    async fn migrate_checkout_paths(
        &self,
        node: NodeId,
        moves: &[PathMove],
    ) -> ApiResult<MigratedPaths> {
        let mut tx = self.db.begin().await?;
        let mut out = MigratedPaths::default();
        for m in moves {
            let checkouts = tx
                .exec(
                    &format!(
                        "UPDATE node_workspaces SET path = $3, last_scanned_at = {}
                         WHERE node_id = $1 AND path = $2",
                        Postgres.now()
                    ),
                    params![node, &m.old, &m.new],
                )
                .await?;
            out.checkouts += checkouts as u32;

            // A task's worktree lives on a specific node; scope the rewrite to
            // this one so an identical relative path on another machine is
            // never touched.
            let tasks = tx
                .exec(
                    &format!(
                        "UPDATE tasks SET worktree_path = $3, updated_at = {}
                         WHERE worktree_node_id = $1 AND worktree_path = $2",
                        Postgres.now()
                    ),
                    params![node, &m.old, &m.new],
                )
                .await?;
            out.tasks += tasks as u32;
        }
        tx.commit().await?;
        Ok(out)
    }

    async fn reap_tombstoned(&self, retention_secs: i64) -> ApiResult<Vec<ReapedCheckout>> {
        let rows: Vec<(NodeWorkspaceId, NodeId, String)> = self
            .db
            .query_all(
                &format!(
                    "DELETE FROM node_workspaces
                     WHERE missing_at IS NOT NULL AND missing_at < {}
                     RETURNING id, node_id, path",
                    Postgres.now_minus_scaled("$1", "1 second")
                ),
                params![retention_secs],
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|(id, node_id, path)| ReapedCheckout { id, node_id, path })
            .collect())
    }

    async fn node_in_tenant(&self, node: NodeId, tenant: TenantId) -> ApiResult<bool> {
        let found: Option<NodeId> = self
            .db
            .query_scalar_opt(
                "SELECT id FROM nodes WHERE id = $1 AND tenant_id = $2",
                params![node, tenant],
            )
            .await?;
        Ok(found.is_some())
    }

    async fn live_session_count(&self, workspace: WorkspaceId) -> ApiResult<i64> {
        Ok(self
            .db
            .query_scalar::<i64>(
                "SELECT count(*) FROM sessions
                 WHERE workspace_id = $1 AND status IN ('starting', 'running', 'detached')",
                params![workspace],
            )
            .await?)
    }
}

pub struct DbGitCredentialRepository {
    db: DbPool,
}

impl DbGitCredentialRepository {
    pub fn new(db: DbPool) -> Self {
        Self { db }
    }
}

#[async_trait]
impl GitCredentialRepository for DbGitCredentialRepository {
    async fn list(&self, tenant: TenantId) -> ApiResult<Vec<GitCredential>> {
        Ok(self
            .db
            .query_all(
                "SELECT id, tenant_id, name, kind, public_key, created_at
                 FROM git_credentials WHERE tenant_id = $1 ORDER BY name",
                params![tenant],
            )
            .await?)
    }

    async fn create(
        &self,
        tenant: TenantId,
        name: &str,
        public_key: &str,
        secret_enc: Vec<u8>,
        created_by: UserId,
    ) -> ApiResult<GitCredential> {
        self.db
            .query_one(
                "INSERT INTO git_credentials
                   (id, tenant_id, name, kind, public_key, secret_enc, created_by)
                 VALUES ($1, $2, $3, 'ssh_key', $4, $5, $6)
                 RETURNING id, tenant_id, name, kind, public_key, created_at",
                params![
                    GitCredentialId::new(),
                    tenant,
                    name,
                    public_key,
                    secret_enc,
                    created_by
                ],
            )
            .await
            .map_err(|e| match &e {
                sqlx::Error::Database(d) if d.is_unique_violation() => {
                    ApiError::Conflict("a credential with that name already exists".into())
                }
                _ => e.into(),
            })
    }

    async fn delete(&self, id: GitCredentialId, tenant: TenantId) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                "DELETE FROM git_credentials WHERE id = $1 AND tenant_id = $2",
                params![id, tenant],
            )
            .await?)
    }

    async fn sealed_secret(
        &self,
        id: GitCredentialId,
        tenant: TenantId,
    ) -> ApiResult<Option<Vec<u8>>> {
        Ok(self
            .db
            .query_scalar_opt(
                "SELECT secret_enc FROM git_credentials WHERE id = $1 AND tenant_id = $2",
                params![id, tenant],
            )
            .await?)
    }
}

pub struct DbWorkspaceSecretRepository {
    db: DbPool,
}

impl DbWorkspaceSecretRepository {
    pub fn new(db: DbPool) -> Self {
        Self { db }
    }
}

#[async_trait]
impl WorkspaceSecretRepository for DbWorkspaceSecretRepository {
    async fn store(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
        name: &str,
        content_enc: Vec<u8>,
        kdf_salt: Vec<u8>,
        verifier: Vec<u8>,
        ephemeral: bool,
    ) -> ApiResult<()> {
        self.db
            .exec(
                &format!(
                    "INSERT INTO workspace_secrets
                       (id, tenant_id, workspace_id, name, content_enc, kdf_salt,
                        verifier, ephemeral)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                     ON CONFLICT (workspace_id, name)
                     DO UPDATE SET content_enc = EXCLUDED.content_enc,
                                   kdf_salt = EXCLUDED.kdf_salt,
                                   verifier = EXCLUDED.verifier,
                                   ephemeral = EXCLUDED.ephemeral,
                                   updated_at = {}",
                    Postgres.now()
                ),
                params![
                    SettingId::new().0,
                    tenant,
                    workspace,
                    name,
                    content_enc,
                    kdf_salt,
                    verifier,
                    ephemeral
                ],
            )
            .await?;
        Ok(())
    }

    async fn list(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
    ) -> ApiResult<Vec<WorkspaceSecret>> {
        let rows: Vec<(String, chrono::DateTime<chrono::Utc>, Option<Vec<u8>>, bool)> = self
            .db
            .query_all(
                "SELECT name, updated_at, kdf_salt, ephemeral FROM workspace_secrets
                 WHERE tenant_id = $1 AND workspace_id = $2 ORDER BY name",
                params![tenant, workspace],
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|(name, updated_at, salt, ephemeral)| WorkspaceSecret {
                name,
                updated_at,
                content: None,
                protected: salt.is_some(),
                ephemeral,
            })
            .collect())
    }

    async fn get(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
        name: &str,
    ) -> ApiResult<Option<StoredSecret>> {
        let row: Option<(
            Vec<u8>,
            chrono::DateTime<chrono::Utc>,
            Option<Vec<u8>>,
            Option<Vec<u8>>,
            bool,
        )> = self
            .db
            .query_opt(
                "SELECT content_enc, updated_at, kdf_salt, verifier, ephemeral
                 FROM workspace_secrets
                 WHERE tenant_id = $1 AND workspace_id = $2 AND name = $3",
                params![tenant, workspace, name],
            )
            .await?;
        Ok(row.map(
            |(content_enc, updated_at, kdf_salt, verifier, ephemeral)| StoredSecret {
                content_enc,
                updated_at,
                kdf_salt,
                verifier,
                ephemeral,
            },
        ))
    }

    async fn exists(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
        name: &str,
    ) -> ApiResult<bool> {
        let found: Option<String> = self
            .db
            .query_scalar_opt(
                "SELECT name FROM workspace_secrets
                 WHERE tenant_id = $1 AND workspace_id = $2 AND name = $3",
                params![tenant, workspace, name],
            )
            .await?;
        Ok(found.is_some())
    }
}

// ── in-memory fakes (AC-3) ──────────────────────────────────────────────────
//
// Enough behavior that a caller test is worth trusting: tenant scoping, the
// NULL-guards that make remote adoption safe, `ON CONFLICT (node_id, path)`
// healing a tombstone in place, and the retention clock. A fake that accepted
// everything would let a caller test pass while the real statement refused.

use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Debug, Clone)]
struct FakeCheckout {
    id: NodeWorkspaceId,
    tenant: TenantId,
    node_id: NodeId,
    workspace_id: WorkspaceId,
    path: String,
    git_remote_url: Option<String>,
    git_remote_normalized: Option<String>,
    branch: Option<String>,
    git_status: serde_json::Value,
    kind: String,
    missing_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Orders `clone_path`, standing in for `discovered_at`.
    seq: u64,
}

#[derive(Default)]
struct FakeState {
    workspaces: Vec<Workspace>,
    checkouts: Vec<FakeCheckout>,
    /// (id, tenant, name, status) — the join `locations` needs.
    nodes: Vec<(NodeId, TenantId, String, String)>,
    live_sessions: HashMap<WorkspaceId, i64>,
    /// (node, worktree_path) pairs, so a path migration can report task moves.
    task_worktrees: Vec<(NodeId, String)>,
    seq: u64,
}

#[derive(Default)]
pub struct FakeWorkspaceRepository {
    inner: Mutex<FakeState>,
}

impl FakeWorkspaceRepository {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a node so `locations` can join to it and `node_in_tenant` can
    /// answer.
    pub fn add_node(&self, id: NodeId, tenant: TenantId, name: &str, status: &str) {
        self.inner
            .lock()
            .unwrap()
            .nodes
            .push((id, tenant, name.to_string(), status.to_string()));
    }

    pub fn set_live_sessions(&self, workspace: WorkspaceId, n: i64) {
        self.inner
            .lock()
            .unwrap()
            .live_sessions
            .insert(workspace, n);
    }

    pub fn add_task_worktree(&self, node: NodeId, path: &str) {
        self.inner
            .lock()
            .unwrap()
            .task_worktrees
            .push((node, path.to_string()));
    }

    /// Backdate a checkout's tombstone so the retention clock can be tested
    /// without waiting.
    pub fn tombstone_since(&self, node: NodeId, path: &str, ago: chrono::Duration) {
        let mut s = self.inner.lock().unwrap();
        if let Some(c) = s
            .checkouts
            .iter_mut()
            .find(|c| c.node_id == node && c.path == path)
        {
            c.missing_at = Some(chrono::Utc::now() - ago);
        }
    }

    pub fn checkout_count(&self) -> usize {
        self.inner.lock().unwrap().checkouts.len()
    }

    /// Is this checkout currently tombstoned?
    pub fn is_tombstoned(&self, node: NodeId, path: &str) -> bool {
        self.inner
            .lock()
            .unwrap()
            .checkouts
            .iter()
            .find(|c| c.node_id == node && c.path == path)
            .is_some_and(|c| c.missing_at.is_some())
    }

    /// The stable row id, so a test can prove a re-report heals in place rather
    /// than inserting a new row.
    pub fn checkout_id(&self, node: NodeId, path: &str) -> Option<NodeWorkspaceId> {
        self.inner
            .lock()
            .unwrap()
            .checkouts
            .iter()
            .find(|c| c.node_id == node && c.path == path)
            .map(|c| c.id)
    }

    fn make(tenant: TenantId, name: &str, slug: &str, description: Option<String>) -> Workspace {
        let now = chrono::Utc::now();
        Workspace {
            id: WorkspaceId::new(),
            tenant_id: tenant,
            name: name.to_string(),
            slug: slug.to_string(),
            description,
            git_remote_url: None,
            git_remote_normalized: None,
            created_at: now,
            updated_at: now,
        }
    }
}

#[async_trait]
impl WorkspaceRepository for FakeWorkspaceRepository {
    async fn list(&self, tenant: TenantId) -> ApiResult<Vec<Workspace>> {
        let s = self.inner.lock().unwrap();
        let mut out: Vec<Workspace> = s
            .workspaces
            .iter()
            .filter(|w| w.tenant_id == tenant)
            .cloned()
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn get(&self, tenant: TenantId, id: WorkspaceId) -> ApiResult<Option<Workspace>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .workspaces
            .iter()
            .find(|w| w.tenant_id == tenant && w.id == id)
            .cloned())
    }

    async fn create(
        &self,
        tenant: TenantId,
        name: &str,
        slug: &str,
        description: Option<String>,
    ) -> ApiResult<Workspace> {
        let mut s = self.inner.lock().unwrap();
        // The constraint is `workspaces_tenant_id_slug_key` — (tenant, SLUG),
        // not name. The message says "name" because the slug is derived from
        // it, but two workspaces may genuinely share a display name, which is
        // exactly why `resolve_key` has an ambiguous case at all.
        if s.workspaces
            .iter()
            .any(|w| w.tenant_id == tenant && w.slug == slug)
        {
            return Err(ApiError::Conflict(
                "a workspace with that name already exists".into(),
            ));
        }
        let w = Self::make(tenant, name, slug, description);
        s.workspaces.push(w.clone());
        Ok(w)
    }

    async fn insert_discovered(
        &self,
        tenant: TenantId,
        name: &str,
        slug: &str,
        git_remote_normalized: Option<&str>,
    ) -> ApiResult<Option<WorkspaceId>> {
        let mut s = self.inner.lock().unwrap();
        // The unique keys a scan can collide on: the slug, and the tenant's
        // normalized remote.
        if s.workspaces.iter().any(|w| {
            w.tenant_id == tenant
                && (w.slug == slug
                    || (git_remote_normalized.is_some()
                        && w.git_remote_normalized.as_deref() == git_remote_normalized))
        }) {
            return Ok(None);
        }
        let mut w = Self::make(tenant, name, slug, None);
        w.git_remote_normalized = git_remote_normalized.map(str::to_string);
        let id = w.id;
        s.workspaces.push(w);
        Ok(Some(id))
    }

    async fn rename(
        &self,
        id: WorkspaceId,
        tenant: TenantId,
        name: &str,
    ) -> ApiResult<Option<Workspace>> {
        let mut s = self.inner.lock().unwrap();
        Ok(s.workspaces
            .iter_mut()
            .find(|w| w.id == id && w.tenant_id == tenant)
            .map(|w| {
                w.name = name.to_string();
                w.updated_at = chrono::Utc::now();
                w.clone()
            }))
    }

    async fn delete(&self, id: WorkspaceId, tenant: TenantId) -> ApiResult<u64> {
        let mut s = self.inner.lock().unwrap();
        let before = s.workspaces.len();
        s.workspaces
            .retain(|w| !(w.id == id && w.tenant_id == tenant));
        let removed = (before - s.workspaces.len()) as u64;
        if removed > 0 {
            // The real schema cascades checkouts.
            s.checkouts.retain(|c| c.workspace_id != id);
        }
        Ok(removed)
    }

    async fn git_remote_url(
        &self,
        id: WorkspaceId,
        tenant: TenantId,
    ) -> ApiResult<Option<Option<String>>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .workspaces
            .iter()
            .find(|w| w.id == id && w.tenant_id == tenant)
            .map(|w| w.git_remote_url.clone()))
    }

    async fn resolve_key(&self, tenant: TenantId, key: &str) -> ApiResult<KeyMatch> {
        let s = self.inner.lock().unwrap();
        let mine = || s.workspaces.iter().filter(|w| w.tenant_id == tenant);
        if let Ok(uuid) = key.parse::<uuid::Uuid>() {
            if let Some(w) = mine().find(|w| w.id == WorkspaceId(uuid)) {
                return Ok(KeyMatch::One(w.id));
            }
        }
        if let Some(w) = mine().find(|w| w.slug == key) {
            return Ok(KeyMatch::One(w.id));
        }
        let mut named: Vec<&Workspace> = mine().filter(|w| w.name == key).collect();
        named.sort_by(|a, b| a.slug.cmp(&b.slug));
        Ok(match named.as_slice() {
            [w] => KeyMatch::One(w.id),
            [] => KeyMatch::None,
            many => KeyMatch::Ambiguous(many.iter().map(|w| w.slug.clone()).collect()),
        })
    }

    async fn find_by_normalized_remote(
        &self,
        tenant: TenantId,
        normalized: &str,
    ) -> ApiResult<Option<WorkspaceId>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .workspaces
            .iter()
            .find(|w| {
                w.tenant_id == tenant && w.git_remote_normalized.as_deref() == Some(normalized)
            })
            .map(|w| w.id))
    }

    async fn find_by_checkout_remote(
        &self,
        tenant: TenantId,
        normalized: &str,
    ) -> ApiResult<Option<WorkspaceId>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .checkouts
            .iter()
            .find(|c| c.tenant == tenant && c.git_remote_normalized.as_deref() == Some(normalized))
            .map(|c| c.workspace_id))
    }

    async fn find_by_slug(&self, tenant: TenantId, slug: &str) -> ApiResult<Option<WorkspaceId>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .workspaces
            .iter()
            .find(|w| w.tenant_id == tenant && w.slug == slug)
            .map(|w| w.id))
    }

    async fn set_normalized_remote(&self, id: WorkspaceId, normalized: &str) -> ApiResult<u64> {
        let mut s = self.inner.lock().unwrap();
        Ok(match s.workspaces.iter_mut().find(|w| w.id == id) {
            Some(w) if w.git_remote_normalized.as_deref() != Some(normalized) => {
                w.git_remote_normalized = Some(normalized.to_string());
                1
            }
            _ => 0,
        })
    }

    async fn adopt_normalized_remote(
        &self,
        id: WorkspaceId,
        tenant: TenantId,
        normalized: &str,
    ) -> ApiResult<u64> {
        let mut s = self.inner.lock().unwrap();
        // The NOT EXISTS guard: another workspace already owning this remote
        // makes the adoption a no-op rather than a unique-violation.
        if s.workspaces.iter().any(|w| {
            w.tenant_id == tenant && w.git_remote_normalized.as_deref() == Some(normalized)
        }) {
            return Ok(0);
        }
        Ok(match s.workspaces.iter_mut().find(|w| w.id == id) {
            Some(w) if w.git_remote_normalized.is_none() => {
                w.git_remote_normalized = Some(normalized.to_string());
                1
            }
            _ => 0,
        })
    }

    async fn adopt_remote_url(&self, id: WorkspaceId, url: &str) -> ApiResult<u64> {
        let mut s = self.inner.lock().unwrap();
        Ok(match s.workspaces.iter_mut().find(|w| w.id == id) {
            Some(w) if w.git_remote_url.is_none() => {
                w.git_remote_url = Some(url.to_string());
                1
            }
            _ => 0,
        })
    }

    async fn qualify_name(
        &self,
        id: WorkspaceId,
        new_name: &str,
        only_if_named: &str,
    ) -> ApiResult<u64> {
        let mut s = self.inner.lock().unwrap();
        Ok(match s.workspaces.iter_mut().find(|w| w.id == id) {
            Some(w) if w.name == only_if_named => {
                w.name = new_name.to_string();
                w.updated_at = chrono::Utc::now();
                1
            }
            _ => 0,
        })
    }

    async fn locations(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
    ) -> ApiResult<Vec<WorkspaceLocation>> {
        let s = self.inner.lock().unwrap();
        let mut out: Vec<WorkspaceLocation> = s
            .checkouts
            .iter()
            .filter(|c| c.tenant == tenant && c.workspace_id == workspace)
            .filter_map(|c| {
                // An INNER JOIN: a checkout whose node is unknown does not
                // appear, which is what the statement does.
                let (_, _, name, status) = s.nodes.iter().find(|(id, ..)| *id == c.node_id)?;
                Some(WorkspaceLocation {
                    node_id: c.node_id,
                    node_name: name.clone(),
                    node_status: status.clone(),
                    path: c.path.clone(),
                    git_branch: c.branch.clone(),
                    dirty: c
                        .git_status
                        .get("dirty")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    worktree: c
                        .git_status
                        .get("worktree")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                })
            })
            .collect();
        out.sort_by(|a, b| a.node_name.cmp(&b.node_name));
        Ok(out)
    }

    async fn checkout_path(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
        node: NodeId,
    ) -> ApiResult<Option<String>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .checkouts
            .iter()
            .find(|c| c.tenant == tenant && c.workspace_id == workspace && c.node_id == node)
            .map(|c| c.path.clone()))
    }

    async fn clone_path(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
        node: NodeId,
    ) -> ApiResult<Option<String>> {
        let s = self.inner.lock().unwrap();
        let mut clones: Vec<&FakeCheckout> = s
            .checkouts
            .iter()
            .filter(|c| {
                c.tenant == tenant
                    && c.workspace_id == workspace
                    && c.node_id == node
                    && c.kind == "clone"
                    && c.missing_at.is_none()
            })
            .collect();
        clones.sort_by_key(|c| c.seq);
        Ok(clones.first().map(|c| c.path.clone()))
    }

    async fn checkout_at(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
        node: NodeId,
        path: &str,
    ) -> ApiResult<Option<String>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .checkouts
            .iter()
            .find(|c| {
                c.tenant == tenant
                    && c.workspace_id == workspace
                    && c.node_id == node
                    && c.path == path
            })
            .map(|c| c.path.clone()))
    }

    async fn checkouts_of(&self, workspace: WorkspaceId) -> ApiResult<Vec<CheckoutRef>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .checkouts
            .iter()
            .filter(|c| c.workspace_id == workspace)
            .map(|c| CheckoutRef {
                node_id: c.node_id,
                path: c.path.clone(),
            })
            .collect())
    }

    async fn checkouts_in_tenant(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
    ) -> ApiResult<Vec<CheckoutRef>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .checkouts
            .iter()
            .filter(|c| c.tenant == tenant && c.workspace_id == workspace)
            .map(|c| CheckoutRef {
                node_id: c.node_id,
                path: c.path.clone(),
            })
            .collect())
    }

    async fn checkout_id_at_path(
        &self,
        node: NodeId,
        path: &str,
    ) -> ApiResult<Option<NodeWorkspaceId>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .checkouts
            .iter()
            .find(|c| c.node_id == node && c.path == path)
            .map(|c| c.id))
    }

    async fn present_checkout_path(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
        node: NodeId,
        path: &str,
    ) -> ApiResult<Option<String>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .checkouts
            .iter()
            .find(|c| {
                c.tenant == tenant
                    && c.workspace_id == workspace
                    && c.node_id == node
                    && c.path == path
                    && c.missing_at.is_none()
            })
            .map(|c| c.path.clone()))
    }

    async fn present_checkout_id_at(
        &self,
        node: NodeId,
        path: &str,
    ) -> ApiResult<Option<NodeWorkspaceId>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .checkouts
            .iter()
            .find(|c| c.node_id == node && c.path == path && c.missing_at.is_none())
            .map(|c| c.id))
    }

    async fn present_checkout_path_by_id(&self, id: NodeWorkspaceId) -> ApiResult<Option<String>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .checkouts
            .iter()
            .find(|c| c.id == id && c.missing_at.is_none())
            .map(|c| c.path.clone()))
    }

    async fn present_clone(
        &self,
        workspace: WorkspaceId,
        node: NodeId,
    ) -> ApiResult<Option<(NodeWorkspaceId, String)>> {
        let s = self.inner.lock().unwrap();
        let mut clones: Vec<&FakeCheckout> = s
            .checkouts
            .iter()
            .filter(|c| {
                c.workspace_id == workspace
                    && c.node_id == node
                    && c.kind == "clone"
                    && c.missing_at.is_none()
            })
            .collect();
        clones.sort_by_key(|c| c.seq);
        Ok(clones.first().map(|c| (c.id, c.path.clone())))
    }

    async fn checkout_repo_and_branch(
        &self,
        workspace: WorkspaceId,
        node: NodeId,
    ) -> ApiResult<Option<(Option<String>, Option<String>)>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .checkouts
            .iter()
            .find(|c| c.workspace_id == workspace && c.node_id == node)
            .map(|c| (c.git_remote_url.clone(), c.branch.clone())))
    }

    async fn any_checkout_repo_and_branch(
        &self,
        workspace: WorkspaceId,
    ) -> ApiResult<Option<(Option<String>, Option<String>)>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .checkouts
            .iter()
            .find(|c| c.workspace_id == workspace && c.git_remote_url.is_some())
            .map(|c| (c.git_remote_url.clone(), c.branch.clone())))
    }

    async fn name_and_remote(
        &self,
        id: WorkspaceId,
        tenant: TenantId,
    ) -> ApiResult<Option<(String, Option<String>)>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .workspaces
            .iter()
            .find(|w| w.id == id && w.tenant_id == tenant)
            .map(|w| (w.name.clone(), w.git_remote_normalized.clone())))
    }

    async fn any_checkout_node(&self, workspace: WorkspaceId) -> ApiResult<Option<NodeId>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .checkouts
            .iter()
            .find(|c| c.workspace_id == workspace)
            .map(|c| c.node_id))
    }

    async fn checkout_summary(&self, id: NodeWorkspaceId) -> ApiResult<Option<CheckoutSummary>> {
        let s = self.inner.lock().unwrap();
        let Some(c) = s.checkouts.iter().find(|c| c.id == id) else {
            return Ok(None);
        };
        // An INNER JOIN to nodes: an unknown node means no row, as in SQL.
        let Some((_, _, node_name, _)) = s.nodes.iter().find(|(nid, ..)| *nid == c.node_id) else {
            return Ok(None);
        };
        Ok(Some(CheckoutSummary {
            id: c.id,
            path: c.path.clone(),
            branch: c.branch.clone(),
            kind: c.kind.clone(),
            node_name: node_name.clone(),
        }))
    }

    async fn upsert_checkout(&self, c: CheckoutUpsert) -> ApiResult<()> {
        let mut s = self.inner.lock().unwrap();
        s.seq += 1;
        let seq = s.seq;
        match s
            .checkouts
            .iter_mut()
            .find(|x| x.node_id == c.node_id && x.path == c.path)
        {
            // ON CONFLICT (node_id, path): heal in place, SAME id.
            Some(x) => {
                x.workspace_id = c.workspace_id;
                x.git_remote_url = c.git_remote_url;
                x.git_remote_normalized = c.git_remote_normalized;
                x.branch = c.branch;
                x.git_status = c.git_status;
                x.kind = c.kind;
                x.missing_at = None;
            }
            None => s.checkouts.push(FakeCheckout {
                id: NodeWorkspaceId::new(),
                tenant: c.tenant,
                node_id: c.node_id,
                workspace_id: c.workspace_id,
                path: c.path,
                git_remote_url: c.git_remote_url,
                git_remote_normalized: c.git_remote_normalized,
                branch: c.branch,
                git_status: c.git_status,
                kind: c.kind,
                missing_at: None,
                seq,
            }),
        }
        Ok(())
    }

    async fn associate_clone(
        &self,
        tenant: TenantId,
        node: NodeId,
        workspace: WorkspaceId,
        path: &str,
        url: &str,
        normalized: &str,
    ) -> ApiResult<()> {
        self.upsert_checkout(CheckoutUpsert {
            tenant,
            node_id: node,
            workspace_id: workspace,
            path: path.to_string(),
            git_remote_url: Some(url.to_string()),
            git_remote_normalized: Some(normalized.to_string()),
            branch: None,
            git_status: serde_json::json!({}),
            kind: "clone".to_string(),
        })
        .await
    }

    async fn tombstone_checkouts_except(
        &self,
        node: NodeId,
        reported_paths: &[String],
    ) -> ApiResult<u64> {
        let mut s = self.inner.lock().unwrap();
        let now = chrono::Utc::now();
        let mut n = 0;
        for c in s.checkouts.iter_mut() {
            // `missing_at IS NULL` guard: an already-tombstoned row keeps its
            // original timestamp, so the retention clock does not reset.
            if c.node_id == node && !reported_paths.contains(&c.path) && c.missing_at.is_none() {
                c.missing_at = Some(now);
                n += 1;
            }
        }
        Ok(n)
    }

    async fn existing_paths(&self, node: NodeId, paths: &[String]) -> ApiResult<Vec<String>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .checkouts
            .iter()
            .filter(|c| c.node_id == node && paths.contains(&c.path))
            .map(|c| c.path.clone())
            .collect())
    }

    async fn migrate_checkout_paths(
        &self,
        node: NodeId,
        moves: &[PathMove],
    ) -> ApiResult<MigratedPaths> {
        let mut s = self.inner.lock().unwrap();
        let mut out = MigratedPaths::default();
        for m in moves {
            for c in s.checkouts.iter_mut() {
                if c.node_id == node && c.path == m.old {
                    c.path = m.new.clone();
                    out.checkouts += 1;
                }
            }
            for (n, p) in s.task_worktrees.iter_mut() {
                if *n == node && *p == m.old {
                    *p = m.new.clone();
                    out.tasks += 1;
                }
            }
        }
        Ok(out)
    }

    async fn reap_tombstoned(&self, retention_secs: i64) -> ApiResult<Vec<ReapedCheckout>> {
        let mut s = self.inner.lock().unwrap();
        let cutoff = chrono::Utc::now() - chrono::Duration::seconds(retention_secs);
        let doomed: Vec<ReapedCheckout> = s
            .checkouts
            .iter()
            .filter(|c| c.missing_at.is_some_and(|m| m < cutoff))
            .map(|c| ReapedCheckout {
                id: c.id,
                node_id: c.node_id,
                path: c.path.clone(),
            })
            .collect();
        s.checkouts
            .retain(|c| !c.missing_at.is_some_and(|m| m < cutoff));
        Ok(doomed)
    }

    async fn node_in_tenant(&self, node: NodeId, tenant: TenantId) -> ApiResult<bool> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .nodes
            .iter()
            .any(|(id, t, ..)| *id == node && *t == tenant))
    }

    async fn live_session_count(&self, workspace: WorkspaceId) -> ApiResult<i64> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .live_sessions
            .get(&workspace)
            .copied()
            .unwrap_or(0))
    }
}

#[derive(Default)]
pub struct FakeGitCredentialRepository {
    inner: Mutex<Vec<(GitCredential, Vec<u8>)>>,
}

impl FakeGitCredentialRepository {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl GitCredentialRepository for FakeGitCredentialRepository {
    async fn list(&self, tenant: TenantId) -> ApiResult<Vec<GitCredential>> {
        let s = self.inner.lock().unwrap();
        let mut out: Vec<GitCredential> = s
            .iter()
            .filter(|(c, _)| c.tenant_id == tenant)
            .map(|(c, _)| c.clone())
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn create(
        &self,
        tenant: TenantId,
        name: &str,
        public_key: &str,
        secret_enc: Vec<u8>,
        _created_by: UserId,
    ) -> ApiResult<GitCredential> {
        let mut s = self.inner.lock().unwrap();
        if s.iter()
            .any(|(c, _)| c.tenant_id == tenant && c.name == name)
        {
            return Err(ApiError::Conflict(
                "a credential with that name already exists".into(),
            ));
        }
        let cred = GitCredential {
            id: GitCredentialId::new(),
            tenant_id: tenant,
            name: name.to_string(),
            kind: "ssh_key".to_string(),
            public_key: public_key.to_string(),
            created_at: chrono::Utc::now(),
        };
        s.push((cred.clone(), secret_enc));
        Ok(cred)
    }

    async fn delete(&self, id: GitCredentialId, tenant: TenantId) -> ApiResult<u64> {
        let mut s = self.inner.lock().unwrap();
        let before = s.len();
        s.retain(|(c, _)| !(c.id == id && c.tenant_id == tenant));
        Ok((before - s.len()) as u64)
    }

    async fn sealed_secret(
        &self,
        id: GitCredentialId,
        tenant: TenantId,
    ) -> ApiResult<Option<Vec<u8>>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .iter()
            .find(|(c, _)| c.id == id && c.tenant_id == tenant)
            .map(|(_, enc)| enc.clone()))
    }
}

#[derive(Default)]
struct FakeSecretState {
    secrets: Vec<(TenantId, WorkspaceId, String, StoredSecret)>,
}

#[derive(Default)]
pub struct FakeWorkspaceSecretRepository {
    inner: Mutex<FakeSecretState>,
}

impl FakeWorkspaceSecretRepository {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl WorkspaceSecretRepository for FakeWorkspaceSecretRepository {
    async fn store(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
        name: &str,
        content_enc: Vec<u8>,
        kdf_salt: Vec<u8>,
        verifier: Vec<u8>,
        ephemeral: bool,
    ) -> ApiResult<()> {
        let mut s = self.inner.lock().unwrap();
        let row = StoredSecret {
            content_enc,
            updated_at: chrono::Utc::now(),
            kdf_salt: Some(kdf_salt),
            verifier: Some(verifier),
            ephemeral,
        };
        // ON CONFLICT (workspace_id, name): replace in place.
        match s
            .secrets
            .iter_mut()
            .find(|(_, w, n, _)| *w == workspace && n == name)
        {
            Some(existing) => existing.3 = row,
            None => s.secrets.push((tenant, workspace, name.to_string(), row)),
        }
        Ok(())
    }

    async fn list(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
    ) -> ApiResult<Vec<WorkspaceSecret>> {
        let s = self.inner.lock().unwrap();
        let mut out: Vec<WorkspaceSecret> = s
            .secrets
            .iter()
            .filter(|(t, w, _, _)| *t == tenant && *w == workspace)
            .map(|(_, _, name, row)| WorkspaceSecret {
                name: name.clone(),
                updated_at: row.updated_at,
                content: None,
                protected: row.kdf_salt.is_some(),
                ephemeral: row.ephemeral,
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn get(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
        name: &str,
    ) -> ApiResult<Option<StoredSecret>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .secrets
            .iter()
            .find(|(t, w, n, _)| *t == tenant && *w == workspace && n == name)
            .map(|(_, _, _, row)| row.clone()))
    }

    async fn exists(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
        name: &str,
    ) -> ApiResult<bool> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .secrets
            .iter()
            .any(|(t, w, n, _)| *t == tenant && *w == workspace && n == name))
    }
}
