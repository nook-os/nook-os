//! The cross-cutting read model (MAIN-304).
//!
//! Every other repository in this chain owns an aggregate — one table family,
//! one card, one blast radius. These three surfaces own none, which is exactly
//! why the per-aggregate chain (MAIN-245–258) could not home them and the
//! MAIN-260 guard still exempted all three:
//!
//! - the **activity event writer**, used by every aggregate there is;
//! - the **activity feed**, whose scope is built from users, nodes and sessions;
//! - the **Mission Control overview**, which joins workspaces, checkouts, nodes,
//!   sessions and tasks into one payload.
//!
//! Filing any of them under one aggregate would hand that aggregate's card a
//! query about four others. So they get a repository named for what they ARE —
//! a read model — rather than for a table.
//!
//! **What this deliberately does NOT own.** `ActivityScope` also resolves the
//! caller's person and their sibling user ids, both reads of `users`.
//! [`crate::repo::identity::IdentityRepository`] already exposes exactly those
//! (`person_id_of`, `sibling_user_ids`), and the module doc next door is explicit
//! about why a second trait must not: *"a second trait touching `users` would be
//! two places to change when that table does, which is the problem the chain
//! exists to remove."* The activity scope calls identity for those two and this
//! trait for the rest.
//!
//! Methods are intent-named and coarse; no `sqlx` type appears in any signature,
//! and row mapping lives inside the impl.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use nook_db::{params, Db, DbPool, Postgres, TypeMapping};
use nook_types::*;
use uuid::Uuid;

use crate::error::ApiResult;

/// An event about to be recorded. The loose columns rather than
/// `events::EventDraft`, so the repository layer does not depend on a service
/// type — the draft is mapped onto this at the one call site that builds one.
#[derive(Debug, Clone)]
pub struct NewEvent {
    pub kind: String,
    pub actor_type: Option<String>,
    pub actor_id: Option<Uuid>,
    pub workspace_id: Option<WorkspaceId>,
    pub node_id: Option<NodeId>,
    pub session_id: Option<SessionId>,
    pub payload: serde_json::Value,
}

/// Which events a member may see, already resolved to id sets.
///
/// `None` is the whole tenant — an owner, an admin, or a node credential. The
/// sets are bound into SQL exactly as `ActivityScope::allows` matches them in
/// memory, so the page and the live bus enforce one rule (MAIN-134).
#[derive(Debug, Clone, Default)]
pub struct EventScopeIds {
    pub user_ids: Vec<Uuid>,
    pub node_ids: Vec<Uuid>,
    pub session_ids: Vec<Uuid>,
}

/// One page of the activity feed, as asked for.
#[derive(Debug, Clone)]
pub struct EventsQuery {
    pub tenant: TenantId,
    pub workspace: Option<WorkspaceId>,
    pub kind_prefix: Option<String>,
    pub before: Option<DateTime<Utc>>,
    /// Already clamped by the caller.
    pub limit: i64,
    /// `None` sees the whole tenant.
    pub scope: Option<EventScopeIds>,
}

/// A checkout as the overview needs it: the checkout joined to the node it sits
/// on, flattened, because the payload nests them the other way round.
#[derive(Debug, Clone)]
pub struct OverviewCheckoutRow {
    pub id: NodeWorkspaceId,
    pub workspace_id: WorkspaceId,
    pub node_id: NodeId,
    pub node_name: String,
    pub node_status: String,
    pub path: String,
    pub git_branch: Option<String>,
    pub git_status: serde_json::Value,
    pub kind: String,
    pub missing_at: Option<DateTime<Utc>>,
}

/// The ticket a checkout is working, already resolved to a display key.
#[derive(Debug, Clone)]
pub struct OverviewTaskRow {
    pub checkout_id: NodeWorkspaceId,
    pub key: String,
    pub title: String,
    pub column_type: String,
}

#[async_trait]
pub trait ReadModelRepository: Send + Sync {
    /// Record one activity event, returning it as stored.
    async fn record_event(&self, tenant: TenantId, event: NewEvent) -> ApiResult<Event>;

    /// The nodes this person owns in a tenant — one third of a member's
    /// activity scope.
    async fn node_ids_owned_by(&self, tenant: TenantId, person: Uuid) -> ApiResult<Vec<Uuid>>;

    /// The sessions any of these users created — the last third of that scope.
    async fn session_ids_created_by(
        &self,
        tenant: TenantId,
        user_ids: &[Uuid],
    ) -> ApiResult<Vec<Uuid>>;

