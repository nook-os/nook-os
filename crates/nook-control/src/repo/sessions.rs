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
use nook_db::dialect::{ci_match, time_math, type_mapping};
use nook_db::{params, Db, DbPool};
use nook_types::*;

use crate::error::ApiResult;
use crate::session_status;

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
    /// Reconciler-owned (MAIN-316). Set on the INSERT, not by a follow-up
    /// update, so the unique index arbitrates the multi-replica race BEFORE
    /// anything is sent to a node. Marking afterwards meant the losing replica
    /// had already told the node to start: a live, unattributed session the
    /// reconciler could never see again.
    pub managed: bool,
    /// What the reconciler wants this session FOR (MAIN-326). On the INSERT for
    /// the same reason `managed` is: it is half the unique index that decides
    /// the race, so setting it afterwards would let two declarations each think
    /// they had won the checkout.
    pub managed_purpose: ManagedPurpose,
    /// Which slice of the declaration this session owns (MAIN-446). On the
    /// INSERT for the reason `managed_purpose` is: it is part of the unique
    /// index, so it is what lets a second reviewer take the same clone rather
    /// than be refused as the first one's duplicate.
    pub managed_shard: i32,
    /// The divisor of `managed_shard`. Carried on the row so a restart re-sends
    /// the same partition; not part of the index, because two sessions with one
    /// index and two divisors are one slot described twice.
    pub managed_shards: i32,
    /// Terminal or chat (MAIN-502), fixed at creation. On the INSERT because
    /// there is no moment at which the row exists and this is unknown: the
    /// start instruction that goes to the node carries it, and a session that
    /// had to be updated into being a chat would have already started a tmux.
    pub interface: SessionInterface,
}

/// One managed session, as the reconciler sees it.
///
/// A named row rather than a tuple because the shard pair (MAIN-446) took it to
/// five fields, and `.3`/`.4` at the call site is exactly how an index and its
/// divisor get swapped.
#[derive(Debug, Clone, nook_db::FromDbRow)]
pub struct ManagedSession {
    pub id: SessionId,
    pub checkout_id: NodeWorkspaceId,
    pub node_id: NodeId,
    pub managed_shard: i32,
    pub managed_shards: i32,
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

    /// Move a session between the two VIEWER-PRESENCE states, and only those:
    /// `detached` ⇄ `running` as the last viewer leaves and the first arrives.
    ///
    /// The narrowness is the point. This used to be a general "set any status
    /// from any live one", which let a viewer opening a tab promote a session
    /// out of `starting` — including one whose node never reported
    /// `SessionStarted`, so the row read `running` with a NULL `tmux_session`
    /// and the UI offered a terminal that could never attach. Leaving
    /// `starting` is the NODE's statement about a process; a browser tab is not
    /// evidence that anything started.
    async fn mark_viewer_presence(&self, id: SessionId, watched: bool) -> ApiResult<u64>;

    async fn delete(&self, id: SessionId, tenant: TenantId) -> ApiResult<u64>;

    /// Tenants holding at least one reapable session, so the sweep only asks
    /// about tenants that have work. Derived from the sessions themselves
    /// rather than from a tenant list: the reaper's business is rows, and a
    /// tenant with nothing to reclaim is a setting lookup nobody needed.
    async fn tenants_with_terminated(&self) -> ApiResult<Vec<TenantId>>;

    /// Hard-delete this tenant's `exited`/`error` sessions that ended more than
    /// `retention_days` ago. Returns how many went.
    ///
    /// `detached` is NOT terminated — tmux still holds it and a browser can
    /// reattach — so it is never matched here.
    async fn reap_terminated(&self, tenant: TenantId, retention_days: i64) -> ApiResult<u64>;

