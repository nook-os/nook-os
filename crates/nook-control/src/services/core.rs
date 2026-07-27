//! Shared queries used by both REST handlers and MCP tools.

use chrono::{DateTime, Utc};
use nook_types::*;
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::ApiResult;

pub async fn workspace_locations(
    db: &PgPool,
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
    )> = sqlx::query_as(
        "SELECT n.id, n.name, n.status, nw.path, nw.git_branch, nw.git_status
             FROM node_workspaces nw
             JOIN nodes n ON n.id = nw.node_id
             WHERE nw.tenant_id = $1 AND nw.workspace_id = $2
             ORDER BY n.name",
    )
    .bind(tenant)
    .bind(workspace)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(node_id, node_name, node_status, path, git_branch, git_status)| WorkspaceLocation {
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
            },
        )
        .collect())
}

pub async fn list_workspaces(db: &PgPool, tenant: TenantId) -> ApiResult<Vec<WorkspaceDetail>> {
    let workspaces: Vec<Workspace> =
        sqlx::query_as("SELECT * FROM workspaces WHERE tenant_id = $1 ORDER BY name")
            .bind(tenant)
            .fetch_all(db)
            .await?;
    let mut out = Vec::with_capacity(workspaces.len());
    for workspace in workspaces {
        let locations = workspace_locations(db, tenant, workspace.id).await?;
        out.push(WorkspaceDetail {
            workspace,
            locations,
        });
    }
    Ok(out)
}

pub async fn get_workspace(
    db: &PgPool,
    tenant: TenantId,
    id: WorkspaceId,
) -> ApiResult<Option<WorkspaceDetail>> {
    let workspace: Option<Workspace> =
        sqlx::query_as("SELECT * FROM workspaces WHERE tenant_id = $1 AND id = $2")
            .bind(tenant)
            .bind(id)
            .fetch_optional(db)
            .await?;
    match workspace {
        None => Ok(None),
        Some(workspace) => {
            let locations = workspace_locations(db, tenant, workspace.id).await?;
            Ok(Some(WorkspaceDetail {
                workspace,
                locations,
            }))
        }
    }
}

/// List a tenant's nodes, optionally scoped to a single owner person (MAIN-132).
/// `owner = Some(person)` returns only that person's nodes (a member's own
/// view); `owner = None` returns the whole fleet (owner/admin, and node tokens
/// whose view is unchanged).
pub async fn list_nodes(
    db: &PgPool,
    tenant: TenantId,
    owner: Option<uuid::Uuid>,
) -> ApiResult<Vec<Node>> {
    Ok(sqlx::query_as(
        "SELECT id, tenant_id, name, hostname, platform, capabilities, resources, status,
                last_seen_at, owner_person_id, created_at, updated_at
         FROM nodes
         WHERE tenant_id = $1 AND ($2::uuid IS NULL OR owner_person_id = $2)
         ORDER BY name",
    )
    .bind(tenant)
    .bind(owner)
    .fetch_all(db)
    .await?)
}

/// List a tenant's sessions, optionally scoped to a single creator (MAIN-133).
/// `creator = Some(user)` returns only sessions that user started — a member's
/// own view, which naturally excludes `created_by NULL` (legacy/MCP) rows since
/// `NULL = user` is never true. `creator = None` returns all sessions (the
/// owner/admin metadata view, and the unchanged view MCP/dispatcher get). This
/// is the metadata/list layer only; content access stays with `session_guard`.
pub async fn list_sessions(
    db: &PgPool,
    tenant: TenantId,
    workspace: Option<WorkspaceId>,
    active_only: bool,
    creator: Option<UserId>,
) -> ApiResult<Vec<Session>> {
    let mut sql = String::from("SELECT * FROM sessions WHERE tenant_id = $1");
    let mut n = 1;
    if workspace.is_some() {
        n += 1;
        sql.push_str(&format!(" AND workspace_id = ${n}"));
    }
    if creator.is_some() {
        n += 1;
        sql.push_str(&format!(" AND created_by = ${n}"));
    }
    if active_only {
        sql.push_str(" AND status IN ('starting', 'running', 'detached')");
    }
    sql.push_str(" ORDER BY created_at DESC");
    // Binds follow the same order the placeholders were numbered above.
    let mut q = sqlx::query_as(&sql).bind(tenant);
    if let Some(w) = workspace {
        q = q.bind(w);
    }
    if let Some(c) = creator {
        q = q.bind(c);
    }
    Ok(q.fetch_all(db).await?)
}

/// Who may see which activity events (MAIN-134). A tenant owner/admin — and a
/// node credential, whose feed is unchanged — sees the whole tenant's activity
/// (the audit view). A member sees only events they caused (`actor_id` is one of
/// their user ids, across every tenant they act in), events on a node they own,
/// or events on a session they created.
///
/// The SAME resolved sets drive both the REST list (bound as arrays into the
/// `WHERE`) and the live bus (`allows`, evaluated per connection), so the
/// Activity page and its live push can never disagree — a list-only filter would
/// leak through the bus, which is exactly what the page's live buffer renders.
pub enum ActivityScope {
    /// Owner/admin/node: the unfiltered tenant feed.
    All,
    /// A member: only their own actions and their own resources' events. The
    /// sets are resolved once (at request time for the list, at connect time for
    /// the bus); a resource acquired mid-connection is picked up on the UI's
    /// next reconnect. Fails closed — empty sets mean "see nothing", never "all".
    Member {
        user_ids: Vec<Uuid>,
        node_ids: Vec<Uuid>,
        session_ids: Vec<Uuid>,
    },
}

