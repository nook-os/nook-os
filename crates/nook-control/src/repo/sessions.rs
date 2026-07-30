//! Session data access (MAIN-253).
//!
//! A session is a tmux-backed terminal running a chosen runtime in one
//! checkout. [`SessionRepository`] owns the `sessions` table: creation, the
//! status transitions the socket and the attach path drive, and the two list
//! shapes the UI needs.
//!
//! What it deliberately does **not** own is `node_workspaces`. Several session
//! operations have to resolve a checkout first — which path to start in, which
//! row to bind to, what to show in the UI — but that is the workspace
//! aggregate's data and MAIN-251 already gave it a repository. Those reads went
//! onto [`crate::repo::workspaces::WorkspaceRepository`] instead of being
//! copied here, so a change to how a checkout is found has one home rather than
//! two. The one that already existed — "the clone on this node,
//! deterministically" — is reused as-is.
//!
//! Methods are intent-named and coarse; no `sqlx` type appears in any
//! signature, and row mapping lives inside the impl (AC-2).

use async_trait::async_trait;
use nook_db::{params, CiMatch, Db, DbPool, Postgres, TypeMapping};
use nook_types::*;

use crate::error::ApiResult;

/// A session to create. One struct for all three entry points — a worktree
/// session, an ad-hoc terminal, a runtime login — because they differ only in
/// whether a workspace and checkout are bound, never in how the row is written.
#[derive(Debug, Clone)]
pub struct NewSession {
    pub tenant: TenantId,
    /// `None` for an ad-hoc terminal or a login flow: those run in the node's
    /// home directory and belong to no repo.
    pub workspace_id: Option<WorkspaceId>,
    pub node_id: NodeId,
    pub name: String,
    pub runtime: String,
    pub created_by: Option<UserId>,
    /// The exact checkout row the working directory is (MAIN-222 AC-2), so a
    /// restart can return to it. `None` when there is no row yet — discovery
    /// may not have scanned a just-made worktree — or for an ad-hoc `$HOME`.
    pub checkout_id: Option<NodeWorkspaceId>,
}

/// Which sessions a list call wants.
#[derive(Debug, Clone, Default)]
pub struct SessionFilter {
    pub workspace: Option<WorkspaceId>,
    /// `Some(user)` is a member's own view (MAIN-133). It naturally excludes
    /// `created_by NULL` (legacy/MCP) rows, because `NULL = user` is never
    /// true; `None` is the owner/admin metadata view.
    pub creator: Option<UserId>,
    pub active_only: bool,
}

#[async_trait]
pub trait SessionRepository: Send + Sync {
    /// List a tenant's sessions. This is the metadata/list layer only —
    /// content access stays with `session_guard`.
    async fn list(&self, tenant: TenantId, filter: SessionFilter) -> ApiResult<Vec<Session>>;

    /// One session by id, scoped to its tenant.
    async fn get(&self, tenant: TenantId, id: SessionId) -> ApiResult<Option<Session>>;

    /// One session by id, **unscoped**. The content paths (attach, output) need
    /// the row before they know whose it is; they authorize on what comes back.
    /// Named to make that visible at every call site.
    async fn by_id_unscoped(&self, id: SessionId) -> ApiResult<Option<Session>>;

    async fn create(&self, new: NewSession) -> ApiResult<Session>;

    async fn rename(
        &self,
        id: SessionId,
        tenant: TenantId,
        name: &str,
    ) -> ApiResult<Option<Session>>;

    /// The node never took the start instruction — the session is stillborn.
    async fn mark_failed_to_start(&self, id: SessionId) -> ApiResult<u64>;

    /// Clear the previous run and set it starting again.
    async fn mark_restarting(&self, id: SessionId) -> ApiResult<Session>;

    /// Re-bind a session to a checkout row — the restart path, when the
    /// original checkout is gone and it lands in the clone instead.
    async fn bind_checkout(&self, id: SessionId, checkout: NodeWorkspaceId) -> ApiResult<u64>;

    async fn status_of(&self, id: SessionId) -> ApiResult<Option<String>>;

    /// The standing feedback session for a workspace: the one named exactly
    /// `name`, else any live session whose name merely mentions feedback.
    /// Someone who calls a session "…Feedback…" has said plainly where
    /// feedback should go, and starting a second agent beside it is both
    /// wasteful and invisible (MAIN-256).
    async fn feedback_session(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
        name: &str,
    ) -> ApiResult<Option<(SessionId, NodeId)>>;