    /// The LIVE managed sessions of one workspace, as `(session, checkout, node)`.
    ///
    /// Keyed on the CHECKOUT now, not the node: a node holding a clone plus
    /// worktrees runs one managed session per checkout, so the reconciler counts
    /// checkouts. The node rides along because stopping one means killing the
    /// process on that machine. Managed rows always carry a checkout_id, and a
    /// stray null one is excluded — it could never be matched to a checkout slot.
    ///
    /// Live only: an exited managed session is a gap the reconciler fills, not
    /// a replica it already has. Managed only: this is the query that makes
    /// "ad-hoc sessions are never touched" true rather than intended.
    /// The live managed sessions of one workspace; `Some(purpose)` narrows to
    /// one declaration, `None` takes every one of them (MAIN-326).
    ///
    /// Narrowing happens in SQL rather than in the caller because the planner's
    /// `actual` set is also its stop list: a declaration that could see another
    /// purpose's sessions would stop them as strays it has no slot for. The
    /// unfiltered form is for callers asking "is anything here reconciler-owned"
    /// — workspace deletion, which must not mistake a review loop for a session
    /// a person started and refuse forever.
    async fn live_managed(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
        purpose: Option<ManagedPurpose>,
    ) -> ApiResult<Vec<ManagedSession>>;

    /// End a managed session — the scale-down half of converging.
    async fn mark_ended(&self, tenant: TenantId, id: SessionId) -> ApiResult<u64>;

    /// Stop a live session: the row stays, the tmux goes, the ports come back.
    ///
    /// Guarded on LIVE so it is a no-op on a session that already ended, and so
    /// a second Stop does not rewrite `ended_at`.
    async fn mark_stopped(&self, tenant: TenantId, id: SessionId) -> ApiResult<u64>;

    /// How many OTHER live sessions share this workspace (MAIN-292, moved from
    /// `services::secrets`). Live = `starting`/`running`/`detached`. It is what
    /// says whether an ending session was the last user of the checkout's
    /// ephemeral secret files, so wiping them cannot pull the file out from
    /// under a sibling still running.
    async fn live_siblings(&self, workspace: WorkspaceId, excluding: SessionId) -> ApiResult<i64>;
    // ── port leases (MAIN-301) ──────────────────────────────────────────────

    /// Drop every lease held by a session on this node that is no longer live,
    /// and answer with the ports still held.
    ///
    /// One call, because the two halves are the same question asked at the same
    /// instant: reclaim is LAZY (AC-4), so "which ports are taken" is only
    /// answerable after the dead ones have gone. Nothing releases a lease when
    /// a session ends, is killed or is reaped — a dead session's ports come
    /// back here, at the moment somebody needs one.
    async fn reclaim_and_held_ports(&self, node: NodeId) -> ApiResult<Vec<i32>>;

    /// Record one lease. `false` means another session took that port between
    /// the read and this write — the unique index is the arbiter, so the caller
    /// picks again rather than double-leasing.
    async fn add_lease(&self, lease: NewPortLease) -> ApiResult<bool>;

    /// Every port a session holds, in requirement-name order.
    async fn leases_of(&self, session: SessionId) -> ApiResult<Vec<LeasedPort>>;

    /// Every live lease on a node, lowest port first — what the UI lists.
    async fn leases_on(&self, node: NodeId) -> ApiResult<Vec<PortLease>>;

    /// Hand a session's ports back without ending it (AC-6): the escape hatch
    /// for a lease a human can see is stuck.
    ///
    /// Scoped to the NODE as well as the tenant (MAIN-301 review): the caller
    /// was authorized as that node's owner, so letting the session id alone
    /// decide would have let one machine's owner free a port on another's.
    async fn release_leases(&self, node: NodeId, id: SessionId) -> ApiResult<u64>;

    // ── chat messages (MAIN-502) ────────────────────────────────────────────

    /// A chat session's whole conversation, oldest first.
    ///
    /// Unpaged deliberately, for now: a session's history is what a reader
    /// opens the page to see, and `ChatView` already owns the scrolling.
    /// Paging is what MAIN-502's NG-4 defers along with history GC — the two
    /// are the same question about how big a conversation is allowed to get.
    async fn messages(&self, session: SessionId) -> ApiResult<Vec<SessionMessage>>;

    /// Append one line and hand back the row that was written.
    ///
    /// Returning the row rather than the id is what lets the REST call answer
    /// with the message it created — the client shows what the server stored,
    /// not its own optimistic copy of it.
    async fn append_message(&self, new: NewSessionMessage) -> ApiResult<SessionMessage>;