    /// One page of the activity feed, newest first.
    async fn events_page(&self, q: EventsQuery) -> ApiResult<Vec<Event>>;

    /// Repo identity for every workspace in a tenant. The overview's content
    /// joins decide which of them actually appear.
    async fn overview_workspaces(&self, tenant: TenantId) -> ApiResult<Vec<Workspace>>;

    /// Checkouts the caller can see, node-scoped identically to `list_nodes`:
    /// `None` is the whole fleet, `Some(person)` is own plus shared.
    async fn overview_checkouts(
        &self,
        tenant: TenantId,
        node_owner: Option<Uuid>,
    ) -> ApiResult<Vec<OverviewCheckoutRow>>;

    /// The ticket each checkout is working, card-visibility scoped.
    async fn overview_checkout_tasks(
        &self,
        tenant: TenantId,
        task_viewer: Option<UserId>,
    ) -> ApiResult<Vec<OverviewTaskRow>>;
}

pub struct DbReadModelRepository {
    db: DbPool,
}

impl DbReadModelRepository {
    pub fn new(db: DbPool) -> Self {
        Self { db }
    }
}

#[async_trait]
impl ReadModelRepository for DbReadModelRepository {
    async fn record_event(&self, tenant: TenantId, event: NewEvent) -> ApiResult<Event> {
        Ok(self
            .db
            .query_one::<Event>(
                "INSERT INTO events (id, tenant_id, kind, actor_type, actor_id, workspace_id, node_id, session_id, payload)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         RETURNING *",
                params![
                    EventId::new(),
                    tenant,
                    event.kind,
                    event.actor_type,
                    event.actor_id,
                    event.workspace_id.map(|x| x.0),
                    event.node_id.map(|x| x.0),
                    event.session_id.map(|x| x.0),
                    &event.payload
                ],
            )
            .await?)
    }

    async fn node_ids_owned_by(&self, tenant: TenantId, person: Uuid) -> ApiResult<Vec<Uuid>> {
        Ok(self
            .db
            .query_scalar_all(
                "SELECT id FROM nodes WHERE tenant_id = $1 AND owner_person_id = $2",
                params![tenant, person],
            )
            .await?)
    }

    async fn session_ids_created_by(
        &self,
        tenant: TenantId,
        user_ids: &[Uuid],
    ) -> ApiResult<Vec<Uuid>> {
        Ok(self
            .db
            .query_scalar_all(
                "SELECT id FROM sessions WHERE tenant_id = $1 AND created_by = ANY($2)",
                params![tenant, user_ids],
            )
            .await?)
    }

    async fn events_page(&self, q: EventsQuery) -> ApiResult<Vec<Event>> {
        // The list filter is the SQL twin of `ActivityScope::allows`, bound from
        // the same resolved sets — so page and bus enforce one rule (MAIN-134).
        let mut sql = format!(
            "SELECT * FROM events
         WHERE tenant_id = $1
           AND ({ws} IS NULL OR workspace_id = $2)
           AND ({kind} IS NULL OR kind LIKE $3 || '%')
           AND ({before} IS NULL OR occurred_at < $4)",
            ws = Postgres.cast("$2", "uuid"),
            before = Postgres.cast("$4", "timestamptz"),
            kind = Postgres.cast("$3", "text"),
        );
        if q.scope.is_some() {
            sql.push_str(" AND (actor_id = ANY($6) OR node_id = ANY($7) OR session_id = ANY($8))");
        }
        sql.push_str(" ORDER BY occurred_at DESC, id DESC LIMIT $5");
        let mut binds = params![
            q.tenant,
            q.workspace.map(|w| w.0),
            q.kind_prefix,
            q.before,
            q.limit
        ];
        if let Some(scope) = &q.scope {
            binds.extend(params![
                &scope.user_ids[..],
                &scope.node_ids[..],
                &scope.session_ids[..]
            ]);
        }
        Ok(self.db.query_all(&sql, binds).await?)
    }

    async fn overview_workspaces(&self, tenant: TenantId) -> ApiResult<Vec<Workspace>> {
        Ok(self
            .db
            .query_all(
                "SELECT * FROM workspaces WHERE tenant_id = $1 ORDER BY name",
                params![tenant],
            )
            .await?)
    }

