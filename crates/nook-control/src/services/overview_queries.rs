//! The Mission Control overview read-model (MAIN-245).
//!
//! Its own module rather than an aggregate's, deliberately. `overview` joins
//! workspaces, checkouts, sessions, nodes and tasks into one payload, so it
//! belongs to no single aggregate and filing it under one would hand that
//! aggregate's card a query about four others. Keeping it separate is what lets
//! every *other* module map 1:1 onto an aggregate (AC-4).

use chrono::{DateTime, Utc};
use nook_db::{params, Db, DbPool, Postgres, TypeMapping};
use nook_types::*;
use uuid::Uuid;

use crate::error::ApiResult;
use crate::services::session_queries::list_sessions;

/// Mission Control's one aggregate read (MAIN-226): every workspace the caller
/// can see anything of, grouped repo → node → checkout → sessions, in a single
/// round trip.
///
/// `node_owner` scopes checkouts exactly as the nodes list does (`None` = the
/// whole fleet for an owner/admin/node; `Some(person)` = own + shared only);
/// `session_creator` scopes sessions exactly as the sessions list does (MAIN-133).
/// A workspace with no visible checkout AND no visible session is omitted — the
/// view is "where things live and run", and hiding an all-invisible repo leaks
/// nothing the caller could not already see.
pub async fn overview(
    db: &DbPool,
    tenant: TenantId,
    node_owner: Option<Uuid>,
    session_creator: Option<UserId>,
    task_viewer: Option<UserId>,
) -> ApiResult<Overview> {
    use std::collections::HashMap;

    // Repo identity for every workspace in the tenant; the content joins below
    // decide which actually appear.
    let workspaces: Vec<Workspace> = db
        .query_all(
            "SELECT * FROM workspaces WHERE tenant_id = $1 ORDER BY name",
            params![tenant],
        )
        .await?;

    // Visible checkouts, node-scoped identically to `list_nodes`.
    #[derive(sqlx::FromRow)]
    struct CheckoutRow {
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
    let rows: Vec<CheckoutRow> = db
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

    // Active sessions, creator-scoped identically to `list_sessions`.
    //
    // The repositories are built from the pool this read-model already holds
    // rather than injected: `overview` spans five aggregates and belongs to no
    // card yet (see the inline-SQL allow-list), so it still takes a `DbPool`.
    // Whichever card gives it a home should hand it `AppState`'s repositories
    // instead.
    let session_repo = crate::repo::sessions::DbSessionRepository::new(db.clone());
    let workspace_repo = crate::repo::workspaces::DbWorkspaceRepository::new(db.clone());
    let sessions = list_sessions(
        &session_repo,
        &workspace_repo,
        tenant,
        None,
        true,
        session_creator,
    )
    .await?;

    // The ticket each checkout is working (MAIN-230), by the two joins that can
    // know it:
    //
    //   tasks.checkout_id          the durable one (MAIN-225), but NULL until
    //                              discovery scans the fresh worktree;
    //   tasks.session_id → sessions.checkout_id
    //                              the one that covers the gap, so work started
    //                              seconds ago still names its ticket.
    //
    // `task_viewer` is the SAME predicate every other read of a card uses
    // (`tasks::visible_by_cols`, mirrored here in SQL): `None` means the caller
    // already sees the whole tenant. Without it this endpoint would be a
    // side-channel around card visibility — a private ticket's key leaking onto
    // a shared node's row is exactly the class of hole MAIN-226's tests exist to
    // catch.
    #[derive(sqlx::FromRow)]
    struct TaskRow {
        checkout_id: NodeWorkspaceId,
        key: String,
        title: String,
        column_type: String,
    }
    let task_rows: Vec<TaskRow> = db
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
                    AND ({viewer} IS NULL
                         OR t.visibility <> 'private'
                         OR t.created_by = $2
                         OR t.assignee_user_id = $2)
                  ORDER BY key",
                viewer = Postgres.cast("$2", "uuid")
            ),
            params![tenant, task_viewer.map(|u| u.0)],
        )
        .await?;

    // Bucket by checkout, keeping only checkouts this caller can actually see —
    // the node scope above is the authority on that, so a ticket cannot pull an
    // invisible checkout into the payload.
    let mut tasks_by_checkout: HashMap<NodeWorkspaceId, Vec<OverviewTask>> = HashMap::new();
    for t in task_rows {
        tasks_by_checkout
            .entry(t.checkout_id)
            .or_default()
            .push(OverviewTask {
                key: t.key,
                title: t.title,
                column_type: t.column_type,
            });
    }

    // A session binds under a checkout only when that checkout is itself visible;
    // otherwise it falls to its workspace's unbound bucket (or the loose bucket
    // when it has no workspace at all).
    let visible: std::collections::HashSet<NodeWorkspaceId> = rows.iter().map(|r| r.id).collect();
    let mut by_checkout: HashMap<NodeWorkspaceId, Vec<Session>> = HashMap::new();
    let mut unbound: HashMap<WorkspaceId, Vec<Session>> = HashMap::new();
    let mut loose: Vec<Session> = Vec::new();
    for s in sessions {
        match (s.checkout_id, s.workspace_id) {
            (Some(cid), _) if visible.contains(&cid) => by_checkout.entry(cid).or_default().push(s),
            (_, Some(ws)) => unbound.entry(ws).or_default().push(s),
            (_, None) => loose.push(s),
        }
    }

    // Group checkouts under their workspace, attaching their bound sessions.
    let mut checkouts_by_ws: HashMap<WorkspaceId, Vec<OverviewCheckout>> = HashMap::new();
    for r in rows {
        let sessions = by_checkout.remove(&r.id).unwrap_or_default();
        checkouts_by_ws
            .entry(r.workspace_id)
            .or_default()
            .push(OverviewCheckout {
                id: r.id,
                node_id: r.node_id,
                node_name: r.node_name,
                node_status: r.node_status,
                path: r.path,
                branch: r.git_branch,
                kind: r.kind,
                dirty: r
                    .git_status
                    .get("dirty")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                missing_at: r.missing_at,
                sessions,
                tasks: tasks_by_checkout.remove(&r.id).unwrap_or_default(),
            });
    }

    let out = workspaces
        .into_iter()
        .filter_map(|w| {
            let checkouts = checkouts_by_ws.remove(&w.id).unwrap_or_default();
            let unbound_sessions = unbound.remove(&w.id).unwrap_or_default();
            if checkouts.is_empty() && unbound_sessions.is_empty() {
                return None;
            }
            Some(OverviewWorkspace {
                id: w.id,
                name: w.name,
                slug: w.slug,
                git_remote_url: w.git_remote_url,
                git_remote_normalized: w.git_remote_normalized,
                checkouts,
                unbound_sessions,
            })
        })
        .collect();

    Ok(Overview {
        workspaces: out,
        loose_sessions: loose,
    })
}
