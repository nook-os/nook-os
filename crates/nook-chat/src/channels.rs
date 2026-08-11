//! Channel CRUD (MAIN-49 AC-1, AC-5; org channels MAIN-112).
//!
//! Two owner models on the generic `chat_channels` owner columns:
//! - `owner_type='tenant'` — shared by one tenant (the default), visible to its
//!   members. This is v1's behaviour, unchanged.
//! - `owner_type='org'` — shared across every tenant under an org, visible to a
//!   caller whose *person* belongs to any of those tenants (`tenants.org_id`).
//!
//! Both are enforced in one place — [`access`] — reused by the message and
//! websocket handlers, so message read/post and the WS upgrade inherit org
//! support for free. Orgs stay isolated from each other.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use nook_types::{ChatChannel, ChatChannelPlacement, CreateChatChannel, UpdateChatChannel};
use uuid::Uuid;

use crate::internal;
use crate::repo::channels::{ChannelRow, OwnerScope};
use crate::{AppState, Caller};
use nook_errors::ApiError;

impl From<ChannelRow> for ChatChannel {
    fn from(r: ChannelRow) -> Self {
        ChatChannel {
            id: r.id,
            name: r.name,
            slug: r.slug,
            owner_type: r.owner_type,
            archived: r.archived_at.is_some(),
            category_id: r.category_id,
            position: r.position,
            unread_count: r.unread_count,
            created_at: r.created_at,
        }
    }
}

/// A channel's scope facts, resolved once and reused by every handler that
/// touches a channel by id.
pub struct Access {
    pub archived: bool,
}

/// Resolve a channel to the caller's scope, or refuse. A channel the caller has
/// no claim to is 403 (AC-5, "cross-tenant access is refused"); a channel that
/// does not exist is 404 — kept distinct on purpose.
///
/// Two owner models (MAIN-112):
/// - **tenant** — visible to members of the owning tenant, exactly as before.
/// - **org** — visible to a caller whose *person* belongs to any tenant under
///   the owning org (`tenants.org_id`). The person behind a user is stable
///   across tenants, so one person in two org tenants sees the one channel.
pub async fn access(
    repo: &dyn crate::repo::channels::ChannelRepository,
    channel_id: Uuid,
    caller: &Caller,
) -> Result<Access, ApiError> {
    let owner = repo.owner_of(channel_id).await.map_err(|_| internal())?;

    let Some(owner) = owner else {
        return Err(ApiError::NotFound);
    };
    let (owner_type, owner_id, archived_at) = (owner.owner_type, owner.owner_id, owner.archived_at);
    let authorized = match owner_type.as_str() {
        "tenant" => owner_id == caller.tenant_id,
        "org" => person_in_org(repo, caller.user_id, owner_id).await?,
        // A DM is reachable only by its participants, resolved by person so a
        // participant reaches it from any tenant and a tenant admin who is not a
        // participant is refused (MAIN-113 AC-3).
        "dm" => person_is_participant(repo, channel_id, caller.user_id).await?,
        _ => false,
    };
    if !authorized {
        return Err(ApiError::Forbidden);
    }
    Ok(Access {
        archived: archived_at.is_some(),
    })
}

/// The posting rule, in ONE place: the caller can reach the channel and the
/// channel is not archived (MAIN-528 AC-2). Both the command routes and
/// `messages::post` gate on this, so "who may run a command here" cannot drift
/// from "who may post here" — that is why it is a function and not two copies.
pub async fn require_postable(
    repo: &dyn crate::repo::channels::ChannelRepository,
    channel_id: Uuid,
    caller: &Caller,
) -> Result<(), ApiError> {
    if access(repo, channel_id, caller).await?.archived {
        return Err(ApiError::Conflict("this channel is archived".into()));
    }
    Ok(())
}

/// Does the caller's person belong to any tenant under `org`? Resolves the
/// caller's `person_id` (from their user row) and asks whether that person has a
/// user in any tenant whose `org_id` matches — the cross-tenant membership rule
/// (AC-1). Reaches `users`/`tenants` via the `chat,public`
/// search_path, like the existing `tenant_role` lookup.
async fn person_in_org(
    repo: &dyn crate::repo::channels::ChannelRepository,
    user_id: Uuid,
    org: Uuid,
) -> Result<bool, ApiError> {
    repo.person_in_org(user_id, org)
        .await
        .map_err(|_| internal())
}

/// Is the caller's person a participant of this DM (MAIN-113)? Resolves the
/// caller's `person_id` and checks `chat_channel_participants` — the same
/// person-keyed membership `dms::open` writes.
async fn person_is_participant(
    repo: &dyn crate::repo::channels::ChannelRepository,
    channel_id: Uuid,
    user_id: Uuid,
) -> Result<bool, ApiError> {
    repo.person_is_participant(channel_id, user_id)
        .await
        .map_err(|_| internal())
}

/// The org a tenant belongs to (`tenants.org_id`).
async fn org_of(
    repo: &dyn crate::repo::channels::ChannelRepository,
    tenant: Uuid,
) -> Result<Uuid, ApiError> {
    repo.org_of_tenant(tenant)
        .await
        .map_err(|_| internal())?
        .ok_or(internal())
}