impl ActivityScope {
    /// Resolve the caller's activity scope from their role and owned resources.
    pub async fn load(
        db: &PgPool,
        tenant: TenantId,
        auth: &crate::auth::AuthCtx,
    ) -> ApiResult<Self> {
        // A node credential is not a person watching a feed — unchanged tenant
        // view. (The role query lives here, not in shared auth code, so this
        // stays independent of MAIN-132's is_tenant_admin.)
        if !matches!(auth.principal, crate::auth::Principal::User) {
            return Ok(Self::All);
        }
        let role: Option<(String,)> =
            sqlx::query_as("SELECT role FROM users WHERE id = $1 AND tenant_id = $2")
                .bind(auth.user_id)
                .bind(tenant)
                .fetch_optional(db)
                .await?;
        if matches!(
            role.as_ref().map(|(r,)| r.as_str()),
            Some("owner") | Some("admin")
        ) {
            return Ok(Self::All);
        }
        // A member: their person's user ids, the nodes that person owns, and the
        // sessions those user ids created.
        let person: Uuid = sqlx::query_scalar("SELECT person_id FROM users WHERE id = $1")
            .bind(auth.user_id)
            .fetch_one(db)
            .await?;
        let user_ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM users WHERE person_id = $1")
            .bind(person)
            .fetch_all(db)
            .await?;
        let node_ids: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM nodes WHERE tenant_id = $1 AND owner_person_id = $2",
        )
        .bind(tenant)
        .bind(person)
        .fetch_all(db)
        .await?;
        let session_ids: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM sessions WHERE tenant_id = $1 AND created_by = ANY($2)",
        )
        .bind(tenant)
        .bind(&user_ids)
        .fetch_all(db)
        .await?;
        Ok(Self::Member {
            user_ids,
            node_ids,
            session_ids,
        })
    }

    /// Does this event reach the caller? The bus applies this per connection; the
    /// list binds the same sets into SQL. `All` sees everything; a member sees an
    /// event caused by them, on their node, or on their session.
    pub fn allows(&self, e: &Event) -> bool {
        match self {
            Self::All => true,
            Self::Member {
                user_ids,
                node_ids,
                session_ids,
            } => {
                e.actor_id.is_some_and(|a| user_ids.contains(&a))
                    || e.node_id.is_some_and(|n| node_ids.contains(&n.0))
                    || e.session_id.is_some_and(|s| session_ids.contains(&s.0))
            }
        }
    }
}

pub async fn events_page(
    db: &PgPool,
    tenant: TenantId,
    workspace: Option<WorkspaceId>,
    kind_prefix: Option<String>,
    before: Option<DateTime<Utc>>,
    limit: i64,
    scope: &ActivityScope,
) -> ApiResult<EventsPage> {
    let limit = limit.clamp(1, 200);
    // The list filter is the SQL twin of `ActivityScope::allows`, bound from the
    // same resolved sets — so page and bus enforce one rule (MAIN-134).
    let mut sql = String::from(
        "SELECT * FROM events
         WHERE tenant_id = $1
           AND ($2::uuid IS NULL OR workspace_id = $2)
           AND ($3::text IS NULL OR kind LIKE $3 || '%')
           AND ($4::timestamptz IS NULL OR occurred_at < $4)",
    );
    if matches!(scope, ActivityScope::Member { .. }) {
        sql.push_str(" AND (actor_id = ANY($6) OR node_id = ANY($7) OR session_id = ANY($8))");
    }
    sql.push_str(" ORDER BY occurred_at DESC, id DESC LIMIT $5");
    let mut q = sqlx::query_as(&sql)
        .bind(tenant)
        .bind(workspace)
        .bind(kind_prefix)
        .bind(before)
        .bind(limit);
    if let ActivityScope::Member {
        user_ids,
        node_ids,
        session_ids,
    } = scope
    {
        q = q.bind(user_ids).bind(node_ids).bind(session_ids);
    }
    let events: Vec<Event> = q.fetch_all(db).await?;
    let next_cursor = if events.len() as i64 == limit {
        events.last().map(|e| e.occurred_at)
    } else {
        None
    };
    Ok(EventsPage {
        events,
        next_cursor,
    })
}

