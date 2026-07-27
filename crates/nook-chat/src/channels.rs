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
use chrono::{DateTime, Utc};
use nook_types::{ChatChannel, CreateChatChannel, UpdateChatChannel};
use uuid::Uuid;

use crate::{AppState, Caller, ChatError};

#[derive(sqlx::FromRow)]
struct ChannelRow {
    id: Uuid,
    name: String,
    slug: String,
    owner_type: String,
    archived_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
}

impl From<ChannelRow> for ChatChannel {
    fn from(r: ChannelRow) -> Self {
        ChatChannel {
            id: r.id,
            name: r.name,
            slug: r.slug,
            owner_type: r.owner_type,
            archived: r.archived_at.is_some(),
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
    db: &sqlx::PgPool,
    channel_id: Uuid,
    caller: &Caller,
) -> Result<Access, ChatError> {
    let row: Option<(String, Uuid, Option<DateTime<Utc>>)> =
        sqlx::query_as("SELECT owner_type, owner_id, archived_at FROM chat_channels WHERE id = $1")
            .bind(channel_id)
            .fetch_optional(db)
            .await
            .map_err(|_| ChatError::Internal)?;

    let Some((owner_type, owner_id, archived_at)) = row else {
        return Err(ChatError::NotFound);
    };
    let authorized = match owner_type.as_str() {
        "tenant" => owner_id == caller.tenant_id,
        "org" => person_in_org(db, caller.user_id, owner_id).await?,
        _ => false,
    };
    if !authorized {
        return Err(ChatError::Forbidden);
    }
    Ok(Access {
        archived: archived_at.is_some(),
    })
}

/// Does the caller's person belong to any tenant under `org`? Resolves the
/// caller's `person_id` (from their user row) and asks whether that person has a
/// user in any tenant whose `org_id` matches — the cross-tenant membership rule
/// (AC-1). Reaches `public.users`/`public.tenants` via the `chat,public`
/// search_path, like the existing `tenant_role` lookup.
async fn person_in_org(db: &sqlx::PgPool, user_id: Uuid, org: Uuid) -> Result<bool, ChatError> {
    let (ok,): (bool,) = sqlx::query_as(
        "SELECT EXISTS(
             SELECT 1 FROM public.users u
             JOIN public.tenants t ON t.id = u.tenant_id
             WHERE t.org_id = $2
               AND u.person_id = (SELECT person_id FROM public.users WHERE id = $1)
         )",
    )
    .bind(user_id)
    .bind(org)
    .fetch_one(db)
    .await
    .map_err(|_| ChatError::Internal)?;
    Ok(ok)
}

/// The org a tenant belongs to (`tenants.org_id`).
async fn org_of(db: &sqlx::PgPool, tenant: Uuid) -> Result<Uuid, ChatError> {
    let (org,): (Uuid,) = sqlx::query_as("SELECT org_id FROM public.tenants WHERE id = $1")
        .bind(tenant)
        .fetch_one(db)
        .await
        .map_err(|_| ChatError::Internal)?;
    Ok(org)
}

pub async fn create(
    State(state): State<AppState>,
    caller: Caller,
    Json(req): Json<CreateChatChannel>,
) -> Result<(StatusCode, Json<ChatChannel>), ChatError> {
    // Channel management is owner/admin only (AC-5). For an org channel the same
    // tenant owner/admin check is evaluated in the caller's own tenant — being an
    // admin of any tenant in the org is enough (AC-3); no org-level role exists.
    crate::require_admin(&state.db, &caller).await?;
    let name = req.name.trim();
    if name.is_empty() {
        return Err(ChatError::BadRequest("a channel needs a name".into()));
    }
    let slug = slugify(name);
    // Default is a tenant channel (unchanged); `owner: "org"` makes an org
    // channel owned by the caller's tenant's org (MAIN-112 AC-3).
    let (owner_type, owner_id) = match req.owner.as_deref().unwrap_or("tenant") {
        "tenant" => ("tenant", caller.tenant_id),
        "org" => ("org", org_of(&state.db, caller.tenant_id).await?),
        other => {
            return Err(ChatError::BadRequest(format!(
                "channel owner must be \"tenant\" or \"org\" (got {other:?})"
            )))
        }
    };
    let row = sqlx::query_as::<_, ChannelRow>(
        "INSERT INTO chat_channels (id, owner_type, owner_id, name, slug)
         VALUES ($1, $2, $3, $4, $5)
         RETURNING id, name, slug, owner_type, archived_at, created_at",
    )
    .bind(Uuid::now_v7())
    .bind(owner_type)
    .bind(owner_id)
    .bind(name)
    .bind(&slug)
    .fetch_one(&state.db)
    .await
    .map_err(|e| {
        if is_unique_violation(&e) {
            ChatError::Conflict("a channel with that name already exists".into())
        } else {
            ChatError::Internal
        }
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
) -> Result<Json<Vec<ChatChannel>>, ChatError> {
    // Archived channels drop out of the default list but keep their history
    // (AC-1); the management modal opts them back in with `include_archived`
    // (AC-6). Either way the caller only ever sees their own tenant's (AC-5).
    // The caller's own tenant channels, plus the channels of the org their
    // tenant belongs to — one org per session, so an org channel appears once
    // (AC-1, AC-2). Cross-org isolation holds: only this tenant's org matches.
    let rows = sqlx::query_as::<_, ChannelRow>(
        "SELECT id, name, slug, owner_type, archived_at, created_at FROM chat_channels
         WHERE (
                 (owner_type = 'tenant' AND owner_id = $1)
              OR (owner_type = 'org' AND owner_id = (SELECT org_id FROM public.tenants WHERE id = $1))
               )
           AND ($2 OR archived_at IS NULL)
         ORDER BY created_at",
    )
    .bind(caller.tenant_id)
    .bind(q.include_archived)
    .fetch_all(&state.db)
    .await
    .map_err(|_| ChatError::Internal)?;
    Ok(Json(rows.into_iter().map(Into::into).collect()))
}

pub async fn update(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateChatChannel>,
) -> Result<Json<ChatChannel>, ChatError> {
    // Channel management is owner/admin only (AC-5); for an org channel this is
    // the tenant admin check in the caller's own tenant (AC-3).
    crate::require_admin(&state.db, &caller).await?;
    // Scope check next: `access` refuses a channel the caller has no claim to —
    // another tenant's, or an org channel outside the caller's org — with a 403
    // (AC-5) rather than a silent no-op update. It admits both owner models.
    access(&state.db, id, &caller).await?;

    if let Some(name) = req.name.as_deref() {
        if name.trim().is_empty() {
            return Err(ChatError::BadRequest(
                "a channel name cannot be blank".into(),
            ));
        }
    }

    // `$3` says "archived was supplied"; when it was, `$4` sets archived_at to
    // now (archive) or NULL (restore). name is COALESCEd so an absent name is
    // left untouched. `access` already scoped the row, so the id alone is safe.
    let row = sqlx::query_as::<_, ChannelRow>(
        "UPDATE chat_channels
         SET name = COALESCE($2, name),
             archived_at = CASE
                 WHEN $3 THEN (CASE WHEN $4 THEN now() ELSE NULL END)
                 ELSE archived_at
             END
         WHERE id = $1
         RETURNING id, name, slug, owner_type, archived_at, created_at",
    )
    .bind(id)
    .bind(req.name.as_deref().map(str::trim))
    .bind(req.archived.is_some())
    .bind(req.archived.unwrap_or(false))
    .fetch_optional(&state.db)
    .await
    .map_err(|_| ChatError::Internal)?
    .ok_or(ChatError::NotFound)?;
    Ok(Json(row.into()))
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.code().as_deref() == Some("23505"))
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
    use super::{access, create, list, slugify, update, ListQuery};
    use crate::{AppState, Caller, ChatError};
    use axum::extract::{Path, Query, State};
    use axum::Json;
    use nook_types::{CreateChatChannel, UpdateChatChannel};
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
    use sqlx::PgPool;
    use std::str::FromStr;
    use std::sync::Arc;
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

    async fn pool(url: &str, search_path: &str) -> PgPool {
        let opts = PgConnectOptions::from_str(url)
            .unwrap()
            .options([("search_path", search_path)]);
        PgPoolOptions::new()
            .max_connections(2)
            .connect_with(opts)
            .await
            .unwrap()
    }

    /// The service's pool, with the `public` auth/user tables and the `chat`
    /// schema both provisioned — the same bootstrap the search_path regression
    /// test uses, extended to run chat's own migrations for `chat_channels`.
    async fn setup() -> Option<AppState> {
        if std::env::var("NOOK_REQUIRE_DB").ok().as_deref() != Some("1") {
            eprintln!("skipping channel-admin test — no NOOK_REQUIRE_DB");
            return None;
        }
        let url = std::env::var("DATABASE_URL").ok()?;
        let bootstrap = pool(&url, "public").await;
        crate::ensure_chat_schema(&bootstrap).await.unwrap();
        nook_control::MIGRATOR.run(&bootstrap).await.unwrap();
        let db = pool(&url, "chat,public").await;
        crate::MIGRATOR.run(&db).await.unwrap();
        Some(AppState {
            db,
            registry: Arc::new(crate::registry::Registry::new()),
        })
    }

    async fn new_tenant(db: &PgPool) -> Uuid {
        let id = Uuid::now_v7();
        sqlx::query("INSERT INTO public.tenants (id, name, slug) VALUES ($1, $2, $2)")
            .bind(id)
            .bind(format!("t-{}", id.simple()))
            .execute(db)
            .await
            .unwrap();
        id
    }

    async fn add_user(db: &PgPool, tenant: Uuid, role: &str) -> Uuid {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO public.users (id, tenant_id, person_id, display_name, email, role)
             VALUES ($1, $2, gen_random_uuid(), 'U', $3, $4)",
        )
        .bind(id)
        .bind(tenant)
        .bind(format!("u-{}@example.test", id.simple()))
        .bind(role)
        .execute(db)
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

    fn is_forbidden<T>(r: &Result<T, ChatError>) -> bool {
        matches!(r, Err(ChatError::Forbidden))
    }

    async fn cleanup(db: &PgPool, tenant: Uuid) {
        let _ = sqlx::query("DELETE FROM chat_channels WHERE owner_id = $1")
            .bind(tenant)
            .execute(db)
            .await;
        let _ = sqlx::query("DELETE FROM public.tenants WHERE id = $1")
            .bind(tenant)
            .execute(db)
            .await;
    }

    // ── Org channels (MAIN-112) ─────────────────────────────────────────────

    async fn new_org(db: &PgPool) -> Uuid {
        let id = Uuid::now_v7();
        sqlx::query("INSERT INTO public.orgs (id, name, slug) VALUES ($1, $2, $2)")
            .bind(id)
            .bind(format!("o-{}", id.simple()))
            .execute(db)
            .await
            .unwrap();
        id
    }

    async fn new_tenant_in_org(db: &PgPool, org: Uuid) -> Uuid {
        let id = Uuid::now_v7();
        sqlx::query("INSERT INTO public.tenants (id, name, slug, org_id) VALUES ($1, $2, $2, $3)")
            .bind(id)
            .bind(format!("t-{}", id.simple()))
            .bind(org)
            .execute(db)
            .await
            .unwrap();
        id
    }

    /// A user for a specific `person` — so the same person can hold users in two
    /// org tenants (the AC-2 dedupe case).
    async fn add_user_person(db: &PgPool, tenant: Uuid, person: Uuid, role: &str) -> Uuid {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO public.users (id, tenant_id, person_id, display_name, email, role)
             VALUES ($1, $2, $3, 'U', $4, $5)",
        )
        .bind(id)
        .bind(tenant)
        .bind(person)
        .bind(format!("u-{}@example.test", id.simple()))
        .bind(role)
        .execute(db)
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
            access(&state.db, ch.id, &caller(member_b, tb))
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
                access(&state.db, ch.id, &caller(outsider, tc)).await,
                Err(ChatError::Forbidden)
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

        // Cleanup: channels owned by either org, then users, tenants, orgs.
        for owner in [org, other_org] {
            let _ = sqlx::query("DELETE FROM chat_channels WHERE owner_id = $1")
                .bind(owner)
                .execute(&state.db)
                .await;
        }
        for t in [ta, tb, tc] {
            let _ = sqlx::query("DELETE FROM public.users WHERE tenant_id = $1")
                .bind(t)
                .execute(&state.db)
                .await;
            let _ = sqlx::query("DELETE FROM public.tenants WHERE id = $1")
                .bind(t)
                .execute(&state.db)
                .await;
        }
        for o in [org, other_org] {
            let _ = sqlx::query("DELETE FROM public.orgs WHERE id = $1")
                .bind(o)
                .execute(&state.db)
                .await;
        }
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
            matches!(blank, Err(ChatError::BadRequest(_))),
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

        cleanup(&state.db, tenant).await;
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

        cleanup(&state.db, tenant).await;
    }
}