    async fn overview_checkouts(
        &self,
        tenant: TenantId,
        node_owner: Option<Uuid>,
    ) -> ApiResult<Vec<OverviewCheckoutRow>> {
        #[derive(nook_db::FromDbRow)]
        struct Row {
            id: NodeWorkspaceId,
            workspace_id: WorkspaceId,
            node_id: NodeId,
            node_name: String,
            node_status: String,
            path: String,
            git_branch: Option<String>,
            git_status: serde_json::Value,
            kind: String,
            missing_at: Option<DateTime<Utc>>,
        }
        let rows: Vec<Row> = self
            .db
            .query_all(
                &format!(
                    "SELECT nw.id, nw.workspace_id, n.id AS node_id, n.name AS node_name,
                        n.status AS node_status, nw.path, nw.git_branch, nw.git_status,
                        nw.kind, nw.missing_at
                 FROM node_workspaces nw
                 JOIN nodes n ON n.id = nw.node_id
                 WHERE nw.tenant_id = $1
                   AND ({owner} IS NULL OR n.owner_person_id = $2 OR n.shared)
                 ORDER BY n.name, nw.path",
                    owner = Postgres.cast("$2", "uuid")
                ),
                params![tenant, node_owner],
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| OverviewCheckoutRow {
                id: r.id,
                workspace_id: r.workspace_id,
                node_id: r.node_id,
                node_name: r.node_name,
                node_status: r.node_status,
                path: r.path,
                git_branch: r.git_branch,
                git_status: r.git_status,
                kind: r.kind,
                missing_at: r.missing_at,
            })
            .collect())
    }

    async fn overview_checkout_tasks(
        &self,
        tenant: TenantId,
        task_viewer: Option<UserId>,
    ) -> ApiResult<Vec<OverviewTaskRow>> {
        // The ticket each checkout is working (MAIN-230), by the two joins that
        // can know it:
        //
        //   tasks.checkout_id          the durable one (MAIN-225), but NULL until
        //                              discovery scans the fresh worktree;
        //   tasks.session_id → sessions.checkout_id
        //                              the one that covers the gap, so work
        //                              started seconds ago still names its ticket.
        //
        // `task_viewer` is the SAME predicate every other read of a card uses —
        // literally the same string, from `tasks::visible_sql` (MAIN-265). The
        // `IS NULL` leg around it is this endpoint's own question, not part of
        // the rule: `None` means the caller already sees the whole tenant.
        // Without it this endpoint would be a side-channel around card
        // visibility — a private ticket's key leaking onto a shared node's row is
        // exactly the class of hole MAIN-226's tests exist to catch.
        #[derive(nook_db::FromDbRow)]
        struct Row {
            checkout_id: NodeWorkspaceId,
            key: String,
            title: String,
            column_type: String,
        }
        let rows: Vec<Row> = self
            .db
            .query_all(
                &format!(
                    "SELECT DISTINCT
                        COALESCE(t.checkout_id, s.checkout_id) AS checkout_id,
                        b.key || '-' || t.number AS key,
                        t.title,
                        c.type AS column_type
                   FROM tasks t
                   JOIN boards b ON b.id = t.board_id
                   JOIN board_columns c ON c.id = t.column_id
                   LEFT JOIN sessions s ON s.id = t.session_id
                  WHERE t.tenant_id = $1
                    AND t.archived_at IS NULL
                    AND COALESCE(t.checkout_id, s.checkout_id) IS NOT NULL
                    AND ({viewer} IS NULL OR {visible})
                  ORDER BY key",
                    viewer = Postgres.cast("$2", "uuid"),
                    visible = crate::services::tasks::visible_sql("t", "$2"),
                ),
                params![tenant, task_viewer.map(|u| u.0)],
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| OverviewTaskRow {
                checkout_id: r.checkout_id,
                key: r.key,
                title: r.title,
                column_type: r.column_type,
            })
            .collect())
    }
}

/// An in-memory [`ReadModelRepository`] for tests that should not need a
/// database (MAIN-304 AC-3).
///
/// Faithful where the behaviour under test lives — events are stored, returned
/// newest-first, and filtered by tenant, workspace, kind prefix, cursor, limit
/// and scope, which is the whole of what the feed decides — and deliberately
/// simple elsewhere. The overview reads are served from rows a test pushes in.
#[derive(Default)]
pub struct FakeReadModelRepository {
    inner: std::sync::Mutex<Fake>,
}

#[derive(Default)]
struct Fake {
    events: Vec<Event>,
    nodes_by_person: Vec<(TenantId, Uuid, Uuid)>,
    sessions_by_creator: Vec<(TenantId, Uuid, Uuid)>,
    workspaces: Vec<Workspace>,
    checkouts: Vec<OverviewCheckoutRow>,
    tasks: Vec<OverviewTaskRow>,
}