    /// Answer an outstanding permission request, and say whether anything was
    /// still outstanding to answer.
    ///
    /// Guarded on `decision IS NULL`, so the SECOND answer — the other device,
    /// the double-click, the reload that re-posts — writes nothing and reports
    /// `false`. The caller uses that to decide whether to bother the node,
    /// which is what stops one request being answered twice with two different
    /// verdicts.
    async fn decide_permission(
        &self,
        session: SessionId,
        request_id: &str,
        allow: bool,
    ) -> ApiResult<bool>;
}

/// One line to append to a chat session's conversation.
#[derive(Debug, Clone)]
pub struct NewSessionMessage {
    pub session: SessionId,
    /// `human` | `agent` | `system` | `permission`.
    pub role: String,
    pub body: String,
    /// Set together on a `permission` row and on no other: the id an answer is
    /// addressed to, and the tool being asked about.
    pub permission_request_id: Option<String>,
    pub tool_name: Option<String>,
}

impl NewSessionMessage {
    /// An ordinary line — from a person, the agent, or the node itself.
    pub fn line(session: SessionId, role: &str, body: impl Into<String>) -> Self {
        Self {
            session,
            role: role.to_string(),
            body: body.into(),
            permission_request_id: None,
            tool_name: None,
        }
    }
}