pub async fn create(
    State(state): State<AppState>,
    caller: Caller,
    Json(req): Json<CreateChatChannel>,
) -> Result<(StatusCode, Json<ChatChannel>), ApiError> {
    // Channel management is owner/admin only (AC-5). For an org channel the same
    // tenant owner/admin check is evaluated in the caller's own tenant — being an
    // admin of any tenant in the org is enough (AC-3); no org-level role exists.
    crate::require_admin(&*state.channels, &caller).await?;
    let name = req.name.trim();
    if name.is_empty() {
        return Err(ApiError::BadRequest("a channel needs a name".into()));
    }
    let slug = slugify(name);
    // Default is a tenant channel (unchanged); `owner: "org"` makes an org
    // channel owned by the caller's tenant's org (MAIN-112 AC-3).
    let (owner_type, owner_id) = match req.owner.as_deref().unwrap_or("tenant") {
        "tenant" => ("tenant", caller.tenant_id),
        "org" => ("org", org_of(&*state.channels, caller.tenant_id).await?),
        other => {
            return Err(ApiError::BadRequest(format!(
                "channel owner must be \"tenant\" or \"org\" (got {other:?})"
            )))
        }
    };
    let row = state
        .channels
        .create(
            OwnerScope {
                owner_type,
                owner_id,
            },
            name,
            &slug,
        )
        .await
        .map_err(|e| match e {
            crate::repo::RepoError::Conflict => {
                ApiError::Conflict("a channel with that name already exists".into())
            }
            crate::repo::RepoError::Other => internal(),
        })?;
    Ok((StatusCode::CREATED, Json(row.into())))
}

/// `?include_archived=true` opts the archived channels back in; the default —
/// what the sidebar requests — leaves them out (MAIN-94 AC-6).
#[derive(serde::Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub include_archived: bool,
}

pub async fn list(
    State(state): State<AppState>,
    caller: Caller,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<ChatChannel>>, ApiError> {
    // Archived channels drop out of the default list but keep their history
    // (AC-1); the management modal opts them back in with `include_archived`
    // (AC-6). Either way the caller only ever sees their own tenant's (AC-5).
    // The caller's own tenant channels, plus the channels of the org their
    // tenant belongs to — one org per session, so an org channel appears once
    // (AC-1, AC-2). Cross-org isolation holds: only this tenant's org matches.
    let rows = state
        .channels
        .list(caller.tenant_id, q.include_archived, caller.user_id)
        .await
        .map_err(|_| internal())?;
    Ok(Json(rows.into_iter().map(Into::into).collect()))
}

pub async fn update(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateChatChannel>,
) -> Result<Json<ChatChannel>, ApiError> {
    // Channel management is owner/admin only (AC-5); for an org channel this is
    // the tenant admin check in the caller's own tenant (AC-3).
    crate::require_admin(&*state.channels, &caller).await?;
    // Scope check next: `access` refuses a channel the caller has no claim to —
    // another tenant's, or an org channel outside the caller's org — with a 403
    // (AC-5) rather than a silent no-op update. It admits both owner models.
    access(&*state.channels, id, &caller).await?;

    if let Some(name) = req.name.as_deref() {
        if name.trim().is_empty() {
            return Err(ApiError::BadRequest(
                "a channel name cannot be blank".into(),
            ));
        }
    }

    // `$3` says "archived was supplied"; when it was, `$4` sets archived_at to
    // now (archive) or NULL (restore). name is COALESCEd so an absent name is
    // left untouched. `access` already scoped the row, so the id alone is safe.
    let row = state
        .channels
        .update(
            id,
            req.name.as_deref().map(|s| s.trim().to_owned()),
            req.archived,
        )
        .await
        .map_err(|_| internal())?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(row.into()))
}

/// Set a channel's category and ordering position (MAIN-178 AC-2). Admin-only,
/// and the channel is scope-checked via `access`. A named category must belong to
/// the SAME owner scope as the channel — the FK proves it exists, not that it is
/// this tenant/org's — so a channel can never be filed under a foreign group;
/// `None` un-categorizes it.
pub async fn place(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(req): Json<ChatChannelPlacement>,
) -> Result<Json<ChatChannel>, ApiError> {
    crate::require_admin(&*state.channels, &caller).await?;
    access(&*state.channels, id, &caller).await?;

    if let Some(cat) = req.category_id {
        let same_owner = state
            .channels
            .category_matches_channel(cat, id)
            .await
            .map_err(|_| internal())?;
        if !matches!(same_owner, Some(true)) {
            return Err(ApiError::BadRequest(
                "that category is not in this channel's scope".into(),
            ));
        }
    }

    let row = state
        .channels
        .place(id, req.category_id, req.position)
        .await
        .map_err(|_| internal())?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(row.into()))
}

/// Advance the caller's read cursor for a channel to "now" (MAIN-117 AC-2). The
/// cursor is monotonic: `GREATEST` keeps it from ever moving backward, so a
/// late-arriving or duplicate call can only leave it where it is or ahead — the
/// endpoint is idempotent. `access` scopes it, so this covers tenant, org, and
/// DM channels alike (AC-4), and refuses a channel the caller can't see.
pub async fn mark_read(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    access(&*state.channels, id, &caller).await?;
    state
        .channels
        .mark_read(id, caller.user_id)
        .await
        .map_err(|_| internal())?;
    Ok(StatusCode::NO_CONTENT)
}