impl FakeReadModelRepository {
    pub fn new() -> Self {
        Self::default()
    }

    /// A node owned by `person` in `tenant`.
    pub fn add_owned_node(&self, tenant: TenantId, person: Uuid, node: Uuid) {
        self.inner
            .lock()
            .unwrap()
            .nodes_by_person
            .push((tenant, person, node));
    }

    /// A session created by `user` in `tenant`.
    pub fn add_created_session(&self, tenant: TenantId, user: Uuid, session: Uuid) {
        self.inner
            .lock()
            .unwrap()
            .sessions_by_creator
            .push((tenant, user, session));
    }

    pub fn add_workspace(&self, w: Workspace) {
        self.inner.lock().unwrap().workspaces.push(w);
    }

    pub fn add_checkout(&self, c: OverviewCheckoutRow) {
        self.inner.lock().unwrap().checkouts.push(c);
    }

    pub fn add_checkout_task(&self, t: OverviewTaskRow) {
        self.inner.lock().unwrap().tasks.push(t);
    }

    /// How many events have been recorded — for asserting a write happened.
    pub fn event_count(&self) -> usize {
        self.inner.lock().unwrap().events.len()
    }
}

#[async_trait]
impl ReadModelRepository for FakeReadModelRepository {
    async fn record_event(&self, tenant: TenantId, event: NewEvent) -> ApiResult<Event> {
        let stored = Event {
            id: EventId::new(),
            tenant_id: tenant,
            // Monotonic per insert, so "newest first" is well-defined without a
            // clock a test would have to wait on.
            occurred_at: Utc::now(),
            kind: event.kind,
            actor_type: event.actor_type,
            actor_id: event.actor_id,
            workspace_id: event.workspace_id,
            node_id: event.node_id,
            session_id: event.session_id,
            payload: event.payload,
        };
        self.inner.lock().unwrap().events.push(stored.clone());
        Ok(stored)
    }

    async fn node_ids_owned_by(&self, tenant: TenantId, person: Uuid) -> ApiResult<Vec<Uuid>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .nodes_by_person
            .iter()
            .filter(|(t, p, _)| *t == tenant && *p == person)
            .map(|(_, _, n)| *n)
            .collect())
    }

    async fn session_ids_created_by(
        &self,
        tenant: TenantId,
        user_ids: &[Uuid],
    ) -> ApiResult<Vec<Uuid>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .sessions_by_creator
            .iter()
            .filter(|(t, u, _)| *t == tenant && user_ids.contains(u))
            .map(|(_, _, s)| *s)
            .collect())
    }

    async fn events_page(&self, q: EventsQuery) -> ApiResult<Vec<Event>> {
        let inner = self.inner.lock().unwrap();
        let mut out: Vec<Event> = inner
            .events
            .iter()
            .filter(|e| e.tenant_id == q.tenant)
            .filter(|e| q.workspace.is_none_or(|w| e.workspace_id == Some(w)))
            .filter(|e| {
                q.kind_prefix
                    .as_deref()
                    .is_none_or(|p| e.kind.starts_with(p))
            })
            .filter(|e| q.before.is_none_or(|b| e.occurred_at < b))
            .filter(|e| match &q.scope {
                None => true,
                // The same three-way match `ActivityScope::allows` makes.
                Some(s) => {
                    e.actor_id.is_some_and(|a| s.user_ids.contains(&a))
                        || e.node_id.is_some_and(|n| s.node_ids.contains(&n.0))
                        || e.session_id.is_some_and(|x| s.session_ids.contains(&x.0))
                }
            })
            .cloned()
            .collect();
        out.sort_by(|a, b| b.occurred_at.cmp(&a.occurred_at).then(b.id.0.cmp(&a.id.0)));
        out.truncate(q.limit.max(0) as usize);
        Ok(out)
    }

    async fn overview_workspaces(&self, tenant: TenantId) -> ApiResult<Vec<Workspace>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .workspaces
            .iter()
            .filter(|w| w.tenant_id == tenant)
            .cloned()
            .collect())
    }

    async fn overview_checkouts(
        &self,
        _tenant: TenantId,
        _node_owner: Option<Uuid>,
    ) -> ApiResult<Vec<OverviewCheckoutRow>> {
        Ok(self.inner.lock().unwrap().checkouts.clone())
    }

    async fn overview_checkout_tasks(
        &self,
        _tenant: TenantId,
        _task_viewer: Option<UserId>,
    ) -> ApiResult<Vec<OverviewTaskRow>> {
        Ok(self.inner.lock().unwrap().tasks.clone())
    }
}