/// The operator audit trail, paged by keyset cursor and filtered by an optional
/// server-side search (MAIN-43).
///
/// Search (`q`) is case-insensitive and matches across the event kind, the
/// tenant slug, and the actor (type or id) — the whole log, not just the page
/// in hand, because the `WHERE` runs before `LIMIT`. Pagination is keyset on the
/// row's UUID v7 `id`: `after` is the last id the caller has seen, and rows are
/// walked `id DESC`, so each page is strictly older with no offset to drift.
///
/// The cursor is the last id of a full page (mirroring `events_page`): when a
/// page comes back short of `limit` there is no more, so `next_cursor` is null.
/// A caller that pages one past the end gets an empty page and a null cursor —
/// a clean end-of-list, not an error.
///
/// Kinds, actors and times only — never payloads, which can carry a branch name
/// or task title this surface must not hand over (the same rule `audit_log`
/// enforced before it grew a cursor).
pub async fn operator_audit_page(
    db: &PgPool,
    q: Option<String>,
    after: Option<EventId>,
    limit: i64,
) -> ApiResult<OperatorAuditPage> {
    let limit = limit.clamp(1, 200);
    // An empty or whitespace-only search is "no filter", not "match the empty
    // string" — the search box clears to that and must show the whole log.
    let q = q.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let rows: Vec<OperatorAuditEntry> = sqlx::query_as(
        "SELECT e.id, e.kind, e.actor_type, e.actor_id, e.tenant_id,
                t.slug AS tenant_slug, e.occurred_at
         FROM events e JOIN tenants t ON t.id = e.tenant_id
         WHERE (e.kind LIKE 'operator.%' OR e.kind LIKE 'rbac.%'
                OR e.kind LIKE 'node.%'  OR e.kind LIKE 'user.%')
           AND ($2::text IS NULL OR (
                    e.kind ILIKE '%' || $2 || '%'
                 OR t.slug ILIKE '%' || $2 || '%'
                 OR e.actor_type ILIKE '%' || $2 || '%'
                 OR e.actor_id::text ILIKE '%' || $2 || '%'))
           AND ($3::uuid IS NULL OR e.id < $3)
         ORDER BY e.id DESC
         LIMIT $1",
    )
    .bind(limit)
    .bind(q)
    .bind(after)
    .fetch_all(db)
    .await?;
    let next_cursor = if rows.len() as i64 == limit {
        rows.last().map(|r| r.id)
    } else {
        None
    };
    Ok(OperatorAuditPage { rows, next_cursor })
}

