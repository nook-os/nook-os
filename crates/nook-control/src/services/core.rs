//! Shared queries used by both REST handlers and MCP tools.

use chrono::{DateTime, Utc};
use nook_db::{params, CiMatch, Db, DbPool, Postgres, TypeMapping};
use nook_types::*;
use uuid::Uuid;

use crate::error::ApiResult;

pub async fn workspace_locations(
    db: &DbPool,
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
    )> = db
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

pub async fn list_workspaces(db: &DbPool, tenant: TenantId) -> ApiResult<Vec<WorkspaceDetail>> {
    let workspaces: Vec<Workspace> = db
        .query_all(
            "SELECT * FROM workspaces WHERE tenant_id = $1 ORDER BY name",
            params![tenant],
        )
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
    db: &DbPool,
    tenant: TenantId,
    id: WorkspaceId,
) -> ApiResult<Option<WorkspaceDetail>> {
    let workspace: Option<Workspace> = db
        .query_opt(
            "SELECT * FROM workspaces WHERE tenant_id = $1 AND id = $2",
            params![tenant, id],
        )
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
/// `owner = Some(person)` returns that person's own nodes PLUS any node the
/// team has been given — those flagged `shared` (MAIN-135); `owner = None`
/// returns the whole fleet (owner/admin, and node tokens whose view is
/// unchanged). Shared grants visibility only — session-start stays owner-only.
pub async fn list_nodes(
    db: &DbPool,
    tenant: TenantId,
    owner: Option<uuid::Uuid>,
) -> ApiResult<Vec<Node>> {
    Ok(db
        .query_all(
            &format!(
                "SELECT id, tenant_id, name, hostname, platform, capabilities, resources, status,
                last_seen_at, owner_person_id, shared, created_at, updated_at
         FROM nodes
         WHERE tenant_id = $1 AND ({owner} IS NULL OR owner_person_id = $2 OR shared)
         ORDER BY name",
                owner = Postgres.cast("$2", "uuid")
            ),
            params![tenant, owner],
        )
        .await?)
}

/// List a tenant's sessions, optionally scoped to a single creator (MAIN-133).
/// `creator = Some(user)` returns only sessions that user started — a member's
/// own view, which naturally excludes `created_by NULL` (legacy/MCP) rows since
/// `NULL = user` is never true. `creator = None` returns all sessions (the
/// owner/admin metadata view, and the unchanged view MCP/dispatcher get). This
/// is the metadata/list layer only; content access stays with `session_guard`.
pub async fn list_sessions(
    db: &DbPool,
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
    let mut binds = params![tenant];
    if let Some(w) = workspace {
        binds.extend(params![w]);
    }
    if let Some(c) = creator {
        binds.extend(params![c]);
    }
    let mut sessions = db.query_all::<Session>(&sql, binds).await?;
    hydrate_checkouts(db, &mut sessions).await?;
    Ok(sessions)
}

/// Fill each session's `checkout` summary from its `checkout_id` (MAIN-222 AC-5)
/// — id, path, branch, kind, and the node it lives on. A small N+1 over a short
/// page of sessions; sessions with no binding (ad-hoc terminals, pruned
/// checkouts) are left `None`.
pub async fn hydrate_checkouts(db: &DbPool, sessions: &mut [Session]) -> ApiResult<()> {
    for s in sessions.iter_mut() {
        let Some(cid) = s.checkout_id else { continue };
        s.checkout = db
            .query_opt::<nook_types::CheckoutSummary>(
                "SELECT nw.id, nw.path, nw.git_branch AS branch, nw.kind, n.name AS node_name
                 FROM node_workspaces nw JOIN nodes n ON n.id = nw.node_id
                 WHERE nw.id = $1",
                params![cid],
            )
            .await?;
    }
    Ok(())
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
        db: &DbPool,
        tenant: TenantId,
        auth: &crate::auth::AuthCtx,
    ) -> ApiResult<Self> {
        // A node credential is not a person watching a feed — unchanged tenant
        // view. Kept ahead of the role check because `is_tenant_admin` reports a
        // node as non-admin, whereas the feed grants a node the full view.
        if !matches!(auth.principal, crate::auth::Principal::User) {
            return Ok(Self::All);
        }
        // An owner/admin sees the whole tenant's activity. Now that MAIN-118's
        // children have merged, this reuses the one shared role check rather
        // than its own copy of the query (MAIN-137).
        if auth.is_tenant_admin(db).await? {
            return Ok(Self::All);
        }
        // A member: their person's user ids, the nodes that person owns, and the
        // sessions those user ids created.
        let person: Uuid = db
            .query_scalar(
                "SELECT person_id FROM users WHERE id = $1",
                params![auth.user_id],
            )
            .await?;
        let user_ids: Vec<Uuid> = db
            .query_scalar_all("SELECT id FROM users WHERE person_id = $1", params![person])
            .await?;
        let node_ids: Vec<Uuid> = db
            .query_scalar_all(
                "SELECT id FROM nodes WHERE tenant_id = $1 AND owner_person_id = $2",
                params![tenant, person],
            )
            .await?;
        let session_ids: Vec<Uuid> = db
            .query_scalar_all(
                "SELECT id FROM sessions WHERE tenant_id = $1 AND created_by = ANY($2)",
                params![tenant, &user_ids[..]],
            )
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
    db: &DbPool,
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
    if matches!(scope, ActivityScope::Member { .. }) {
        sql.push_str(" AND (actor_id = ANY($6) OR node_id = ANY($7) OR session_id = ANY($8))");
    }
    sql.push_str(" ORDER BY occurred_at DESC, id DESC LIMIT $5");
    let mut binds = params![tenant, workspace.map(|w| w.0), kind_prefix, before, limit];
    if let ActivityScope::Member {
        user_ids,
        node_ids,
        session_ids,
    } = scope
    {
        binds.extend(params![&user_ids[..], &node_ids[..], &session_ids[..]]);
    }
    let events: Vec<Event> = db.query_all(&sql, binds).await?;
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
    db: &DbPool,
    q: Option<String>,
    after: Option<EventId>,
    limit: i64,
) -> ApiResult<OperatorAuditPage> {
    let limit = limit.clamp(1, 200);
    // An empty or whitespace-only search is "no filter", not "match the empty
    // string" — the search box clears to that and must show the whole log.
    let q = q.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let term = Postgres.cast("$2", "text");
    let rows: Vec<OperatorAuditEntry> = db
        .query_all(
            &format!(
                "SELECT e.id, e.kind, e.actor_type, e.actor_id, e.tenant_id,
                t.slug AS tenant_slug, e.occurred_at
         FROM events e JOIN tenants t ON t.id = e.tenant_id
         WHERE (e.kind LIKE 'operator.%' OR e.kind LIKE 'rbac.%'
                OR e.kind LIKE 'node.%'  OR e.kind LIKE 'user.%')
           AND ({term} IS NULL OR (
                    {m_kind}
                 OR {m_slug}
                 OR {m_atype}
                 OR {m_aid}))
           AND ({cursor} IS NULL OR e.id < $3)
         ORDER BY e.id DESC
         LIMIT $1",
                cursor = Postgres.cast("$3", "uuid"),
                m_kind = Postgres.ci_match("e.kind", "'%' || $2 || '%'"),
                m_slug = Postgres.ci_match("t.slug", "'%' || $2 || '%'"),
                m_atype = Postgres.ci_match("e.actor_type", "'%' || $2 || '%'"),
                m_aid = Postgres.ci_match(&Postgres.cast("e.actor_id", "text"), "'%' || $2 || '%'")
            ),
            params![limit, q, after.map(|e| e.0)],
        )
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
    db: &DbPool,
    q: Option<String>,
    after: Option<TenantId>,
    limit: i64,
) -> ApiResult<OperatorTenantPage> {
    let limit = limit.clamp(1, 200);
    let q = search_filter(q);
    let term = Postgres.cast("$2", "text");
    let rows: Vec<OperatorTenant> = db
        .query_all(
            &format!(
                "SELECT t.id, t.slug, t.org_id, t.created_at,
                (SELECT count(*) FROM users u WHERE u.tenant_id = t.id)    AS members,
                (SELECT count(*) FROM nodes n WHERE n.tenant_id = t.id)    AS nodes,
                (SELECT count(*) FROM sessions s
                  WHERE s.tenant_id = t.id
                    AND s.status IN ('starting','running','detached'))     AS active_sessions,
                (SELECT count(*) FROM workspaces w WHERE w.tenant_id = t.id) AS workspaces
         FROM tenants t
         WHERE ({term} IS NULL OR {m_slug} OR {m_name})
           AND ({cursor} IS NULL OR t.id < $3)
         ORDER BY t.id DESC
         LIMIT $1",
                cursor = Postgres.cast("$3", "uuid"),
                m_slug = Postgres.ci_match("t.slug", "'%' || $2 || '%'"),
                m_name = Postgres.ci_match("t.name", "'%' || $2 || '%'")
            ),
            params![limit, q, after.map(|t| t.0)],
        )
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
    db: &DbPool,
    q: Option<String>,
    after: Option<NodeId>,
    limit: i64,
) -> ApiResult<OperatorNodePage> {
    let limit = limit.clamp(1, 200);
    let q = search_filter(q);
    let term = Postgres.cast("$2", "text");
    let rows: Vec<OperatorNode> = db
        .query_all(
            &format!(
                "SELECT n.id, n.name, n.platform, n.status, n.last_seen_at, n.resources,
                n.tenant_id, t.slug AS tenant_slug,
                (SELECT count(*) FROM sessions s
                  WHERE s.node_id = n.id
                    AND s.status IN ('starting','running','detached')) AS active_sessions
         FROM nodes n JOIN tenants t ON t.id = n.tenant_id
         WHERE ({term} IS NULL OR (
                    {m_name}
                 OR {m_slug}
                 OR {m_platform}
                 OR {m_status}))
           AND ({cursor} IS NULL OR n.id < $3)
         ORDER BY n.id DESC
         LIMIT $1",
                cursor = Postgres.cast("$3", "uuid"),
                m_name = Postgres.ci_match("n.name", "'%' || $2 || '%'"),
                m_slug = Postgres.ci_match("t.slug", "'%' || $2 || '%'"),
                m_platform = Postgres.ci_match("n.platform", "'%' || $2 || '%'"),
                m_status = Postgres.ci_match("n.status", "'%' || $2 || '%'")
            ),
            params![limit, q, after.map(|n| n.0)],
        )
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
    db: &DbPool,
    q: Option<String>,
    after: Option<uuid::Uuid>,
    limit: i64,
) -> ApiResult<OperatorBindingPage> {
    let limit = limit.clamp(1, 200);
    let q = search_filter(q);
    let term = Postgres.cast("$2", "text");
    let rows: Vec<BindingRow> = db
        .query_all(
            &format!(
                "SELECT b.id, u.email, u.display_name, b.role_key, b.scope_type, b.scope_id,
                COALESCE(o.slug, t.slug) AS scope_label, b.created_at
         FROM role_bindings b
         JOIN users u ON u.id = b.subject_id
         LEFT JOIN orgs o    ON b.scope_type = 'org'    AND o.id = b.scope_id
         LEFT JOIN tenants t ON b.scope_type = 'tenant' AND t.id = b.scope_id
         WHERE ({term} IS NULL OR (
                    {m_email}
                 OR {m_role}
                 OR {m_scope}
                 OR {m_label}))
           AND ({cursor} IS NULL OR b.id < $3)
         ORDER BY b.id DESC
         LIMIT $1",
                cursor = Postgres.cast("$3", "uuid"),
                m_email = Postgres.ci_match("u.email", "'%' || $2 || '%'"),
                m_role = Postgres.ci_match("b.role_key", "'%' || $2 || '%'"),
                m_scope = Postgres.ci_match("b.scope_type", "'%' || $2 || '%'"),
                m_label = Postgres.ci_match("COALESCE(o.slug, t.slug)", "'%' || $2 || '%'")
            ),
            params![limit, q, after],
        )
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
    db: &DbPool,
    tenant: TenantId,
    q: Option<String>,
    after: Option<uuid::Uuid>,
    limit: i64,
) -> ApiResult<TenantMemberPage> {
    let limit = limit.clamp(1, 200);
    let q = search_filter(q);
    let term = Postgres.cast("$3", "text");
    let rows: Vec<TenantMemberItem> = db
        .query_all(
            &format!(
                "SELECT m.principal_id, u.email, u.display_name, m.role, m.created_at AS joined_at
         FROM tenant_members m
         JOIN users u ON u.id = m.principal_id
         WHERE m.tenant_id = $1 AND m.principal_type = 'user'
           AND ({term} IS NULL OR (
                    {m_email}
                 OR {m_name}
                 OR {m_role}))
           AND ({cursor} IS NULL OR m.principal_id < $4)
         ORDER BY m.principal_id DESC
         LIMIT $2",
                cursor = Postgres.cast("$4", "uuid"),
                m_email = Postgres.ci_match("u.email", "'%' || $3 || '%'"),
                m_name = Postgres.ci_match("u.display_name", "'%' || $3 || '%'"),
                m_role = Postgres.ci_match("m.role", "'%' || $3 || '%'")
            ),
            params![tenant, limit, q, after],
        )
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
    let path: Option<String> = match &req.path {
        Some(p) => {
            state
                .db
                .query_scalar_opt(
                    "SELECT path FROM node_workspaces
             WHERE tenant_id = $1 AND node_id = $2 AND workspace_id = $3 AND path = $4
               AND missing_at IS NULL",
                    params![tenant, req.node_id, req.workspace_id, p],
                )
                .await?
        }
        None => {
            state
                .db
                .query_scalar_opt(
                    // MAIN-222 AC-3: the default "the checkout" is the CLONE,
                    // deterministically — never a worktree a delete/reinsert
                    // happened to order first.
                    "SELECT path FROM node_workspaces
             WHERE tenant_id = $1 AND node_id = $2 AND workspace_id = $3
               AND kind = 'clone' AND missing_at IS NULL
             ORDER BY discovered_at LIMIT 1",
                    params![tenant, req.node_id, req.workspace_id],
                )
                .await?
        }
    };
    let Some(workspace_path) = path else {
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

    // MAIN-222 AC-2: bind the session to the exact checkout row its working
    // directory is, so restart can return to it and the UI can show where it
    // runs. Resolved from the path (present rows only) — whatever picked the
    // path, primary clone or a freshly-created worktree; NULL if no row yet
    // (discovery may not have scanned a just-made worktree) or an ad-hoc $HOME.
    let checkout_id: Option<NodeWorkspaceId> = if workspace_path.is_empty() {
        None
    } else {
        state
            .db
            .query_scalar_opt(
                "SELECT id FROM node_workspaces
                 WHERE node_id = $1 AND path = $2 AND missing_at IS NULL",
                params![node_id, workspace_path],
            )
            .await?
    };

    let session: Session = state
        .db
        .query_one(
            "INSERT INTO sessions (id, tenant_id, workspace_id, node_id, name, runtime, status, created_by, checkout_id)
         VALUES ($1, $2, $3, $4, $5, $6, 'starting', $7, $8) RETURNING *",
            params![
                SessionId::new(),
                tenant,
                workspace_id,
                node_id,
                &name,
                runtime,
                created_by.map(|u| u.0),
                checkout_id.map(|c| c.0)
            ],
        )
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
        state
            .db
            .exec(
                &format!(
                    "UPDATE sessions SET status = 'error', updated_at = {} WHERE id = $1",
                    Postgres.now()
                ),
                params![session.id],
            )
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
    let session: Session = state
        .db
        .query_one(
            "INSERT INTO sessions (id, tenant_id, workspace_id, node_id, name, runtime, status, created_by)
         VALUES ($1, $2, NULL, $3, $4, $5, 'starting', $6) RETURNING *",
            params![
                SessionId::new(),
                tenant,
                node_id,
                &name,
                runtime,
                created_by.map(|u| u.0)
            ],
        )
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
        state
            .db
            .exec(
                &format!(
                    "UPDATE sessions SET status = 'error', updated_at = {} WHERE id = $1",
                    Postgres.now()
                ),
                params![session.id],
            )
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

/// Start a runtime's LOGIN flow in a session on a node (MAIN-126). Like an
/// ad-hoc terminal, but the node runs the runtime's allowlisted login command
/// instead of a shell, so the device code/URL renders in the live session view
/// and the flow completes as it would in a terminal. Authorization of WHO may do
/// this is the caller's job (node-scoped); this only launches it.
pub async fn create_auth_session(
    state: &crate::state::AppState,
    tenant: TenantId,
    created_by: Option<UserId>,
    node_id: NodeId,
    runtime: &str,
) -> ApiResult<Session> {
    use crate::error::ApiError;

    if !state.registry.node_online(node_id) {
        return Err(ApiError::BadRequest("node is offline".into()));
    }
    let name = format!("authorize {runtime}");
    let session: Session = state
        .db
        .query_one(
            "INSERT INTO sessions (id, tenant_id, workspace_id, node_id, name, runtime, status, created_by)
         VALUES ($1, $2, NULL, $3, $4, $5, 'starting', $6) RETURNING *",
            params![
                SessionId::new(),
                tenant,
                node_id,
                &name,
                runtime,
                created_by.map(|u| u.0)
            ],
        )
        .await?;

    let sent = state.registry.send_to_node(
        node_id,
        nook_proto::ControlToNode::StartAuthSession {
            session_id: session.id,
            runtime: runtime.to_string(),
            cols: 120,
            rows: 32,
        },
    );
    if !sent {
        state
            .db
            .exec(
                &format!(
                    "UPDATE sessions SET status = 'error', updated_at = {} WHERE id = $1",
                    Postgres.now()
                ),
                params![session.id],
            )
            .await?;
        return Err(ApiError::BadRequest("node went offline".into()));
    }

    crate::events::record(
        state,
        tenant,
        crate::events::EventDraft::new("session.created")
            .node(node_id)
            .session(session.id)
            .payload(serde_json::json!({ "runtime": runtime, "name": name, "authorize": true })),
    )
    .await;

    Ok(session)
}

pub async fn list_notes(
    db: &DbPool,
    tenant: TenantId,
    workspace: WorkspaceId,
) -> ApiResult<Vec<Note>> {
    Ok(db
        .query_all(
            "SELECT * FROM notes WHERE tenant_id = $1 AND workspace_id = $2 ORDER BY updated_at DESC",
            params![tenant, workspace],
        )
        .await?)
}

pub async fn create_note(
    db: &DbPool,
    tenant: TenantId,
    workspace: WorkspaceId,
    req: CreateNoteRequest,
) -> ApiResult<Note> {
    Ok(db
        .query_one(
            "INSERT INTO notes (id, tenant_id, workspace_id, title, content_md, kind)
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING *",
            params![
                NoteId::new(),
                tenant,
                workspace,
                req.title.unwrap_or_else(|| "Rolling notes".into()),
                &req.content_md,
                req.kind.unwrap_or_else(|| "rolling".into())
            ],
        )
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
    use nook_db::{params, Db, DbPool};
    use nook_types::{EventId, NodeId, TenantId};
    use sqlx::postgres::PgPoolOptions;
    use uuid::Uuid;

    async fn pool() -> Option<DbPool> {
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
        Some(nook_db::EnginePool::from_pg(db))
    }

    async fn tenant(db: &DbPool, slug: &str) -> TenantId {
        // v7 (creation-ordered), matching production `TenantId::new()`, so the
        // keyset `ORDER BY id DESC` walks newest-first as the real endpoints do.
        let id = Uuid::now_v7();
        db.exec(
            "INSERT INTO tenants (id, name, slug) VALUES ($1, $2, $3)",
            params![id, slug, format!("{slug}-{id}")],
        )
        .await
        .unwrap();
        TenantId(id)
    }

    async fn node(db: &DbPool, tenant: TenantId, name: &str, status: &str) -> Uuid {
        let id = Uuid::now_v7();
        db.exec(
            "INSERT INTO nodes (id, tenant_id, name, node_token_hash, platform, status)
             VALUES ($1, $2, $3, $4, 'linux', $5)",
            // Token hash unique per node — node_token_hash is unique instance-wide.
            params![id, tenant.0, name, id.to_string(), status],
        )
        .await
        .unwrap();
        id
    }

    async fn user(db: &DbPool, tenant: TenantId, email: &str) -> Uuid {
        let id = Uuid::now_v7();
        db.exec(
            "INSERT INTO users (id, tenant_id, display_name, email, role)
             VALUES ($1, $2, 'U', $3, 'member')",
            params![id, tenant.0, email],
        )
        .await
        .unwrap();
        id
    }

    async fn binding(db: &DbPool, subject: Uuid, role_key: &str) -> Uuid {
        let id = Uuid::now_v7();
        db.exec(
            "INSERT INTO role_bindings (id, subject_id, role_key, scope_type)
             VALUES ($1, $2, $3, 'deployment')",
            params![id, subject, role_key],
        )
        .await
        .unwrap();
        id
    }

    /// Insert one audit-visible event and return its (v7, creation-ordered) id.
    async fn event(db: &DbPool, tenant: TenantId, kind: &str, actor_type: &str) -> EventId {
        let id = EventId::new();
        db.exec(
            "INSERT INTO events (id, tenant_id, kind, actor_type, actor_id)
             VALUES ($1, $2, $3, $4, $5)",
            params![id, tenant.0, kind, actor_type, Uuid::new_v4()],
        )
        .await
        .unwrap();
        id
    }

    async fn cleanup(db: &DbPool, t: TenantId) {
        // role_bindings have no tenant_id column, so delete them via their
        // subjects first (both role_bindings and tenant_members reference users).
        let _ = db
            .exec(
                "DELETE FROM role_bindings WHERE subject_id IN (SELECT id FROM users WHERE tenant_id = $1)",
                params![t.0],
            )
            .await;
        for tbl in ["events", "nodes", "tenant_members", "users"] {
            let _ = db
                .exec(
                    &format!("DELETE FROM {tbl} WHERE tenant_id = $1"),
                    params![t.0],
                )
                .await;
        }
        let _ = db
            .exec("DELETE FROM tenants WHERE id = $1", params![t.0])
            .await;
    }

    /// A member: a v7 `users` row (the keyset id) + its `tenant_members` grant.
    async fn member(db: &DbPool, tenant: TenantId, email: &str, name: &str, role: &str) -> Uuid {
        let uid = Uuid::now_v7();
        db.exec(
            "INSERT INTO users (id, tenant_id, display_name, email, role)
             VALUES ($1, $2, $3, $4, $5)",
            params![uid, tenant.0, name, email, role],
        )
        .await
        .unwrap();
        db.exec(
            "INSERT INTO tenant_members (id, tenant_id, principal_type, principal_id, role)
             VALUES ($1, $2, 'user', $3, $4)",
            params![Uuid::new_v4(), tenant.0, uid, role],
        )
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
