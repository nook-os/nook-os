//! Activity/event-feed reads and their scope filter (MAIN-245). Split verbatim
//! out of `services/core.rs`; see that file's header for why.

use chrono::{DateTime, Utc};
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
        read_model: &dyn crate::repo::read_model::ReadModelRepository,
        tenant: TenantId,
        auth: &crate::auth::AuthCtx,
        // The admin check is identity's (MAIN-246), and so are the two `users`
        // reads below (MAIN-304) — `person_id_of` and `sibling_user_ids` already
        // exist there, and a second trait touching `users` would be two places
        // to change when that table does.
        repo: &dyn crate::repo::identity::IdentityRepository,
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
        if auth.is_tenant_admin(repo).await? {
            return Ok(Self::All);
        }
        // A member: their person's user ids, the nodes that person owns, and the
        // sessions those user ids created. Fails closed — a person with no user
        // row resolves to empty sets, which means "see nothing", never "all".
        let Some(person) = repo.person_id_of(auth.user_id).await? else {
            return Ok(Self::Member {
                user_ids: Vec::new(),
                node_ids: Vec::new(),
                session_ids: Vec::new(),
            });
        };
        let user_ids: Vec<Uuid> = repo
            .sibling_user_ids(auth.user_id)
            .await?
            .into_iter()
            .map(|u| u.0)
            .collect();
        let node_ids = read_model.node_ids_owned_by(tenant, person).await?;
        let session_ids = read_model.session_ids_created_by(tenant, &user_ids).await?;
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
    read_model: &dyn crate::repo::read_model::ReadModelRepository,
    tenant: TenantId,
    workspace: Option<WorkspaceId>,
    kind_prefix: Option<String>,
    before: Option<DateTime<Utc>>,
    limit: i64,
    scope: &ActivityScope,
) -> ApiResult<EventsPage> {
    let limit = limit.clamp(1, 200);
    // The scope is handed over as resolved id sets, so the repository binds the
    // same three arrays that `ActivityScope::allows` matches in memory — page
    // and bus stay one rule (MAIN-134).
    let scope_ids = match scope {
        ActivityScope::All => None,
        ActivityScope::Member {
            user_ids,
            node_ids,
            session_ids,
        } => Some(crate::repo::read_model::EventScopeIds {
            user_ids: user_ids.clone(),
            node_ids: node_ids.clone(),
            session_ids: session_ids.clone(),
        }),
    };
    let events = read_model
        .events_page(crate::repo::read_model::EventsQuery {
            tenant,
            workspace,
            kind_prefix,
            before,
            limit,
            scope: scope_ids,
        })
        .await?;
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