/// Normalize a search box value: whitespace-only is "no filter", not "match the
/// empty string". Shared by the operator list queries (MAIN-44).
fn search_filter(q: Option<String>) -> Option<String> {
    q.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Operator tenants, keyset-paginated + searched (slug/name), mirroring
/// `operator_audit_page`. Rows come back WITHOUT the policy-gated fields
/// (`repositories`/`task_titles`); the handler enriches them per opted-in org.
pub async fn operator_tenants_page(
    db: &PgPool,
    q: Option<String>,
    after: Option<TenantId>,
    limit: i64,
) -> ApiResult<OperatorTenantPage> {
    let limit = limit.clamp(1, 200);
    let q = search_filter(q);
    let rows: Vec<OperatorTenant> = sqlx::query_as(
        "SELECT t.id, t.slug, t.org_id, t.created_at,
                (SELECT count(*) FROM users u WHERE u.tenant_id = t.id)    AS members,
                (SELECT count(*) FROM nodes n WHERE n.tenant_id = t.id)    AS nodes,
                (SELECT count(*) FROM sessions s
                  WHERE s.tenant_id = t.id
                    AND s.status IN ('starting','running','detached'))     AS active_sessions,
                (SELECT count(*) FROM workspaces w WHERE w.tenant_id = t.id) AS workspaces
         FROM tenants t
         WHERE ($2::text IS NULL OR t.slug ILIKE '%' || $2 || '%' OR t.name ILIKE '%' || $2 || '%')
           AND ($3::uuid IS NULL OR t.id < $3)
         ORDER BY t.id DESC
         LIMIT $1",
    )
    .bind(limit)
    .bind(q)
    .bind(after)
    .fetch_all(db)
    .await?;
    let next_cursor = if rows.len() as i64 == limit {
        rows.last().map(|r| r.id)
    } else {
        None
    };
    Ok(OperatorTenantPage { rows, next_cursor })
}

/// Operator nodes, keyset-paginated + searched (name/tenant slug/platform/status).
pub async fn operator_nodes_page(
    db: &PgPool,
    q: Option<String>,
    after: Option<NodeId>,
    limit: i64,
) -> ApiResult<OperatorNodePage> {
    let limit = limit.clamp(1, 200);
    let q = search_filter(q);
    let rows: Vec<OperatorNode> = sqlx::query_as(
        "SELECT n.id, n.name, n.platform, n.status, n.last_seen_at, n.resources,
                n.tenant_id, t.slug AS tenant_slug,
                (SELECT count(*) FROM sessions s
                  WHERE s.node_id = n.id
                    AND s.status IN ('starting','running','detached')) AS active_sessions
         FROM nodes n JOIN tenants t ON t.id = n.tenant_id
         WHERE ($2::text IS NULL OR (
                    n.name ILIKE '%' || $2 || '%'
                 OR t.slug ILIKE '%' || $2 || '%'
                 OR n.platform ILIKE '%' || $2 || '%'
                 OR n.status ILIKE '%' || $2 || '%'))
           AND ($3::uuid IS NULL OR n.id < $3)
         ORDER BY n.id DESC
         LIMIT $1",
    )
    .bind(limit)
    .bind(q)
    .bind(after)
    .fetch_all(db)
    .await?;
    let next_cursor = if rows.len() as i64 == limit {
        rows.last().map(|r| r.id)
    } else {
        None
    };
    Ok(OperatorNodePage { rows, next_cursor })
}

/// Operator role bindings, keyset-paginated + searched (email/role/scope).
pub async fn operator_bindings_page(
    db: &PgPool,
    q: Option<String>,
    after: Option<uuid::Uuid>,
    limit: i64,
) -> ApiResult<OperatorBindingPage> {
    let limit = limit.clamp(1, 200);
    let q = search_filter(q);
    let rows: Vec<BindingRow> = sqlx::query_as(
        "SELECT b.id, u.email, u.display_name, b.role_key, b.scope_type, b.scope_id,
                COALESCE(o.slug, t.slug) AS scope_label, b.created_at
         FROM role_bindings b
         JOIN users u ON u.id = b.subject_id
         LEFT JOIN orgs o    ON b.scope_type = 'org'    AND o.id = b.scope_id
         LEFT JOIN tenants t ON b.scope_type = 'tenant' AND t.id = b.scope_id
         WHERE ($2::text IS NULL OR (
                    u.email ILIKE '%' || $2 || '%'
                 OR b.role_key ILIKE '%' || $2 || '%'
                 OR b.scope_type ILIKE '%' || $2 || '%'
                 OR COALESCE(o.slug, t.slug) ILIKE '%' || $2 || '%'))
           AND ($3::uuid IS NULL OR b.id < $3)
         ORDER BY b.id DESC
         LIMIT $1",
    )
    .bind(limit)
    .bind(q)
    .bind(after)
    .fetch_all(db)
    .await?;
    let next_cursor = if rows.len() as i64 == limit {
        rows.last().map(|r| r.id)
    } else {
        None
    };
    Ok(OperatorBindingPage { rows, next_cursor })
}

/// Tenant members, keyset-paginated + searched (email/name/role), mirroring
/// `operator_audit_page` (MAIN-45 AC-2). Keyed on the member's UUID v7
/// `principal_id`; searches only members of `tenant`.
pub async fn tenant_members_page(
    db: &PgPool,
    tenant: TenantId,
    q: Option<String>,
    after: Option<uuid::Uuid>,
    limit: i64,
) -> ApiResult<TenantMemberPage> {
    let limit = limit.clamp(1, 200);
    let q = search_filter(q);
    let rows: Vec<TenantMemberItem> = sqlx::query_as(
        "SELECT m.principal_id, u.email, u.display_name, m.role, m.created_at AS joined_at
         FROM tenant_members m
         JOIN users u ON u.id = m.principal_id
         WHERE m.tenant_id = $1 AND m.principal_type = 'user'
           AND ($3::text IS NULL OR (
                    u.email ILIKE '%' || $3 || '%'
                 OR u.display_name ILIKE '%' || $3 || '%'
                 OR m.role ILIKE '%' || $3 || '%'))
           AND ($4::uuid IS NULL OR m.principal_id < $4)
         ORDER BY m.principal_id DESC
         LIMIT $2",
    )
    .bind(tenant)
    .bind(limit)
    .bind(q)
    .bind(after)
    .fetch_all(db)
    .await?;
    let next_cursor = if rows.len() as i64 == limit {
        rows.last().map(|r| r.principal_id)
    } else {
        None
    };
    Ok(TenantMemberPage { rows, next_cursor })
}

/// Create a session and instruct the node to start it. Shared by the REST
/// handler and the MCP backend. Resolves the checkout path from workspace +
/// node (first match), then delegates to [`create_session_at`].
pub async fn create_session(
    state: &crate::state::AppState,
    tenant: TenantId,
    created_by: Option<UserId>,
    req: CreateSessionRequest,
) -> ApiResult<Session> {
    use crate::error::ApiError;

    // Pin to an explicit checkout (e.g. a worktree) when given, validating it
    // belongs to this workspace on this node. Otherwise use the first checkout.
    // LIMIT 1: a workspace can have several checkouts on one node (worktrees).
    let path: Option<(String,)> = match &req.path {
        Some(p) => {
            sqlx::query_as(
                "SELECT path FROM node_workspaces
             WHERE tenant_id = $1 AND node_id = $2 AND workspace_id = $3 AND path = $4",
            )
            .bind(tenant)
            .bind(req.node_id)
            .bind(req.workspace_id)
            .bind(p)
            .fetch_optional(&state.db)
            .await?
        }
        None => {
            sqlx::query_as(
                "SELECT path FROM node_workspaces
             WHERE tenant_id = $1 AND node_id = $2 AND workspace_id = $3
             ORDER BY discovered_at LIMIT 1",
            )
            .bind(tenant)
            .bind(req.node_id)
            .bind(req.workspace_id)
            .fetch_optional(&state.db)
            .await?
        }
    };
    let Some((workspace_path,)) = path else {
        return Err(ApiError::BadRequest(
            "that workspace has no checkout on that node".into(),
        ));
    };
    create_session_at(
        state,
        tenant,
        created_by,
        req.workspace_id,
        req.node_id,
        &req.runtime,
        req.name,
        &workspace_path,
    )
    .await
}

/// Create a session pinned to an explicit checkout path — used by the kanban
/// "start work" flow so the session runs in the freshly-created worktree.
#[allow(clippy::too_many_arguments)]
pub async fn create_session_at(
    state: &crate::state::AppState,
    tenant: TenantId,
    created_by: Option<UserId>,
    workspace_id: WorkspaceId,
    node_id: NodeId,
    runtime: &str,
    name: Option<String>,
    workspace_path: &str,
) -> ApiResult<Session> {
    use crate::error::ApiError;

    if !state.registry.node_online(node_id) {
        return Err(ApiError::BadRequest("node is offline".into()));
    }
    let name = name.unwrap_or_else(|| format!("{runtime} session"));
    let session: Session = sqlx::query_as(
        "INSERT INTO sessions (id, tenant_id, workspace_id, node_id, name, runtime, status, created_by)
         VALUES ($1, $2, $3, $4, $5, $6, 'starting', $7) RETURNING *",
    )
    .bind(SessionId::new())
    .bind(tenant)
    .bind(workspace_id)
    .bind(node_id)
    .bind(&name)
    .bind(runtime)
    .bind(created_by)
    .fetch_one(&state.db)
    .await?;

    let sent = state.registry.send_to_node(
        node_id,
        nook_proto::ControlToNode::StartSession {
            session_id: session.id,
            runtime: runtime.to_string(),
            workspace_path: workspace_path.to_string(),
            cols: 120,
            rows: 32,
        },
    );
    if !sent {
        sqlx::query("UPDATE sessions SET status = 'error', updated_at = now() WHERE id = $1")
            .bind(session.id)
            .execute(&state.db)
            .await?;
        return Err(ApiError::BadRequest("node went offline".into()));
    }

    crate::events::record(
        state,
        tenant,
        crate::events::EventDraft::new("session.created")
            .workspace(workspace_id)
            .node(node_id)
            .session(session.id)
            .payload(serde_json::json!({ "runtime": runtime, "name": name })),
    )
    .await;

    Ok(session)
}

/// Open an ad-hoc terminal: a session with no workspace, run in the node's home
/// directory. An empty `workspace_path` is the wire signal for "home" — the node
/// resolves it to `$HOME` before starting the shell.
pub async fn create_ad_hoc_session(
    state: &crate::state::AppState,
    tenant: TenantId,
    created_by: Option<UserId>,
    node_id: NodeId,
    runtime: &str,
    name: Option<String>,
) -> ApiResult<Session> {
    use crate::error::ApiError;

    if !state.registry.node_online(node_id) {
        return Err(ApiError::BadRequest("node is offline".into()));
    }
    let name = name.unwrap_or_else(|| format!("{runtime} · terminal"));
    let session: Session = sqlx::query_as(
        "INSERT INTO sessions (id, tenant_id, workspace_id, node_id, name, runtime, status, created_by)
         VALUES ($1, $2, NULL, $3, $4, $5, 'starting', $6) RETURNING *",
    )
    .bind(SessionId::new())
    .bind(tenant)
    .bind(node_id)
    .bind(&name)
    .bind(runtime)
    .bind(created_by)
    .fetch_one(&state.db)
    .await?;

    let sent = state.registry.send_to_node(
        node_id,
        nook_proto::ControlToNode::StartSession {
            session_id: session.id,
            runtime: runtime.to_string(),
            // Empty = the node's home directory. See conn.rs StartSession.
            workspace_path: String::new(),
            cols: 120,
            rows: 32,
        },
    );
    if !sent {
        sqlx::query("UPDATE sessions SET status = 'error', updated_at = now() WHERE id = $1")
            .bind(session.id)
            .execute(&state.db)
            .await?;
        return Err(ApiError::BadRequest("node went offline".into()));
    }

    // No `.workspace(...)`: there isn't one. The node is still recorded, so the
    // activity feed reads "terminal opened on <node>".
    crate::events::record(
        state,
        tenant,
        crate::events::EventDraft::new("session.created")
            .node(node_id)
            .session(session.id)
            .payload(serde_json::json!({ "runtime": runtime, "name": name, "ad_hoc": true })),
    )
    .await;

    Ok(session)
}

pub async fn list_notes(
    db: &PgPool,
    tenant: TenantId,
    workspace: WorkspaceId,
) -> ApiResult<Vec<Note>> {
    Ok(sqlx::query_as(
        "SELECT * FROM notes WHERE tenant_id = $1 AND workspace_id = $2 ORDER BY updated_at DESC",
    )
    .bind(tenant)
    .bind(workspace)
    .fetch_all(db)
    .await?)
}

pub async fn create_note(
    db: &PgPool,
    tenant: TenantId,
    workspace: WorkspaceId,
    req: CreateNoteRequest,
) -> ApiResult<Note> {
    Ok(sqlx::query_as(
        "INSERT INTO notes (id, tenant_id, workspace_id, title, content_md, kind)
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING *",
    )
    .bind(NoteId::new())
    .bind(tenant)
    .bind(workspace)
    .bind(req.title.unwrap_or_else(|| "Rolling notes".into()))
    .bind(&req.content_md)
    .bind(req.kind.unwrap_or_else(|| "rolling".into()))
    .fetch_one(db)
    .await?)
}

/// DB-backed tests for the audit paging/search query. They self-provision the
/// schema and no-op without `NOOK_REQUIRE_DB=1`, matching the suite convention.
#[cfg(test)]
mod db_tests {
    use super::{
        operator_audit_page, operator_bindings_page, operator_nodes_page, operator_tenants_page,
        tenant_members_page,
    };
    use nook_types::{EventId, NodeId, TenantId};
    use sqlx::postgres::PgPoolOptions;
    use sqlx::PgPool;
    use uuid::Uuid;

    async fn pool() -> Option<PgPool> {
        if std::env::var("NOOK_REQUIRE_DB").ok().as_deref() != Some("1") {
            return None;
        }
        let url = std::env::var("DATABASE_URL").ok()?;
        let db = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .ok()?;
        crate::MIGRATOR.run(&db).await.ok()?;
        Some(db)
    }

    async fn tenant(db: &PgPool, slug: &str) -> TenantId {
        // v7 (creation-ordered), matching production `TenantId::new()`, so the
        // keyset `ORDER BY id DESC` walks newest-first as the real endpoints do.
        let id = Uuid::now_v7();
        sqlx::query("INSERT INTO tenants (id, name, slug) VALUES ($1, $2, $3)")
            .bind(id)
            .bind(slug)
            .bind(format!("{slug}-{id}"))
            .execute(db)
            .await
            .unwrap();
        TenantId(id)
    }

    async fn node(db: &PgPool, tenant: TenantId, name: &str, status: &str) -> Uuid {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO nodes (id, tenant_id, name, node_token_hash, platform, status)
             VALUES ($1, $2, $3, $4, 'linux', $5)",
        )
        .bind(id)
        .bind(tenant.0)
        .bind(name)
        // Unique per node — node_token_hash is unique instance-wide.
        .bind(id.to_string())
        .bind(status)
        .execute(db)
        .await
        .unwrap();
        id
    }

    async fn user(db: &PgPool, tenant: TenantId, email: &str) -> Uuid {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO users (id, tenant_id, display_name, email, role)
             VALUES ($1, $2, 'U', $3, 'member')",
        )
        .bind(id)
        .bind(tenant.0)
        .bind(email)
        .execute(db)
        .await
        .unwrap();
        id
    }

    async fn binding(db: &PgPool, subject: Uuid, role_key: &str) -> Uuid {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO role_bindings (id, subject_id, role_key, scope_type)
             VALUES ($1, $2, $3, 'deployment')",
        )
        .bind(id)
        .bind(subject)
        .bind(role_key)
        .execute(db)
        .await
        .unwrap();
        id
    }

    /// Insert one audit-visible event and return its (v7, creation-ordered) id.
    async fn event(db: &PgPool, tenant: TenantId, kind: &str, actor_type: &str) -> EventId {
        let id = EventId::new();
        sqlx::query(
            "INSERT INTO events (id, tenant_id, kind, actor_type, actor_id)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id)
        .bind(tenant.0)
        .bind(kind)
        .bind(actor_type)
        .bind(Uuid::new_v4())
        .execute(db)
        .await
        .unwrap();
        id
    }

    async fn cleanup(db: &PgPool, t: TenantId) {
        // role_bindings have no tenant_id column, so delete them via their
        // subjects first (both role_bindings and tenant_members reference users).
        let _ = sqlx::query(
            "DELETE FROM role_bindings WHERE subject_id IN (SELECT id FROM users WHERE tenant_id = $1)",
        )
        .bind(t.0)
        .execute(db)
        .await;
        for tbl in ["events", "nodes", "tenant_members", "users"] {
            let _ = sqlx::query(&format!("DELETE FROM {tbl} WHERE tenant_id = $1"))
                .bind(t.0)
                .execute(db)
                .await;
        }
        let _ = sqlx::query("DELETE FROM tenants WHERE id = $1")
            .bind(t.0)
            .execute(db)
            .await;
    }

    /// A member: a v7 `users` row (the keyset id) + its `tenant_members` grant.
    async fn member(db: &PgPool, tenant: TenantId, email: &str, name: &str, role: &str) -> Uuid {
        let uid = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO users (id, tenant_id, display_name, email, role)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(uid)
        .bind(tenant.0)
        .bind(name)
        .bind(email)
        .bind(role)
        .execute(db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO tenant_members (id, tenant_id, principal_type, principal_id, role)
             VALUES ($1, $2, 'user', $3, $4)",
        )
        .bind(Uuid::new_v4())
        .bind(tenant.0)
        .bind(uid)
        .bind(role)
        .execute(db)
        .await
        .unwrap();
        uid
    }

    /// AC-2 for members: bounded page + a cursor that walks older rows, and a
    /// search (email/name/role) that reaches a match beyond the first page.
    #[tokio::test]
    async fn member_page_cursors_and_searches() {
        let Some(db) = pool().await else {
            eprintln!("skipping member_page_cursors_and_searches — no DATABASE_URL");
            return;
        };
        let t = tenant(&db, "mem-page").await;
        // The needle is the OLDEST member (smallest v7 id → a later page).
        let needle = member(&db, t, "needle@m.test", "Needle Person", "member").await;
        for i in 0..4 {
            member(
                &db,
                t,
                &format!("f{i}@m.test"),
                &format!("Filler {i}"),
                "member",
            )
            .await;
        }

        let p1 = tenant_members_page(&db, t, None, None, 2).await.unwrap();
        assert!(p1.rows.len() <= 2, "page is bounded");
        assert!(p1.next_cursor.is_some(), "a full page carries a cursor");

        // Search by (distinctive) email/name reaches the needle on a later page.
        let hit = tenant_members_page(&db, t, Some("NEEDLE".into()), None, 2)
            .await
            .unwrap();
        assert!(
            hit.rows.iter().any(|r| r.principal_id == needle),
            "case-insensitive search finds a later-page member"
        );
        assert!(
            hit.rows
                .iter()
                .all(|r| r.email.to_lowercase().contains("needle")
                    || r.display_name.to_lowercase().contains("needle")),
            "non-matching members are excluded"
        );

        // No matches → empty.
        assert!(
            tenant_members_page(&db, t, Some("zzno".into()), None, 50)
                .await
                .unwrap()
                .rows
                .is_empty(),
            "no matches is empty"
        );

        cleanup(&db, t).await;
    }

    /// AC-1/AC-2: pages are bounded, the cursor walks strictly older rows with
    /// no overlap or gap, and the end of the list yields a null cursor.
    #[tokio::test]
    async fn cursor_walks_older_rows_with_no_overlap_or_gap() {
        let Some(db) = pool().await else {
            eprintln!("skipping cursor_walks_older_rows_with_no_overlap_or_gap — no DATABASE_URL");
            return;
        };
        let t = tenant(&db, "audit-page").await;
        // Five events, oldest → newest (v7 ids increase with insertion order).
        let mut ids = Vec::new();
        for _ in 0..5 {
            ids.push(event(&db, t, "operator.audit", "user").await);
        }
        // Newest first is the reverse of insertion order.
        let newest_first: Vec<EventId> = ids.iter().rev().copied().collect();

        // Page 1: the two newest, with a cursor.
        let p1 = operator_audit_page(&db, None, None, 2).await.unwrap();
        // Filter to THIS tenant's rows so a shared dev DB's other events don't
        // perturb the assertions — we only reason about ids we inserted.
        let seen: Vec<EventId> = p1
            .rows
            .iter()
            .map(|r| r.id)
            .filter(|id| ids.contains(id))
            .collect();
        assert!(p1.rows.len() <= 2, "page is bounded by the limit");
        assert!(p1.next_cursor.is_some(), "a full page carries a cursor");

        // Walk pages via the cursor and collect our ids, stopping once we have
        // all of OUR rows — NOT paging the global list to exhaustion (MAIN-93
        // AC-1). `operator_audit_page` is deployment-wide by design (NG-1), so
        // on the shared dev DB the global list is effectively unbounded; our
        // five rows are the newest, so a bounded walk reaches them in the first
        // pages. Walking to `next_cursor == None` tripped the guard once the DB
        // held more than ~40 audit rows.
        let mut collected = Vec::new();
        collected.extend(seen);
        let mut cursor = p1.next_cursor;
        let mut guard = 0;
        while cursor.is_some() && collected.len() < ids.len() {
            let after = cursor.take().unwrap();
            guard += 1;
            assert!(guard < 20, "cursor did not reach our rows");
            let page = operator_audit_page(&db, None, Some(after), 2)
                .await
                .unwrap();
            for r in &page.rows {
                if ids.contains(&r.id) {
                    collected.push(r.id);
                }
            }
            cursor = page.next_cursor;
        }

        // No id appears twice (no overlap) and every id appears (no gap).
        let mut deduped = collected.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(deduped.len(), collected.len(), "no row was returned twice");
        for id in &ids {
            assert!(collected.contains(id), "every inserted row was reached");
        }
        // And the order our ids came back in is newest-first.
        let ours_in_order: Vec<EventId> = collected
            .iter()
            .filter(|id| ids.contains(id))
            .copied()
            .collect();
        assert_eq!(ours_in_order, newest_first, "rows arrive newest-first");

        cleanup(&db, t).await;
    }

    /// AC-2: search filters the WHOLE log — a match that lives beyond the first
    /// page is still returned — and is case-insensitive.
    #[tokio::test]
    async fn search_finds_a_match_beyond_the_first_page() {
        let Some(db) = pool().await else {
            eprintln!("skipping search_finds_a_match_beyond_the_first_page — no DATABASE_URL");
            return;
        };
        let t = tenant(&db, "audit-search").await;
        // The distinctive kind is the OLDEST row, so without server-side search
        // it would sit on a later page.
        let needle = event(&db, t, "node.RevokeD", "node").await;
        for _ in 0..5 {
            event(&db, t, "operator.audit", "user").await;
        }

        // Case-insensitive substring on the kind, small page — the match is not
        // on page one, yet search returns it.
        let hit = operator_audit_page(&db, Some("revoked".into()), None, 2)
            .await
            .unwrap();
        assert!(
            hit.rows.iter().any(|r| r.id == needle),
            "server-side search reached a match beyond the first page"
        );
        // The noise rows do not match the needle.
        assert!(
            hit.rows
                .iter()
                .all(|r| r.kind.to_lowercase().contains("revoked")),
            "search excludes non-matching rows"
        );

        cleanup(&db, t).await;
    }

    /// AC-2: paging one past the end is a clean empty page with a null cursor,
    /// not an error; and a short page (fewer than the limit) has no cursor.
    #[tokio::test]
    async fn end_of_list_is_a_clean_null_cursor() {
        let Some(db) = pool().await else {
            eprintln!("skipping end_of_list_is_a_clean_null_cursor — no DATABASE_URL");
            return;
        };
        let t = tenant(&db, "audit-end").await;
        // A kind unique to this run, so the searched list is EXACTLY our rows —
        // the shared dev DB holds many other `operator.*` events, and searching
        // the common "operator.audit" would fill a 50-row page and never end
        // (MAIN-93 AC-3). It still starts `operator.` so it is an operator event.
        let kind = format!("operator.audit_end_{}", uuid::Uuid::now_v7().simple());
        let only = event(&db, t, &kind, "user").await;

        // Search by that unique token: a list of exactly one row, so the page is
        // short and the cursor is null.
        let page = operator_audit_page(&db, Some(kind.clone()), None, 50)
            .await
            .unwrap();
        assert!(
            page.rows.iter().any(|r| r.id == only),
            "our row is in the page"
        );
        assert_eq!(page.rows.len(), 1, "only our unique-kind row matches");
        assert!(
            (page.rows.len() as i64) < 50,
            "the page did not fill, so there is no next page"
        );
        assert!(page.next_cursor.is_none(), "a short page ends the list");

        // Paging strictly past our row returns no error (empty of our id).
        let past = operator_audit_page(&db, None, Some(only), 50)
            .await
            .unwrap();
        assert!(
            !past.rows.iter().any(|r| r.id == only),
            "the cursor excludes the row it points at"
        );

        cleanup(&db, t).await;
    }

    /// AC-1/AC-2 for tenants: a bounded page + a cursor that walks older rows,
    /// and a slug/name search that reaches a match beyond the first page.
    #[tokio::test]
    async fn tenants_page_cursors_and_searches() {
        let Some(db) = pool().await else {
            eprintln!("skipping tenants_page_cursors_and_searches — no DATABASE_URL");
            return;
        };
        // The needle is created FIRST (oldest, smallest v7 id → a later page).
        let needle = tenant(&db, "zzneedle").await;
        let mut all = vec![needle];
        for i in 0..4 {
            all.push(tenant(&db, &format!("filler{i}")).await);
        }

        // Page 1 is bounded and carries a cursor.
        let p1 = operator_tenants_page(&db, None, None, 2).await.unwrap();
        assert!(p1.rows.len() <= 2, "page is bounded");
        assert!(p1.next_cursor.is_some(), "a full page carries a cursor");

        // Search reaches the needle even though it is not on page 1.
        let hit = operator_tenants_page(&db, Some("ZZNEEDLE".into()), None, 2)
            .await
            .unwrap();
        assert!(
            hit.rows.iter().any(|r| r.id == needle),
            "case-insensitive search finds a later-page match"
        );
        assert!(
            hit.rows.iter().all(|r| r.slug.contains("zzneedle")),
            "non-matching tenants are excluded"
        );

        for t in all.drain(..) {
            cleanup(&db, t).await;
        }
    }

    /// AC-1/AC-2 for nodes: cursor + search on name/status.
    #[tokio::test]
    async fn nodes_page_cursors_and_searches() {
        let Some(db) = pool().await else {
            eprintln!("skipping nodes_page_cursors_and_searches — no DATABASE_URL");
            return;
        };
        let t = tenant(&db, "nodes-host").await;
        let needle = node(&db, t, "edge-oddball", "online").await;
        for i in 0..4 {
            node(&db, t, &format!("worker{i}"), "offline").await;
        }

        let p1 = operator_nodes_page(&db, None, None, 2).await.unwrap();
        assert!(p1.rows.len() <= 2 && p1.next_cursor.is_some());

        // Search by (distinctive) name reaches the needle on a later page.
        let by_name = operator_nodes_page(&db, Some("ODDBALL".into()), None, 2)
            .await
            .unwrap();
        assert!(by_name.rows.iter().any(|r| r.id == NodeId(needle)));
        // Search by status matches the whole set of that status.
        let online = operator_nodes_page(&db, Some("online".into()), None, 50)
            .await
            .unwrap();
        assert!(online.rows.iter().all(|r| r.status == "online"));

        cleanup(&db, t).await;
    }

    /// AC-1/AC-2 for bindings: cursor + search on email/role.
    #[tokio::test]
    async fn bindings_page_cursors_and_searches() {
        let Some(db) = pool().await else {
            eprintln!("skipping bindings_page_cursors_and_searches — no DATABASE_URL");
            return;
        };
        let t = tenant(&db, "bind-host").await;
        // The needle binding is created first (oldest id → later page).
        let subj = user(&db, t, "needle@bind.test").await;
        let needle = binding(&db, subj, "operator").await;
        for i in 0..4 {
            let u = user(&db, t, &format!("filler{i}@bind.test")).await;
            binding(&db, u, "org_admin").await;
        }

        let p1 = operator_bindings_page(&db, None, None, 2).await.unwrap();
        assert!(p1.rows.len() <= 2 && p1.next_cursor.is_some());

        // Search by email reaches the needle beyond page 1.
        let by_email = operator_bindings_page(&db, Some("NEEDLE@".into()), None, 2)
            .await
            .unwrap();
        assert!(by_email.rows.iter().any(|r| r.id == needle));
        // Search by role narrows to that role.
        let operators = operator_bindings_page(&db, Some("operator".into()), None, 50)
            .await
            .unwrap();
        assert!(operators.rows.iter().all(|r| r.role_key == "operator"));

        cleanup(&db, t).await;
    }
}