/// A lease to record.
#[derive(Debug, Clone)]
pub struct NewPortLease {
    pub session: SessionId,
    pub node: NodeId,
    pub name: String,
    pub env: String,
    pub port: i32,
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
            sql.push_str(&format!(" AND status IN ({})", session_status::LIVE_SQL));
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
                    created_by, checkout_id, managed, managed_purpose,
                    managed_shard, managed_shards, interface)
                 VALUES ($1, $2, $3, $4, $5, $6, 'starting', $7, $8, $9, $10, $11, $12, $13)
                 RETURNING *",
                params![
                    SessionId::new(),
                    new.tenant,
                    new.workspace_id.map(|w| w.0),
                    new.node_id,
                    new.name,
                    new.runtime,
                    new.created_by.map(|u| u.0),
                    new.checkout_id.map(|c| c.0),
                    new.managed,
                    new.managed_purpose,
                    new.managed_shard,
                    new.managed_shards,
                    new.interface
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
                    type_mapping(self.db.engine()).now()
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
                    type_mapping(self.db.engine()).now()
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
                    type_mapping(self.db.engine()).now()
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
                       AND (name = $3 OR {ci})
                       AND status IN ({live})
                     ORDER BY (name = $3) DESC, created_at DESC LIMIT 1",
                    live = session_status::LIVE_SQL,
                    ci = ci_match(self.db.engine()).ci_match("name", "'%feedback%'")
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

    async fn mark_viewer_presence(&self, id: SessionId, watched: bool) -> ApiResult<u64> {
        // Two static statements rather than one with a bound status: the source
        // state is half the guard, so it belongs in the SQL, not in a parameter.
        let sql = if watched {
            "UPDATE sessions SET status = 'running', updated_at = {now}
             WHERE id = $1 AND status = 'detached'"
        } else {
            "UPDATE sessions SET status = 'detached', updated_at = {now}
             WHERE id = $1 AND status = 'running'"
        };
        Ok(self
            .db
            .exec(
                &sql.replace("{now}", type_mapping(self.db.engine()).now()),
                params![id],
            )
            .await?)
    }

    async fn tenants_with_terminated(&self) -> ApiResult<Vec<TenantId>> {
        Ok(self
            .db
            .query_all::<(TenantId,)>(
                "SELECT DISTINCT tenant_id FROM sessions
                 WHERE status IN ('exited', 'error') AND ended_at IS NOT NULL",
                params![],
            )
            .await?
            .into_iter()
            .map(|(t,)| t)
            .collect())
    }

    async fn reap_terminated(&self, tenant: TenantId, retention_days: i64) -> ApiResult<u64> {
        // `ended_at IS NOT NULL` is load-bearing, not defensive: a row that
        // reached a terminal status without one has no age, and comparing NULL
        // would silently never match — better to leave it and have it show up
        // than to invent a timestamp for it.
        self.db
            .exec(
                &format!(
                    "DELETE FROM sessions
                     WHERE tenant_id = $1
                       AND status IN ('exited', 'error')
                       AND ended_at IS NOT NULL
                       AND ended_at < {}",
                    time_math(self.db.engine()).now_minus_scaled("$2", "1 day")
                ),
                params![tenant, retention_days],
            )
            .await
            .map_err(Into::into)
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

    async fn live_managed(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
        purpose: Option<ManagedPurpose>,
    ) -> ApiResult<Vec<ManagedSession>> {
        // `$3 IS NULL OR …` rather than two query strings: one shape to read,
        // and the planner's filter cannot drift from the unfiltered count.
        Ok(self
            .db
            .query_all(
                &format!(
                    "SELECT id, checkout_id, node_id, managed_shard, managed_shards
                 FROM sessions
                 WHERE tenant_id = $1 AND workspace_id = $2 AND managed
                   AND ($3 IS NULL OR managed_purpose = $3)
                   AND checkout_id IS NOT NULL
                   AND status IN ({declared})
                 ORDER BY checkout_id, managed_shard",
                    declared = session_status::DECLARED_SQL
                ),
                params![tenant, workspace, purpose.map(|p| p.as_str().to_string())],
            )
            .await?)
    }

    async fn mark_ended(&self, tenant: TenantId, id: SessionId) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                &format!(
                    "UPDATE sessions SET status = 'exited', ended_at = {now}, updated_at = {now}
                     WHERE id = $1 AND tenant_id = $2
                       AND status IN ({live})",
                    now = type_mapping(self.db.engine()).now(),
                    live = session_status::LIVE_SQL
                ),
                params![id, tenant],
            )
            .await?)
    }

    async fn mark_stopped(&self, tenant: TenantId, id: SessionId) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                &format!(
                    "UPDATE sessions SET status = 'stopped', ended_at = {now}, updated_at = {now}
                     WHERE id = $1 AND tenant_id = $2
                       AND status IN ({live})",
                    now = type_mapping(self.db.engine()).now(),
                    live = session_status::LIVE_SQL
                ),
                params![id, tenant],
            )
            .await?)
    }

    async fn live_siblings(&self, workspace: WorkspaceId, excluding: SessionId) -> ApiResult<i64> {
        Ok(self
            .db
            .query_scalar(
                &format!(
                    "SELECT count(*) FROM sessions
                 WHERE workspace_id = $1 AND id <> $2
                   AND status IN ({live})",
                    live = session_status::LIVE_SQL
                ),
                params![workspace, excluding],
            )
            .await?)
    }
    async fn reclaim_and_held_ports(&self, node: NodeId) -> ApiResult<Vec<i32>> {
        // The reclaim, and the only thing that ever frees a lease. A session
        // that ended, was killed or was reaped left its rows behind on purpose;
        // they go now, so the ports they held are free to the read below.
        self.db
            .exec(
                &format!(
                    "DELETE FROM session_port_leases
                  WHERE node_id = $1
                    AND session_id IN (SELECT id FROM sessions
                                        WHERE status NOT IN ({live}))",
                    live = session_status::LIVE_SQL
                ),
                params![node],
            )
            .await?;
        Ok(self
            .db
            .query_scalar_all(
                "SELECT port FROM session_port_leases WHERE node_id = $1 ORDER BY port",
                params![node],
            )
            .await?)
    }

    async fn add_lease(&self, lease: NewPortLease) -> ApiResult<bool> {
        // A unique violation here is the RACE, not a bug: another session took
        // the port between the read and this write. Reported as `false` so the
        // caller picks again.
        //
        // The `(session, name)` index makes a re-lease of the same requirement
        // idempotent — a restart replaces its own row rather than stacking a
        // second one — so that conflict updates while a port conflict refuses.
        match self
            .db
            .exec(
                "INSERT INTO session_port_leases (id, session_id, node_id, name, env, port)
                 VALUES ($1, $2, $3, $4, $5, $6)
                 ON CONFLICT (session_id, name)
                 DO UPDATE SET port = EXCLUDED.port, env = EXCLUDED.env",
                params![
                    uuid::Uuid::new_v4(),
                    lease.session,
                    lease.node,
                    lease.name,
                    lease.env,
                    lease.port
                ],
            )
            .await
        {
            Ok(n) => Ok(n > 0),
            Err(e) if e.is_unique_violation() => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    async fn leases_of(&self, session: SessionId) -> ApiResult<Vec<LeasedPort>> {
        let rows: Vec<(String, String, i32)> = self
            .db
            .query_all(
                "SELECT name, env, port FROM session_port_leases
                  WHERE session_id = $1 ORDER BY name",
                params![session],
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|(name, env, port)| LeasedPort { name, env, port })
            .collect())
    }

    async fn leases_on(&self, node: NodeId) -> ApiResult<Vec<PortLease>> {
        let rows: Vec<(SessionId, String, String, String, String, i32)> = self
            .db
            .query_all(
                "SELECT s.id, s.name, s.status, l.name, l.env, l.port
                   FROM session_port_leases l
                   JOIN sessions s ON s.id = l.session_id
                  WHERE l.node_id = $1
                  ORDER BY l.port",
                params![node],
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(
                |(session_id, session_name, status, name, env, port)| PortLease {
                    session_id,
                    session_name,
                    status,
                    name,
                    env,
                    port,
                },
            )
            .collect())
    }

    async fn release_leases(&self, node: NodeId, id: SessionId) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                "DELETE FROM session_port_leases WHERE session_id = $1 AND node_id = $2",
                params![id, node],
            )
            .await?)
    }

    async fn messages(&self, session: SessionId) -> ApiResult<Vec<SessionMessage>> {
        // By the v7 id, which is time-ordered: `at` has a coarser resolution
        // than a burst of streamed lines arrives at, so ordering by it would
        // let two lines of one turn come back swapped.
        Ok(self
            .db
            .query_all(
                "SELECT * FROM session_messages WHERE session_id = $1 ORDER BY id",
                params![session],
            )
            .await?)
    }

    async fn append_message(&self, new: NewSessionMessage) -> ApiResult<SessionMessage> {
        Ok(self
            .db
            .query_one(
                "INSERT INTO session_messages
                   (id, session_id, role, body, permission_request_id, tool_name)
                 VALUES ($1, $2, $3, $4, $5, $6)
                 RETURNING *",
                params![
                    SessionMessageId::new(),
                    new.session,
                    new.role,
                    new.body,
                    new.permission_request_id,
                    new.tool_name
                ],
            )
            .await?)
    }

    async fn decide_permission(
        &self,
        session: SessionId,
        request_id: &str,
        allow: bool,
    ) -> ApiResult<bool> {
        let n = self
            .db
            .exec(
                "UPDATE session_messages SET decision = $4
                 WHERE session_id = $1 AND permission_request_id = $2
                   AND decision IS NULL AND role = $3",
                params![
                    session,
                    request_id,
                    "permission",
                    if allow { "allow" } else { "deny" }
                ],
            )
            .await?;
        Ok(n > 0)
    }
}