    /// A session's tmux name, or `None` while the node has yet to report it.
    async fn tmux_of(&self, id: SessionId) -> ApiResult<Option<String>>;

    /// Set a status, but only from a live one. The guard is what stops a late
    /// socket event resurrecting a session that already exited or errored.
    async fn mark_status_if_live(&self, id: SessionId, status: &str) -> ApiResult<u64>;

    async fn delete(&self, id: SessionId, tenant: TenantId) -> ApiResult<u64>;
}

// ── the DbPool implementation ───────────────────────────────────────────────

pub struct DbSessionRepository {
    db: DbPool,
}

impl DbSessionRepository {
    pub fn new(db: DbPool) -> Self {
        Self { db }
    }
}

#[async_trait]
impl SessionRepository for DbSessionRepository {
    async fn list(&self, tenant: TenantId, filter: SessionFilter) -> ApiResult<Vec<Session>> {
        // The filter is composed here rather than by the caller: placeholder
        // numbering and bind order have to agree, and that pairing is exactly
        // the thing that should not be spread across call sites.
        let mut sql = String::from("SELECT * FROM sessions WHERE tenant_id = $1");
        let mut n = 1;
        if filter.workspace.is_some() {
            n += 1;
            sql.push_str(&format!(" AND workspace_id = ${n}"));
        }
        if filter.creator.is_some() {
            n += 1;
            sql.push_str(&format!(" AND created_by = ${n}"));
        }
        if filter.active_only {
            sql.push_str(" AND status IN ('starting', 'running', 'detached')");
        }
        sql.push_str(" ORDER BY created_at DESC");
        // Binds follow the same order the placeholders were numbered above.
        let mut binds = params![tenant];
        if let Some(w) = filter.workspace {
            binds.extend(params![w]);
        }
        if let Some(c) = filter.creator {
            binds.extend(params![c]);
        }
        Ok(self.db.query_all::<Session>(&sql, binds).await?)
    }

    async fn get(&self, tenant: TenantId, id: SessionId) -> ApiResult<Option<Session>> {
        Ok(self
            .db
            .query_opt(
                "SELECT * FROM sessions WHERE id = $1 AND tenant_id = $2",
                params![id, tenant],
            )
            .await?)
    }

    async fn by_id_unscoped(&self, id: SessionId) -> ApiResult<Option<Session>> {
        Ok(self
            .db
            .query_opt("SELECT * FROM sessions WHERE id = $1", params![id])
            .await?)
    }

    async fn create(&self, new: NewSession) -> ApiResult<Session> {
        Ok(self
            .db
            .query_one(
                "INSERT INTO sessions
                   (id, tenant_id, workspace_id, node_id, name, runtime, status,
                    created_by, checkout_id)
                 VALUES ($1, $2, $3, $4, $5, $6, 'starting', $7, $8) RETURNING *",
                params![
                    SessionId::new(),
                    new.tenant,
                    new.workspace_id.map(|w| w.0),
                    new.node_id,
                    new.name,
                    new.runtime,
                    new.created_by.map(|u| u.0),
                    new.checkout_id.map(|c| c.0)
                ],
            )
            .await?)
    }

    async fn rename(
        &self,
        id: SessionId,
        tenant: TenantId,
        name: &str,
    ) -> ApiResult<Option<Session>> {
        Ok(self
            .db
            .query_opt(
                &format!(
                    "UPDATE sessions SET name = $3, updated_at = {}
                     WHERE id = $1 AND tenant_id = $2 RETURNING *",
                    Postgres.now()
                ),
                params![id, tenant, name],
            )
            .await?)
    }