/// A URL-safe slug from a channel name: lowercase, `[a-z0-9-]`, collapsed dashes,
/// capped. The `(owner, slug)` uniqueness constraint makes the slug the channel's
/// stable handle within its tenant.
pub fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut pending_dash = false;
    for ch in name.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            out.push(lower);
            pending_dash = false;
        } else {
            pending_dash = true;
        }
    }
    if out.len() > 64 {
        out.truncate(64);
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        out.push_str("channel");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{access, create, list, mark_read, place, slugify, update, ListQuery};
    use crate::{AppState, Caller};
    use axum::extract::{Path, Query, State};
    use axum::Json;
    use chrono::{DateTime, Utc};
    use nook_db::dialect::time_math;
    use nook_db::{params, Db, DbPool};
    use nook_errors::ApiError;
    use nook_types::{
        ChatChannelPlacement, CreateChatCategory, CreateChatChannel, ReorderChatCategories,
        UpdateChatChannel,
    };

    use uuid::Uuid;

    #[test]
    fn slugs_are_lowercase_and_url_safe() {
        assert_eq!(slugify("General"), "general");
        assert_eq!(slugify("  Big Team Chat!!  "), "big-team-chat");
        assert_eq!(slugify("a/b\\c"), "a-b-c");
        assert_eq!(slugify("™™™"), "channel");
        assert!(slugify(&"x".repeat(200)).len() <= 64);
        assert!(!slugify("trailing---").ends_with('-'));
    }

    // ── The admin gate and the archived-list toggle (MAIN-94 AC-2/AC-4/AC-5/
    // AC-6), driven through the real handlers against a live Postgres configured
    // exactly as the service configures its pool. DB-backed; no-ops without
    // NOOK_REQUIRE_DB=1, matching the suite convention.

    /// A PRIVATE database and a wired state on whichever engine the suite runs
    /// (MAIN-165): the service's `chat,public` pool over a bed on Postgres, the
    /// bed's own pool on SQLite. Dropped whole afterwards, which is why nothing
    /// below deletes its own rows. See `crate::testdb`.
    async fn setup() -> Option<crate::testdb::ChatTest> {
        crate::testdb::chat_test("channel-admin test").await
    }

    async fn new_tenant(db: &DbPool) -> Uuid {
        let id = Uuid::now_v7();
        db.exec(
            "INSERT INTO tenants (id, name, slug) VALUES ($1, $2, $2)",
            params![id, format!("t-{}", id.simple())],
        )
        .await
        .unwrap();
        id
    }

    async fn add_user(db: &DbPool, tenant: Uuid, role: &str) -> Uuid {
        let id = Uuid::now_v7();
        // The person id is BOUND rather than `gen_random_uuid()`: that function
        // is Postgres-only, and this test now also runs on SQLite (MAIN-294).
        // Generating it here is the portable form and reads no worse.
        db.exec(
            "INSERT INTO users (id, tenant_id, person_id, display_name, email, role)
             VALUES ($1, $2, $3, 'U', $4, $5)",
            params![
                id,
                tenant,
                Uuid::now_v7(),
                format!("u-{}@example.test", id.simple()),
                role
            ],
        )
        .await
        .unwrap();
        id
    }

    fn caller(user: Uuid, tenant: Uuid) -> Caller {
        Caller {
            user_id: user,
            tenant_id: tenant,
            cookie_session: false,
        }
    }

    fn is_forbidden<T>(r: &Result<T, ApiError>) -> bool {
        matches!(r, Err(ApiError::Forbidden))
    }

    // ── Org channels (MAIN-112) ─────────────────────────────────────────────

    async fn new_org(db: &DbPool) -> Uuid {
        let id = Uuid::now_v7();
        db.exec(
            "INSERT INTO orgs (id, name, slug) VALUES ($1, $2, $2)",
            params![id, format!("o-{}", id.simple())],
        )
        .await
        .unwrap();
        id
    }

    async fn new_tenant_in_org(db: &DbPool, org: Uuid) -> Uuid {
        let id = Uuid::now_v7();
        db.exec(
            "INSERT INTO tenants (id, name, slug, org_id) VALUES ($1, $2, $2, $3)",
            params![id, format!("t-{}", id.simple()), org],
        )
        .await
        .unwrap();
        id
    }

    /// A user for a specific `person` — so the same person can hold users in two
    /// org tenants (the AC-2 dedupe case).
    async fn add_user_person(db: &DbPool, tenant: Uuid, person: Uuid, role: &str) -> Uuid {
        let id = Uuid::now_v7();
        db.exec(
            "INSERT INTO users (id, tenant_id, person_id, display_name, email, role)
             VALUES ($1, $2, $3, 'U', $4, $5)",
            params![
                id,
                tenant,
                person,
                format!("u-{}@example.test", id.simple()),
                role
            ],
        )
        .await
        .unwrap();
        id
    }

    #[tokio::test]
    async fn org_channels_resolve_by_person_across_the_org_and_isolate_outside() {
        let Some(state) = setup().await else { return };
        let org = new_org(&state.db).await;
        let ta = new_tenant_in_org(&state.db, org).await;
        let tb = new_tenant_in_org(&state.db, org).await;
        // One PERSON with a user in both org tenants (AC-2 dedupe).
        let person = Uuid::now_v7();
        let admin_a = add_user_person(&state.db, ta, person, "admin").await;
        let member_b = add_user_person(&state.db, tb, person, "member").await;
        // A fully separate org, tenant, and person.
        let other_org = new_org(&state.db).await;
        let tc = new_tenant_in_org(&state.db, other_org).await;
        let outsider = add_user_person(&state.db, tc, Uuid::now_v7(), "owner").await;

        // An admin in tenant A creates an org channel (AC-3).
        let ch = create(
            State(state.clone()),
            caller(admin_a, ta),
            Json(CreateChatChannel {
                name: "org-wide".into(),
                owner: Some("org".into()),
            }),
        )
        .await
        .expect("admin creates an org channel")
        .1
         .0;
        assert_eq!(ch.owner_type, "org");

        // Visible in tenant A's list, and — as the same person viewing tenant B —
        // exactly once (AC-1, AC-2).
        let from_a = list(
            State(state.clone()),
            caller(admin_a, ta),
            Query(ListQuery {
                include_archived: false,
            }),
        )
        .await
        .unwrap()
        .0;
        assert!(
            from_a.iter().any(|c| c.id == ch.id),
            "org channel visible in the creating tenant"
        );
        let from_b = list(
            State(state.clone()),
            caller(member_b, tb),
            Query(ListQuery {
                include_archived: false,
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(
            from_b.iter().filter(|c| c.id == ch.id).count(),
            1,
            "the same person in the org's other tenant sees it exactly once"
        );

        // Tenant B may access it (so it can read/post) even though it owns no
        // channel — the cross-tenant delivery rule (AC-5).
        assert!(
            access(&*state.channels, ch.id, &caller(member_b, tb))
                .await
                .is_ok(),
            "a member in another org tenant may access the org channel"
        );

        // An unrelated org never sees it, and its id is refused (NG-4 isolation).
        let from_c = list(
            State(state.clone()),
            caller(outsider, tc),
            Query(ListQuery {
                include_archived: false,
            }),
        )
        .await
        .unwrap()
        .0;
        assert!(
            !from_c.iter().any(|c| c.id == ch.id),
            "an unrelated org never lists the channel"
        );
        assert!(
            matches!(
                access(&*state.channels, ch.id, &caller(outsider, tc)).await,
                Err(ApiError::Forbidden)
            ),
            "an unrelated org's access is refused"
        );

        // A non-admin in the org cannot create an org channel (AC-3 gate).
        let denied = create(
            State(state.clone()),
            caller(member_b, tb),
            Json(CreateChatChannel {
                name: "member-attempt".into(),
                owner: Some("org".into()),
            }),
        )
        .await;
        assert!(
            is_forbidden(&denied),
            "a non-admin cannot create an org channel, got {denied:?}"
        );

        state.teardown().await;
    }

    #[tokio::test]
    async fn create_and_update_are_admin_only() {
        let Some(state) = setup().await else { return };
        let tenant = new_tenant(&state.db).await;
        let admin = add_user(&state.db, tenant, "admin").await;
        let member = add_user(&state.db, tenant, "member").await;

        // A member cannot create (AC-5).
        let denied = create(
            State(state.clone()),
            caller(member, tenant),
            Json(CreateChatChannel {
                name: "general".into(),
                owner: None,
            }),
        )
        .await;
        assert!(
            is_forbidden(&denied),
            "a member is refused create, got {denied:?}"
        );

        // An admin can, and an empty name is a 400, not a channel (AC-2).
        let blank = create(
            State(state.clone()),
            caller(admin, tenant),
            Json(CreateChatChannel {
                name: "   ".into(),
                owner: None,
            }),
        )
        .await;
        assert!(
            matches!(blank, Err(ApiError::BadRequest(_))),
            "a blank name is rejected, got {blank:?}"
        );
        let ch = create(
            State(state.clone()),
            caller(admin, tenant),
            Json(CreateChatChannel {
                name: "general".into(),
                owner: None,
            }),
        )
        .await
        .expect("admin creates")
        .1
         .0;

        // A member cannot rename it either (AC-5).
        let denied = update(
            State(state.clone()),
            caller(member, tenant),
            Path(ch.id),
            Json(UpdateChatChannel {
                name: Some("renamed".into()),
                archived: None,
            }),
        )
        .await;
        assert!(
            is_forbidden(&denied),
            "a member is refused update, got {denied:?}"
        );

        state.teardown().await;
    }

    #[tokio::test]
    async fn archive_toggles_the_default_list_and_round_trips() {
        let Some(state) = setup().await else { return };
        let tenant = new_tenant(&state.db).await;
        let admin = add_user(&state.db, tenant, "owner").await;

        let keep = create(
            State(state.clone()),
            caller(admin, tenant),
            Json(CreateChatChannel {
                name: "keep".into(),
                owner: None,
            }),
        )
        .await
        .expect("create keep")
        .1
         .0;
        let gone = create(
            State(state.clone()),
            caller(admin, tenant),
            Json(CreateChatChannel {
                name: "gone".into(),
                owner: None,
            }),
        )
        .await
        .expect("create gone")
        .1
         .0;

        // Archive one (AC-4).
        let archived = update(
            State(state.clone()),
            caller(admin, tenant),
            Path(gone.id),
            Json(UpdateChatChannel {
                name: None,
                archived: Some(true),
            }),
        )
        .await
        .expect("archive")
        .0;
        assert!(archived.archived, "the channel reports archived");

        // Default list drops it; include_archived brings it back (AC-6).
        let ids = |v: Vec<nook_types::ChatChannel>| v.into_iter().map(|c| c.id).collect::<Vec<_>>();
        let default = list(
            State(state.clone()),
            caller(admin, tenant),
            Query(ListQuery {
                include_archived: false,
            }),
        )
        .await
        .expect("default list")
        .0;
        let default = ids(default);
        assert!(default.contains(&keep.id), "the active channel is listed");
        assert!(!default.contains(&gone.id), "the archived channel is not");

        let all = list(
            State(state.clone()),
            caller(admin, tenant),
            Query(ListQuery {
                include_archived: true,
            }),
        )
        .await
        .expect("archived-inclusive list")
        .0;
        let all = ids(all);
        assert!(
            all.contains(&keep.id) && all.contains(&gone.id),
            "include_archived returns both"
        );

        // Unarchive restores it to the default list (AC-4).
        let _ = update(
            State(state.clone()),
            caller(admin, tenant),
            Path(gone.id),
            Json(UpdateChatChannel {
                name: None,
                archived: Some(false),
            }),
        )
        .await
        .expect("unarchive");
        let restored = list(
            State(state.clone()),
            caller(admin, tenant),
            Query(ListQuery {
                include_archived: false,
            }),
        )
        .await
        .expect("list after restore")
        .0;
        assert!(
            ids(restored).contains(&gone.id),
            "unarchived channel is back in the default list"
        );

        state.teardown().await;
    }

    // ── Channel categories (MAIN-178) ───────────────────────────────────────

    /// Every category mutation is admin-only; reads are visible to any member
    /// (AC-2/AC-3). A member is refused on create and reorder but can list.
    #[tokio::test]
    async fn categories_are_admin_gated_but_member_visible() {
        let Some(state) = setup().await else { return };
        let tenant = new_tenant(&state.db).await;
        let admin = add_user(&state.db, tenant, "admin").await;
        let member = add_user(&state.db, tenant, "member").await;

        // A member cannot create a category.
        let denied = crate::categories::create(
            State(state.clone()),
            caller(member, tenant),
            Json(CreateChatCategory {
                name: "Team".into(),
                owner: None,
            }),
        )
        .await;
        assert!(is_forbidden(&denied), "member create refused: {denied:?}");

        // An admin can.
        let cat = crate::categories::create(
            State(state.clone()),
            caller(admin, tenant),
            Json(CreateChatCategory {
                name: "Team".into(),
                owner: None,
            }),
        )
        .await
        .expect("admin creates category")
        .1
         .0;

        // A member CAN read the list — the sidebar renders groups for everyone.
        let listed = crate::categories::list(State(state.clone()), caller(member, tenant))
            .await
            .expect("member lists categories")
            .0;
        assert!(
            listed.iter().any(|c| c.id == cat.id),
            "member sees the created category"
        );

        // A member cannot reorder.
        let denied = crate::categories::reorder(
            State(state.clone()),
            caller(member, tenant),
            Json(ReorderChatCategories {
                ordered_ids: vec![cat.id],
            }),
        )
        .await;
        assert!(is_forbidden(&denied), "member reorder refused: {denied:?}");

        state.teardown().await;
    }

    /// Deleting a category un-categorizes its channels — it never deletes them,
    /// and a channel's placement (`category_id`, `position`) round-trips through
    /// list (AC-3).
    #[tokio::test]
    async fn deleting_a_category_uncategorizes_channels_and_placement_round_trips() {
        let Some(state) = setup().await else { return };
        let tenant = new_tenant(&state.db).await;
        let admin = add_user(&state.db, tenant, "owner").await;

        let cat = crate::categories::create(
            State(state.clone()),
            caller(admin, tenant),
            Json(CreateChatCategory {
                name: "Ops".into(),
                owner: None,
            }),
        )
        .await
        .expect("category")
        .1
         .0;
        let ch = create(
            State(state.clone()),
            caller(admin, tenant),
            Json(CreateChatChannel {
                name: "deploys".into(),
                owner: None,
            }),
        )
        .await
        .expect("channel")
        .1
         .0;

        // Placing the channel records its category and position, and a fresh
        // list carries them back (proves listChannels surfaces the new columns).
        let placed = place(
            State(state.clone()),
            caller(admin, tenant),
            Path(ch.id),
            Json(ChatChannelPlacement {
                category_id: Some(cat.id),
                position: 3,
            }),
        )
        .await
        .expect("place")
        .0;
        assert_eq!(placed.category_id, Some(cat.id));
        assert_eq!(placed.position, 3);
        let before = list(
            State(state.clone()),
            caller(admin, tenant),
            Query(ListQuery {
                include_archived: false,
            }),
        )
        .await
        .expect("list before delete")
        .0;
        let row = before
            .iter()
            .find(|c| c.id == ch.id)
            .expect("channel present");
        assert_eq!(row.category_id, Some(cat.id), "list carries category_id");
        assert_eq!(row.position, 3, "list carries position");

        // Deleting the category leaves the channel — just uncategorized.
        let status =
            crate::categories::delete(State(state.clone()), caller(admin, tenant), Path(cat.id))
                .await
                .expect("delete category");
        assert_eq!(status, axum::http::StatusCode::NO_CONTENT);
        let after = list(
            State(state.clone()),
            caller(admin, tenant),
            Query(ListQuery {
                include_archived: false,
            }),
        )
        .await
        .expect("list after delete")
        .0;
        let row = after
            .iter()
            .find(|c| c.id == ch.id)
            .expect("channel survives category delete");
        assert_eq!(
            row.category_id, None,
            "channel is uncategorized after its category is deleted"
        );

        state.teardown().await;
    }

    /// Reorder persists: the new order is reflected by a subsequent list (AC-2).
    #[tokio::test]
    async fn reorder_persists_across_a_fresh_list() {
        let Some(state) = setup().await else { return };
        let tenant = new_tenant(&state.db).await;
        let admin = add_user(&state.db, tenant, "admin").await;

        let mk = |name: &'static str| {
            let state = state.clone();
            async move {
                crate::categories::create(
                    State(state.clone()),
                    caller(admin, tenant),
                    Json(CreateChatCategory {
                        name: name.into(),
                        owner: None,
                    }),
                )
                .await
                .expect("category")
                .1
                 .0
            }
        };
        let a = mk("A").await; // position 0
        let b = mk("B").await; // position 1

        // Flip the order.
        let reordered = crate::categories::reorder(
            State(state.clone()),
            caller(admin, tenant),
            Json(ReorderChatCategories {
                ordered_ids: vec![b.id, a.id],
            }),
        )
        .await
        .expect("reorder")
        .0;
        let order: Vec<_> = reordered.iter().map(|c| c.id).collect();
        assert_eq!(order, vec![b.id, a.id], "reorder response is B, A");

        // A fresh list reflects the persisted order.
        let listed = crate::categories::list(State(state.clone()), caller(admin, tenant))
            .await
            .expect("list")
            .0;
        let persisted: Vec<_> = listed
            .iter()
            .filter(|c| c.id == a.id || c.id == b.id)
            .map(|c| c.id)
            .collect();
        assert_eq!(persisted, vec![b.id, a.id], "list reflects B before A");

        state.teardown().await;
    }

    // ── Unread counts + read cursors (MAIN-117) ─────────────────────────────

    /// Insert a message into a channel, returning its `created_at` so a test can
    /// pin a cursor exactly at or around it.
    async fn post_msg(db: &DbPool, channel: Uuid, author: Uuid, tenant: Uuid) -> DateTime<Utc> {
        let ts = db
            .query_scalar::<DateTime<Utc>>(
                "INSERT INTO chat_messages (id, channel_id, author_id, tenant_id, body)
             VALUES ($1, $2, $3, $4, 'hi') RETURNING created_at",
                params![Uuid::now_v7(), channel, author, tenant],
            )
            .await
            .unwrap();
        ts
    }

    /// Force a read cursor to an exact instant — used to test the strict-`>`
    /// boundary and the monotonic (`GREATEST`) guard directly.
    async fn set_cursor(db: &DbPool, channel: Uuid, user: Uuid, at: DateTime<Utc>) {
        db.exec(
            "INSERT INTO chat_read_cursors (channel_id, user_id, last_read_at)
             VALUES ($1, $2, $3)
             ON CONFLICT (channel_id, user_id) DO UPDATE SET last_read_at = EXCLUDED.last_read_at",
            params![channel, user, at],
        )
        .await
        .unwrap();
    }

    /// The channel's `unread_count` as the list endpoint reports it for `user`.
    async fn unread_of(state: &AppState, user: Uuid, tenant: Uuid, channel: Uuid) -> i64 {
        let rows = list(
            State(state.clone()),
            caller(user, tenant),
            Query(ListQuery {
                include_archived: false,
            }),
        )
        .await
        .expect("list")
        .0;
        rows.into_iter()
            .find(|c| c.id == channel)
            .expect("channel present")
            .unread_count
    }

    /// Unread counts others' messages only, per-user; marking read clears the
    /// caller's count without touching anyone else's (AC-2/AC-3, isolation).
    #[tokio::test]
    async fn unread_excludes_own_and_is_per_user() {
        let Some(state) = setup().await else { return };
        let tenant = new_tenant(&state.db).await;
        let a = add_user(&state.db, tenant, "admin").await;
        let b = add_user(&state.db, tenant, "member").await;
        let ch = create(
            State(state.clone()),
            caller(a, tenant),
            Json(CreateChatChannel {
                name: "general".into(),
                owner: None,
            }),
        )
        .await
        .expect("channel")
        .1
         .0;

        // Two from B, one from A.
        post_msg(&state.db, ch.id, b, tenant).await;
        post_msg(&state.db, ch.id, b, tenant).await;
        post_msg(&state.db, ch.id, a, tenant).await;

        // A sees only B's two (own excluded); B sees only A's one.
        assert_eq!(unread_of(&state, a, tenant, ch.id).await, 2);
        assert_eq!(unread_of(&state, b, tenant, ch.id).await, 1);

        // A marks read → A is caught up; B is untouched (per-user isolation).
        assert_eq!(
            mark_read(State(state.clone()), caller(a, tenant), Path(ch.id))
                .await
                .expect("mark read"),
            axum::http::StatusCode::NO_CONTENT
        );
        assert_eq!(unread_of(&state, a, tenant, ch.id).await, 0);
        assert_eq!(unread_of(&state, b, tenant, ch.id).await, 1);

        state.teardown().await;
    }

    /// The cursor boundary is strict: a message exactly at the cursor is read; a
    /// message strictly after it is unread (AC-2 "cursor boundary exact").
    #[tokio::test]
    async fn unread_boundary_is_strict() {
        let Some(state) = setup().await else { return };
        let tenant = new_tenant(&state.db).await;
        let a = add_user(&state.db, tenant, "admin").await;
        let b = add_user(&state.db, tenant, "member").await;
        let ch = create(
            State(state.clone()),
            caller(a, tenant),
            Json(CreateChatChannel {
                name: "edge".into(),
                owner: None,
            }),
        )
        .await
        .expect("channel")
        .1
         .0;

        let t = post_msg(&state.db, ch.id, b, tenant).await;
        // Cursor exactly at the message instant → that message is already read.
        set_cursor(&state.db, ch.id, a, t).await;
        assert_eq!(unread_of(&state, a, tenant, ch.id).await, 0);

        // A message strictly after the cursor counts.
        post_msg(&state.db, ch.id, b, tenant).await;
        assert_eq!(unread_of(&state, a, tenant, ch.id).await, 1);

        state.teardown().await;
    }

    /// The cursor never moves backward: with the cursor pinned in the future,
    /// marking read at `now()` leaves it in the future (GREATEST), so messages
    /// created after `now()` but before that future instant stay read (AC-2
    /// idempotent/monotonic).
    #[tokio::test]
    async fn cursor_is_monotonic() {
        let Some(state) = setup().await else { return };
        let tenant = new_tenant(&state.db).await;
        let a = add_user(&state.db, tenant, "admin").await;
        let b = add_user(&state.db, tenant, "member").await;
        let ch = create(
            State(state.clone()),
            caller(a, tenant),
            Json(CreateChatChannel {
                name: "future".into(),
                owner: None,
            }),
        )
        .await
        .expect("channel")
        .1
         .0;

        // Pin A's cursor an hour ahead.
        let future = state
            .db
            .query_scalar::<DateTime<Utc>>(
                &format!("SELECT {}", time_math(state.db.engine()).now_plus("1 hour")),
                params![],
            )
            .await
            .unwrap();
        set_cursor(&state.db, ch.id, a, future).await;

        // A message now (before the future cursor) is read.
        post_msg(&state.db, ch.id, b, tenant).await;
        assert_eq!(unread_of(&state, a, tenant, ch.id).await, 0);

        // Marking read at now() must not drag the cursor back to now(): a further
        // message, still before the future instant, remains read.
        mark_read(State(state.clone()), caller(a, tenant), Path(ch.id))
            .await
            .expect("mark read");
        post_msg(&state.db, ch.id, b, tenant).await;
        assert_eq!(
            unread_of(&state, a, tenant, ch.id).await,
            0,
            "cursor stayed in the future; it did not regress to the present"
        );

        state.teardown().await;
    }

    /// The per-user stream's authorization gate (MAIN-117 AC-6). Each firehose
    /// event is forwarded only if `access` authorizes the caller for its channel
    /// AT THAT MOMENT, so the boundary is fully dynamic: a cross-tenant intruder
    /// never passes, a member ADDED mid-session begins passing (no reconnect), and
    /// a member REMOVED stops passing immediately.
    #[tokio::test]
    async fn stream_gate_isolates_and_tracks_membership_changes() {
        let Some(state) = setup().await else { return };
        let org = new_org(&state.db).await;
        let t_a = new_tenant_in_org(&state.db, org).await;
        let t_b = new_tenant(&state.db).await; // a separate org — the intruder

        let person_a = Uuid::now_v7();
        let a = add_user_person(&state.db, t_a, person_a, "admin").await;
        let person_c = Uuid::now_v7();
        let c = add_user_person(&state.db, t_a, person_c, "member").await;
        let b = add_user(&state.db, t_b, "admin").await;

        let ch_a = create(
            State(state.clone()),
            caller(a, t_a),
            Json(CreateChatChannel {
                name: "team-a".into(),
                owner: None,
            }),
        )
        .await
        .expect("channel a")
        .1
         .0;

        // A DM created by person_a; person_c is NOT a participant yet.
        let dm = Uuid::now_v7();
        state
            .db
            .exec(
                "INSERT INTO chat_channels (id, owner_type, owner_id, name, slug)
             VALUES ($1, 'dm', $2, '', $3)",
                params![dm, person_a, format!("dm-{}", dm.simple())],
            )
            .await
            .unwrap();
        state
            .db
            .exec(
                "INSERT INTO chat_channel_participants (channel_id, person_id) VALUES ($1, $2)",
                params![dm, person_a],
            )
            .await
            .unwrap();

        // Isolation: the tenant-B intruder is refused A's tenant channel and DM,
        // so an event on either is never forwarded to them.
        assert!(is_forbidden(
            &access(&*state.channels, ch_a.id, &caller(b, t_b)).await
        ));
        assert!(is_forbidden(
            &access(&*state.channels, dm, &caller(b, t_b)).await
        ));
        // A, a member, is authorized for both.
        assert!(access(&*state.channels, ch_a.id, &caller(a, t_a))
            .await
            .is_ok());
        assert!(access(&*state.channels, dm, &caller(a, t_a)).await.is_ok());

        // ADD: C is not yet in the DM → refused. Add the participant → now passes,
        // so C's already-open stream starts delivering it without a reconnect.
        assert!(
            is_forbidden(&access(&*state.channels, dm, &caller(c, t_a)).await),
            "C is refused before being added"
        );
        state
            .db
            .exec(
                "INSERT INTO chat_channel_participants (channel_id, person_id) VALUES ($1, $2)",
                params![dm, person_c],
            )
            .await
            .unwrap();
        assert!(
            access(&*state.channels, dm, &caller(c, t_a)).await.is_ok(),
            "a participant added mid-session begins receiving"
        );

        // REMOVE: drop C again → refused immediately.
        state
            .db
            .exec(
                "DELETE FROM chat_channel_participants WHERE channel_id = $1 AND person_id = $2",
                params![dm, person_c],
            )
            .await
            .unwrap();
        assert!(
            is_forbidden(&access(&*state.channels, dm, &caller(c, t_a)).await),
            "a removed participant stops receiving"
        );

        state.teardown().await;
    }
}

/// Scope rules against an in-memory [`FakeChannelRepository`] — no database
/// (MAIN-257 AC-3).
///
/// These are the same assertions the DB-backed tests above make about
/// [`access`], rewritten to run off the fake. They are not a replacement for
/// those: the DB tests still prove the SQL means what the fake claims. What
/// they add is that the *branching in `access`* — tenant, org, DM, missing — is
/// covered without a database anywhere in the loop.
#[cfg(test)]
mod fake_tests {
    use super::*;
    use crate::repo::fakes::FakeChannelRepository;

    fn caller(user: Uuid, tenant: Uuid) -> Caller {
        Caller {
            user_id: user,
            tenant_id: tenant,
            cookie_session: false,
        }
    }

    #[tokio::test]
    async fn a_tenant_channel_is_reachable_only_from_its_own_tenant() {
        let (ta, tb) = (Uuid::now_v7(), Uuid::now_v7());
        let (ua, ub) = (Uuid::now_v7(), Uuid::now_v7());
        let ch = Uuid::now_v7();
        let repo = FakeChannelRepository::new()
            .with_user(ua, Uuid::now_v7(), ta, "member")
            .with_user(ub, Uuid::now_v7(), tb, "owner")
            .with_channel(ch, "tenant", ta);

        assert!(access(&repo, ch, &caller(ua, ta)).await.is_ok());
        assert!(
            matches!(
                access(&repo, ch, &caller(ub, tb)).await,
                Err(ApiError::Forbidden)
            ),
            "another tenant is refused, even its owner"
        );
    }

    #[tokio::test]
    async fn an_org_channel_reaches_every_tenant_under_that_org() {
        let org = Uuid::now_v7();
        let (ta, tb, tc) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let (ua, ub, uc) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let ch = Uuid::now_v7();
        let repo = FakeChannelRepository::new()
            .with_tenant_in_org(ta, org)
            .with_tenant_in_org(tb, org)
            .with_tenant_in_org(tc, Uuid::now_v7())
            .with_user(ua, Uuid::now_v7(), ta, "owner")
            .with_user(ub, Uuid::now_v7(), tb, "member")
            .with_user(uc, Uuid::now_v7(), tc, "owner")
            .with_channel(ch, "org", org);

        assert!(access(&repo, ch, &caller(ua, ta)).await.is_ok());
        assert!(
            access(&repo, ch, &caller(ub, tb)).await.is_ok(),
            "a member of the org's other tenant reaches it"
        );
        assert!(
            matches!(
                access(&repo, ch, &caller(uc, tc)).await,
                Err(ApiError::Forbidden)
            ),
            "an unrelated org does not"
        );
    }

    #[tokio::test]
    async fn a_dm_is_reachable_only_by_its_participants() {
        let tenant = Uuid::now_v7();
        let (p1, p2) = (Uuid::now_v7(), Uuid::now_v7());
        let (u1, u2, admin) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let dm = Uuid::now_v7();
        let repo = FakeChannelRepository::new()
            .with_user(u1, p1, tenant, "member")
            .with_user(u2, p2, tenant, "member")
            .with_user(admin, Uuid::now_v7(), tenant, "owner")
            .with_dm(dm, &[p1, p2]);

        assert!(access(&repo, dm, &caller(u1, tenant)).await.is_ok());
        assert!(access(&repo, dm, &caller(u2, tenant)).await.is_ok());
        assert!(
            matches!(
                access(&repo, dm, &caller(admin, tenant)).await,
                Err(ApiError::Forbidden)
            ),
            "a tenant owner who is not a participant has no special access"
        );
    }

    #[tokio::test]
    async fn a_missing_channel_is_not_found_and_an_archived_one_is_flagged() {
        let tenant = Uuid::now_v7();
        let user = Uuid::now_v7();
        let (live, archived) = (Uuid::now_v7(), Uuid::now_v7());
        let repo = FakeChannelRepository::new()
            .with_user(user, Uuid::now_v7(), tenant, "member")
            .with_channel(live, "tenant", tenant)
            .with_archived_channel(archived, "tenant", tenant);

        assert!(
            matches!(
                access(&repo, Uuid::now_v7(), &caller(user, tenant)).await,
                Err(ApiError::NotFound)
            ),
            "a channel that does not exist is 404, never 403"
        );
        assert!(
            !access(&repo, live, &caller(user, tenant))
                .await
                .unwrap()
                .archived
        );
        assert!(
            access(&repo, archived, &caller(user, tenant))
                .await
                .unwrap()
                .archived,
            "history stays readable, but the caller learns it is archived"
        );
    }

    #[tokio::test]
    async fn require_admin_accepts_owner_and_admin_only() {
        let tenant = Uuid::now_v7();
        let (owner, admin, member) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let repo = FakeChannelRepository::new()
            .with_user(owner, Uuid::now_v7(), tenant, "owner")
            .with_user(admin, Uuid::now_v7(), tenant, "admin")
            .with_user(member, Uuid::now_v7(), tenant, "member");

        assert!(crate::require_admin(&repo, &caller(owner, tenant))
            .await
            .is_ok());
        assert!(crate::require_admin(&repo, &caller(admin, tenant))
            .await
            .is_ok());
        assert!(matches!(
            crate::require_admin(&repo, &caller(member, tenant)).await,
            Err(ApiError::Forbidden)
        ));
    }
}