/// One row of the fake's lease table.
#[derive(Debug, Clone)]
struct FakeLease {
    session: SessionId,
    node: NodeId,
    name: String,
    env: String,
    port: i32,
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
    /// The lease rows, held beside the sessions exactly as the table is held
    /// beside `sessions` — so the fake can enforce the same two unique indexes
    /// in the same critical section as the write.
    leases: Mutex<Vec<FakeLease>>,
    /// Chat conversations (MAIN-502), in insertion order — which is the v7-id
    /// order the real query returns, since the fake mints its ids the same way.
    messages: Mutex<Vec<SessionMessage>>,
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
            .filter(|x| !filter.active_only || session_status::is_live(&x.status))
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
        // The real table refuses a second LIVE managed session on the same
        // CHECKOUT FOR THE SAME PURPOSE via a unique index, and that refusal is
        // what arbitrates the reconciler's race. A fake that accepted it would
        // let a caller test pass against behaviour the database does not have.
        // Null checkout is excluded exactly as the partial index excludes it,
        // and the purpose is part of the key exactly as it is there — an access
        // session must not refuse the review loop its own slot (MAIN-326), and
        // shard 1 must not refuse shard 0 its own (MAIN-446). The DIVISOR is
        // absent from the key here for the reason the index gives: one index
        // under two divisors is one slot described twice.
        if new.managed {
            let rows = self.inner.lock().unwrap();
            if new.checkout_id.is_some()
                && rows.iter().any(|x| {
                    x.managed
                        && x.checkout_id == new.checkout_id
                        && x.managed_purpose == new.managed_purpose
                        && x.managed_shard == new.managed_shard
                        && session_status::is_declared(&x.status)
                })
            {
                return Err(crate::error::ApiError::Conflict(
                    "a managed session already exists on that checkout".into(),
                ));
            }
        }
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
            managed: new.managed,
            managed_purpose: new.managed_purpose,
            managed_shard: new.managed_shard,
            managed_shards: new.managed_shards,
            interface: new.interface,
            checkout: None,
            node_online: None,
            leased_ports: Vec::new(),
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
                    && session_status::is_live(&x.status)
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