    async fn mark_failed_to_start(&self, id: SessionId) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                &format!(
                    "UPDATE sessions SET status = 'error', updated_at = {} WHERE id = $1",
                    Postgres.now()
                ),
                params![id],
            )
            .await?)
    }

    async fn mark_restarting(&self, id: SessionId) -> ApiResult<Session> {
        Ok(self
            .db
            .query_one(
                &format!(
                    "UPDATE sessions SET status = 'starting', error = NULL, ended_at = NULL,
                        updated_at = {}
                     WHERE id = $1 RETURNING *",
                    Postgres.now()
                ),
                params![id],
            )
            .await?)
    }

    async fn bind_checkout(&self, id: SessionId, checkout: NodeWorkspaceId) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                "UPDATE sessions SET checkout_id = $2 WHERE id = $1",
                params![id, checkout],
            )
            .await?)
    }

    async fn feedback_session(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
        name: &str,
    ) -> ApiResult<Option<(SessionId, NodeId)>> {
        Ok(self
            .db
            .query_opt(
                &format!(
                    "SELECT id, node_id FROM sessions
                     WHERE tenant_id = $1 AND workspace_id = $2
                       AND (name = $3 OR {})
                       AND status IN ('starting', 'running', 'detached')
                     ORDER BY (name = $3) DESC, created_at DESC LIMIT 1",
                    Postgres.ci_match("name", "'%feedback%'")
                ),
                params![tenant, workspace, name],
            )
            .await?)
    }

    async fn tmux_of(&self, id: SessionId) -> ApiResult<Option<String>> {
        let row: Option<(Option<String>,)> = self
            .db
            .query_opt(
                "SELECT tmux_session FROM sessions WHERE id = $1",
                params![id],
            )
            .await?;
        Ok(row.and_then(|(t,)| t))
    }

    async fn status_of(&self, id: SessionId) -> ApiResult<Option<String>> {
        Ok(self
            .db
            .query_scalar_opt("SELECT status FROM sessions WHERE id = $1", params![id])
            .await?)
    }

    async fn mark_status_if_live(&self, id: SessionId, status: &str) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                &format!(
                    "UPDATE sessions SET status = $2, updated_at = {now}
                     WHERE id = $1 AND status IN ('starting', 'running', 'detached')",
                    now = Postgres.now()
                ),
                params![id, status],
            )
            .await?)
    }

    async fn delete(&self, id: SessionId, tenant: TenantId) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                "DELETE FROM sessions WHERE id = $1 AND tenant_id = $2",
                params![id, tenant],
            )
            .await?)
    }
}

// ── the in-memory fake (AC-3) ───────────────────────────────────────────────
//
// Enough behavior that a caller test is worth trusting: tenant scoping, the
// creator scope that hides a teammate's terminals, and the "only from a live
// status" guard that stops a late socket event resurrecting a dead session.

use std::sync::Mutex;

#[derive(Default)]
pub struct FakeSessionRepository {
    inner: Mutex<Vec<Session>>,
}

impl FakeSessionRepository {
    pub fn new() -> Self {
        Self::default()
    }

    /// Force a status directly, bypassing the live-status guard — so a test can
    /// set up the very state the guard exists to protect.
    pub fn force_status(&self, id: SessionId, status: &str) {
        let mut s = self.inner.lock().unwrap();
        if let Some(x) = s.iter_mut().find(|x| x.id == id) {
            x.status = status.to_string();
        }
    }

    pub fn status_snapshot(&self, id: SessionId) -> Option<String> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .find(|x| x.id == id)
            .map(|x| x.status.clone())
    }

    pub fn count(&self) -> usize {
        self.inner.lock().unwrap().len()
    }
}

#[async_trait]
impl SessionRepository for FakeSessionRepository {
    async fn list(&self, tenant: TenantId, filter: SessionFilter) -> ApiResult<Vec<Session>> {
        let s = self.inner.lock().unwrap();
        let mut out: Vec<Session> = s
            .iter()
            .filter(|x| x.tenant_id == tenant)
            .filter(|x| match filter.workspace {
                None => true,
                Some(w) => x.workspace_id == Some(w),
            })
            // `created_by = $n` — a NULL creator never matches, which is what
            // keeps legacy/MCP sessions out of a member's own view.
            .filter(|x| match filter.creator {
                None => true,
                Some(c) => x.created_by == Some(c),
            })
            .filter(|x| {
                !filter.active_only
                    || matches!(x.status.as_str(), "starting" | "running" | "detached")
            })
            .cloned()
            .collect();
        out.sort_by_key(|s| std::cmp::Reverse(s.created_at));
        Ok(out)
    }

