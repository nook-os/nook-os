//! Channel CRUD, scoped to the caller's tenant (MAIN-49 AC-1, AC-5).
//!
//! v1 channels are tenant-owned: the generic owner model with
//! `owner_type='tenant'`, `owner_id = the caller's tenant`. A caller only ever
//! sees, updates, posts to, or subscribes to channels of a tenant they belong
//! to — enforced here and reused by the message and websocket handlers via
//! [`access`].

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
    archived_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
}

impl From<ChannelRow> for ChatChannel {
    fn from(r: ChannelRow) -> Self {
        ChatChannel {
            id: r.id,
            name: r.name,
            slug: r.slug,
            archived: r.archived_at.is_some(),
            created_at: r.created_at,
        }
    }
}

/// A channel's tenant-scope facts, resolved once and reused by every handler
/// that touches a channel by id.
pub struct Access {
    pub archived: bool,
}

/// Resolve a channel to the tenant scope, or refuse. A channel of another tenant
/// is 403 (AC-5, "cross-tenant access is refused"); a channel that does not
/// exist at all is 404. The two are kept distinct on purpose — a member of the
/// owning tenant gets a real answer, everyone else gets the same 403 whether or
/// not the channel is archived.
pub async fn access(
    db: &sqlx::PgPool,
    channel_id: Uuid,
    tenant: Uuid,
) -> Result<Access, ChatError> {
    let row: Option<(Uuid, Option<DateTime<Utc>>)> = sqlx::query_as(
        "SELECT owner_id, archived_at FROM chat_channels
         WHERE id = $1 AND owner_type = 'tenant'",
    )
    .bind(channel_id)
    .fetch_optional(db)
    .await
    .map_err(|_| ChatError::Internal)?;

    match row {
        None => Err(ChatError::NotFound),
        Some((owner, _)) if owner != tenant => Err(ChatError::Forbidden),
        Some((_, archived_at)) => Ok(Access {
            archived: archived_at.is_some(),
        }),
    }
}

pub async fn create(
    State(state): State<AppState>,
    caller: Caller,
    Json(req): Json<CreateChatChannel>,
) -> Result<(StatusCode, Json<ChatChannel>), ChatError> {
    // Channel management is owner/admin only (AC-5).
    crate::require_admin(&state.db, &caller).await?;
    let name = req.name.trim();
    if name.is_empty() {
        return Err(ChatError::BadRequest("a channel needs a name".into()));
    }
    let slug = slugify(name);
    let row = sqlx::query_as::<_, ChannelRow>(
        "INSERT INTO chat_channels (id, owner_type, owner_id, name, slug)
         VALUES ($1, 'tenant', $2, $3, $4)
         RETURNING id, name, slug, archived_at, created_at",
    )
    .bind(Uuid::now_v7())
    .bind(caller.tenant_id)
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
    let rows = sqlx::query_as::<_, ChannelRow>(
        "SELECT id, name, slug, archived_at, created_at FROM chat_channels
         WHERE owner_type = 'tenant' AND owner_id = $1
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
    // Channel management is owner/admin only (AC-5).
    crate::require_admin(&state.db, &caller).await?;
    // Scope check next, so another tenant's channel is a clean 403 (AC-5)
    // rather than a silent no-op update.
    access(&state.db, id, caller.tenant_id).await?;

    if let Some(name) = req.name.as_deref() {
        if name.trim().is_empty() {
            return Err(ChatError::BadRequest(
                "a channel name cannot be blank".into(),
            ));
        }
    }

    // `$4` says "archived was supplied"; when it was, `$5` sets archived_at to
    // now (archive) or NULL (restore). name is COALESCEd so an absent name is
    // left untouched.
    let row = sqlx::query_as::<_, ChannelRow>(
        "UPDATE chat_channels
         SET name = COALESCE($3, name),
             archived_at = CASE
                 WHEN $4 THEN (CASE WHEN $5 THEN now() ELSE NULL END)
                 ELSE archived_at
             END
         WHERE id = $1 AND owner_type = 'tenant' AND owner_id = $2
         RETURNING id, name, slug, archived_at, created_at",
    )
    .bind(id)
    .bind(caller.tenant_id)
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
    use super::{create, list, slugify, update, ListQuery};
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
        sqlx::query("CREATE SCHEMA IF NOT EXISTS chat")
            .execute(&bootstrap)
            .await
            .unwrap();
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
            Json(CreateChatChannel { name: "   ".into() }),
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