    async fn mark_viewer_presence(&self, id: SessionId, watched: bool) -> ApiResult<u64> {
        let (from, to) = if watched {
            ("detached", "running")
        } else {
            ("running", "detached")
        };
        let mut s = self.inner.lock().unwrap();
        Ok(match s.iter_mut().find(|x| x.id == id) {
            Some(x) if x.status == from => {
                x.status = to.to_string();
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

    async fn tenants_with_terminated(&self) -> ApiResult<Vec<TenantId>> {
        let s = self.inner.lock().unwrap();
        let mut out: Vec<TenantId> = s
            .iter()
            .filter(|x| matches!(x.status.as_str(), "exited" | "error") && x.ended_at.is_some())
            .map(|x| x.tenant_id)
            .collect();
        out.sort_by_key(|t| t.0);
        out.dedup();
        Ok(out)
    }

    async fn reap_terminated(&self, tenant: TenantId, retention_days: i64) -> ApiResult<u64> {
        let cutoff = chrono::Utc::now() - chrono::Duration::days(retention_days);
        let mut s = self.inner.lock().unwrap();
        let before = s.len();
        s.retain(|x| {
            !(x.tenant_id == tenant
                && matches!(x.status.as_str(), "exited" | "error")
                && x.ended_at.is_some_and(|e| e < cutoff))
        });
        Ok((before - s.len()) as u64)
    }

    async fn live_managed(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
        purpose: Option<ManagedPurpose>,
    ) -> ApiResult<Vec<ManagedSession>> {
        let mut out: Vec<ManagedSession> = self
            .inner
            .lock()
            .unwrap()
            .iter()
            .filter(|x| {
                x.tenant_id == tenant
                    && x.workspace_id == Some(workspace)
                    && x.managed
                    && purpose.is_none_or(|p| x.managed_purpose == p)
                    && x.checkout_id.is_some()
                    && session_status::is_live(&x.status)
            })
            .map(|x| ManagedSession {
                id: x.id,
                checkout_id: x.checkout_id.unwrap(),
                node_id: x.node_id,
                managed_shard: x.managed_shard,
                managed_shards: x.managed_shards,
            })
            .collect();
        out.sort_by_key(|s| (s.checkout_id.0, s.managed_shard));
        Ok(out)
    }

    async fn mark_ended(&self, tenant: TenantId, id: SessionId) -> ApiResult<u64> {
        let mut rows = self.inner.lock().unwrap();
        let Some(row) = rows
            .iter_mut()
            .find(|x| x.id == id && x.tenant_id == tenant)
        else {
            return Ok(0);
        };
        if !session_status::is_live(&row.status) {
            return Ok(0);
        }
        row.status = "exited".to_string();
        row.ended_at = Some(chrono::Utc::now());
        Ok(1)
    }

    async fn mark_stopped(&self, tenant: TenantId, id: SessionId) -> ApiResult<u64> {
        let mut rows = self.inner.lock().unwrap();
        let Some(row) = rows
            .iter_mut()
            .find(|x| x.id == id && x.tenant_id == tenant)
        else {
            return Ok(0);
        };
        if !session_status::is_live(&row.status) {
            return Ok(0);
        }
        row.status = session_status::STOPPED.to_string();
        row.ended_at = Some(chrono::Utc::now());
        Ok(1)
    }

    async fn live_siblings(&self, workspace: WorkspaceId, excluding: SessionId) -> ApiResult<i64> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .iter()
            .filter(|x| {
                x.workspace_id == Some(workspace)
                    && x.id != excluding
                    && session_status::is_live(&x.status)
            })
            .count() as i64)
    }
    async fn reclaim_and_held_ports(&self, node: NodeId) -> ApiResult<Vec<i32>> {
        let live: Vec<SessionId> = self
            .inner
            .lock()
            .unwrap()
            .iter()
            .filter(|x| session_status::is_live(&x.status))
            .map(|x| x.id)
            .collect();
        let mut leases = self.leases.lock().unwrap();
        // The lazy reclaim, same as the real one: a lease whose session is no
        // longer live simply goes, and nothing had to call anything to free it.
        leases.retain(|l| l.node != node || live.contains(&l.session));
        let mut held: Vec<i32> = leases
            .iter()
            .filter(|l| l.node == node)
            .map(|l| l.port)
            .collect();
        held.sort_unstable();
        Ok(held)
    }

    async fn add_lease(&self, lease: NewPortLease) -> ApiResult<bool> {
        let mut leases = self.leases.lock().unwrap();
        // `(node, port)` refuses; `(session, name)` replaces. Both indexes, in
        // the same critical section as the write, exactly as the table has them.
        if leases.iter().any(|l| {
            l.node == lease.node
                && l.port == lease.port
                && !(l.session == lease.session && l.name == lease.name)
        }) {
            return Ok(false);
        }
        leases.retain(|l| !(l.session == lease.session && l.name == lease.name));
        leases.push(FakeLease {
            session: lease.session,
            node: lease.node,
            name: lease.name,
            env: lease.env,
            port: lease.port,
        });
        Ok(true)
    }

    async fn leases_of(&self, session: SessionId) -> ApiResult<Vec<LeasedPort>> {
        let mut out: Vec<LeasedPort> = self
            .leases
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.session == session)
            .map(|l| LeasedPort {
                name: l.name.clone(),
                env: l.env.clone(),
                port: l.port,
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn leases_on(&self, node: NodeId) -> ApiResult<Vec<PortLease>> {
        let rows = self.inner.lock().unwrap();
        let mut out: Vec<PortLease> = self
            .leases
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.node == node)
            .filter_map(|l| {
                let s = rows.iter().find(|x| x.id == l.session)?;
                Some(PortLease {
                    session_id: l.session,
                    session_name: s.name.clone(),
                    status: s.status.clone(),
                    name: l.name.clone(),
                    env: l.env.clone(),
                    port: l.port,
                })
            })
            .collect();
        out.sort_by_key(|l| l.port);
        Ok(out)
    }

    async fn release_leases(&self, node: NodeId, id: SessionId) -> ApiResult<u64> {
        let mut leases = self.leases.lock().unwrap();
        let before = leases.len();
        leases.retain(|l| !(l.session == id && l.node == node));
        Ok((before - leases.len()) as u64)
    }

    async fn messages(&self, session: SessionId) -> ApiResult<Vec<SessionMessage>> {
        Ok(self
            .messages
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.session_id == session)
            .cloned()
            .collect())
    }

    async fn append_message(&self, new: NewSessionMessage) -> ApiResult<SessionMessage> {
        let msg = SessionMessage {
            id: SessionMessageId::new(),
            session_id: new.session,
            role: new.role,
            body: new.body,
            permission_request_id: new.permission_request_id,
            tool_name: new.tool_name,
            decision: None,
            at: chrono::Utc::now(),
        };
        self.messages.lock().unwrap().push(msg.clone());
        Ok(msg)
    }

    async fn decide_permission(
        &self,
        session: SessionId,
        request_id: &str,
        allow: bool,
    ) -> ApiResult<bool> {
        // The real UPDATE's `decision IS NULL` guard, kept here because the
        // whole point of the return value is that a second answer changes
        // nothing — a fake that answered twice would let a caller test pass
        // against behaviour the database does not have.
        let mut msgs = self.messages.lock().unwrap();
        let Some(m) = msgs.iter_mut().find(|m| {
            m.session_id == session
                && m.permission_request_id.as_deref() == Some(request_id)
                && m.role == "permission"
                && m.decision.is_none()
        }) else {
            return Ok(false);
        };
        m.decision = Some(if allow { "allow" } else { "deny" }.into());
        Ok(true)
    }
}