    async fn get(&self, tenant: TenantId, id: SessionId) -> ApiResult<Option<Session>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .iter()
            .find(|x| x.id == id && x.tenant_id == tenant)
            .cloned())
    }

    async fn by_id_unscoped(&self, id: SessionId) -> ApiResult<Option<Session>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .iter()
            .find(|x| x.id == id)
            .cloned())
    }

    async fn create(&self, new: NewSession) -> ApiResult<Session> {
        let now = chrono::Utc::now();
        let session = Session {
            id: SessionId::new(),
            tenant_id: new.tenant,
            workspace_id: new.workspace_id,
            node_id: new.node_id,
            name: new.name,
            runtime: new.runtime,
            tmux_session: None,
            status: "starting".into(),
            error: None,
            created_by: new.created_by,
            created_at: now,
            updated_at: now,
            ended_at: None,
            checkout_id: new.checkout_id,
            checkout: None,
        };
        self.inner.lock().unwrap().push(session.clone());
        Ok(session)
    }

    async fn rename(
        &self,
        id: SessionId,
        tenant: TenantId,
        name: &str,
    ) -> ApiResult<Option<Session>> {
        let mut s = self.inner.lock().unwrap();
        Ok(s.iter_mut()
            .find(|x| x.id == id && x.tenant_id == tenant)
            .map(|x| {
                x.name = name.to_string();
                x.updated_at = chrono::Utc::now();
                x.clone()
            }))
    }

    async fn mark_failed_to_start(&self, id: SessionId) -> ApiResult<u64> {
        let mut s = self.inner.lock().unwrap();
        Ok(match s.iter_mut().find(|x| x.id == id) {
            Some(x) => {
                x.status = "error".into();
                1
            }
            None => 0,
        })
    }

    async fn mark_restarting(&self, id: SessionId) -> ApiResult<Session> {
        let mut s = self.inner.lock().unwrap();
        let x = s
            .iter_mut()
            .find(|x| x.id == id)
            .ok_or(crate::error::ApiError::NotFound)?;
        x.status = "starting".into();
        x.error = None;
        x.ended_at = None;
        x.updated_at = chrono::Utc::now();
        Ok(x.clone())
    }

    async fn bind_checkout(&self, id: SessionId, checkout: NodeWorkspaceId) -> ApiResult<u64> {
        let mut s = self.inner.lock().unwrap();
        Ok(match s.iter_mut().find(|x| x.id == id) {
            Some(x) => {
                x.checkout_id = Some(checkout);
                1
            }
            None => 0,
        })
    }

    async fn feedback_session(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
        name: &str,
    ) -> ApiResult<Option<(SessionId, NodeId)>> {
        let s = self.inner.lock().unwrap();
        let mut live: Vec<&Session> = s
            .iter()
            .filter(|x| {
                x.tenant_id == tenant
                    && x.workspace_id == Some(workspace)
                    && matches!(x.status.as_str(), "starting" | "running" | "detached")
                    && (x.name == name || x.name.to_lowercase().contains("feedback"))
            })
            .collect();
        // `ORDER BY (name = $3) DESC, created_at DESC`: the exact name wins,
        // then the newest.
        live.sort_by_key(|x| (x.name != name, std::cmp::Reverse(x.created_at)));
        Ok(live.first().map(|x| (x.id, x.node_id)))
    }

    async fn tmux_of(&self, id: SessionId) -> ApiResult<Option<String>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .iter()
            .find(|x| x.id == id)
            .and_then(|x| x.tmux_session.clone()))
    }

    async fn status_of(&self, id: SessionId) -> ApiResult<Option<String>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .iter()
            .find(|x| x.id == id)
            .map(|x| x.status.clone()))
    }

    async fn mark_status_if_live(&self, id: SessionId, status: &str) -> ApiResult<u64> {
        let mut s = self.inner.lock().unwrap();
        Ok(match s.iter_mut().find(|x| x.id == id) {
            // The live-status guard: a session that already exited or errored
            // is final, whatever a late socket event says.
            Some(x) if matches!(x.status.as_str(), "starting" | "running" | "detached") => {
                x.status = status.to_string();
                x.updated_at = chrono::Utc::now();
                1
            }
            _ => 0,
        })
    }

    async fn delete(&self, id: SessionId, tenant: TenantId) -> ApiResult<u64> {
        let mut s = self.inner.lock().unwrap();
        let before = s.len();
        s.retain(|x| !(x.id == id && x.tenant_id == tenant));
        Ok((before - s.len()) as u64)
    }
}
