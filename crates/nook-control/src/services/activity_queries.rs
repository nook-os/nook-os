//! Activity/event-feed reads and their scope filter (MAIN-245). Split verbatim
//! out of `services/core.rs`; see that file's header for why.

use chrono::{DateTime, Utc};
use nook_db::{params, Db, DbPool, Postgres, TypeMapping};
use nook_types::*;
use uuid::Uuid;

use crate::error::ApiResult;

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
