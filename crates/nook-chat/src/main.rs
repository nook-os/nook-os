//! `nook-chat` — the team-chat service skeleton (MAIN-48).
//!
//! A separate binary and container from the control plane, because chat's
//! real-time fan-out has a very different load profile. It ships no user-facing
//! chat yet: this is the foundation the messaging and UI tickets build on — a
//! service that boots, checks its health, owns its tables in the shared
//! Postgres (in a dedicated `chat` schema so its migration ledger is isolated),
//! and authenticates callers by REUSING NookOS auth via the shared `nook-auth`
//! crate rather than a second login.

mod bus;
mod categories;
mod channels;
mod config;
mod dms;
mod messages;
mod registry;
mod repo;
#[cfg(test)]
mod testdb;
mod ws;

use std::str::FromStr;
use std::sync::Arc;

use axum::extract::{FromRequestParts, State};
use axum::http::request::Parts;
use axum::routing::{get, patch, post, put};
use axum::{Json, Router};
use axum_extra::extract::CookieJar;
use nook_db::{params, Db, DbPool};
use nook_errors::ApiError;
use serde_json::{json, Value};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use uuid::Uuid;

/// Chat's own migration set, embedded at compile time — applied into the `chat`
/// schema, so it never touches the control plane's `public._sqlx_migrations`.
/// Embedded: 0001_chat_init, 0002_chat_channel_archive, 0003_chat_dm,
/// 0004_chat_threads, 0005_chat_reactions, 0006_chat_categories,
/// 0007_chat_read_cursors.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// The service's own search_path: chat first, `public` behind it.
///
/// `chat` first is what puts chat's tables and its `_sqlx_migrations` in its own
/// schema; `public` behind it is what lets the SHARED `nook-auth` session query —
/// unqualified `sessions_auth`, `tenant_members`, `user_tokens`, which live only
/// in `public` — resolve instead of 500ing.
pub(crate) const CHAT_SEARCH_PATH: &str = "chat,public";

/// Chat's squash manifest (MAIN-235); see `nook_control::SQUASH_MANIFEST`.
static SQUASH_MANIFEST: &str = include_str!("../migrations/squash-manifest.txt");

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) db: DbPool,
    /// Per-channel live fan-out for the delivery websocket (AC-3).
    pub(crate) registry: Arc<registry::Registry>,
    /// Channels, their categories and read cursors (MAIN-257).
    pub(crate) channels: Arc<dyn repo::channels::ChannelRepository>,
    /// Messages, reactions and the revision trail (MAIN-257).
    pub(crate) messages: Arc<dyn repo::messages::MessageRepository>,
    /// Direct messages and the person directory that gates them (MAIN-257).
    pub(crate) dms: Arc<dyn repo::dms::DmRepository>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nook_chat=info,tower_http=info".into()),
        )
        .init();

    // After the subscriber, so the hook's chained default and the layer's
    // structured record both land in the configured log (MAIN-273).
    nook_errors::install_panic_hook();

    let cfg = config::Config::from_env()?;
    tracing::info!(
        bind = %cfg.bind,
        web_origin = %cfg.web_origin,
        public_base_url = %cfg.public_base_url,
        "nook-chat starting",
    );

    // The engine picks the pool, exactly as nook-control's boot does (MAIN-196
    // AC-2, mirrored here by MAIN-294).
    let db = open_pool(&cfg.database_url, 10, CHAT_SEARCH_PATH).await?;
    // The schema must exist before the migrator creates chat._sqlx_migrations.
    // A no-op on SQLite, which has no schemas — see the function.
    ensure_chat_schema(&db).await?;
    // Migrations are POSTGRES-ONLY, and that is the whole shape of MAIN-294.
    //
    // On Postgres, chat owns a schema: the pool's search_path pins `chat` first,
    // so the orphan check reads `chat._sqlx_migrations`, chat's squash manifest
    // collapses chat's own ledger (MAIN-235), dev tolerates a ledger ahead of
    // this checkout and production stays strictly fatal (MAIN-224). Unchanged.
    //
    // On SQLite there are no schemas. One file is one namespace and ONE
    // `_sqlx_migrations`, so a second migrator writing it collides with the
    // control plane's on version numbers — measured as "migration 1 was
    // previously applied but has been modified", a checksum mismatch, which is
    // fatal everywhere. Chat's tables therefore live in the control track's
    // `0001` (they are named `chat_*`, so one namespace is enough for both) and
    // chat runs nothing here: the control plane owns the single ledger.
    //
    // Chat cannot run that track itself even if it wanted to — `nook-control` is
    // a dev-dependency, not a runtime one, and making the chat service depend on
    // the whole control plane to boot would be a far larger change than the one
    // this buys.
    if db.engine() == nook_db::Engine::Postgres {
        nook_db::migrate::run_boot_migrations(&MIGRATOR, &db, cfg.is_production(), SQUASH_MANIFEST)
            .await?;
    } else {
        tracing::info!(
            "sqlite: chat's tables come from the control plane's merged 0001 — no chat migrator to run",
        );
    }

    // Live fan-out: a local per-channel broadcast registry, plus a cross-instance
    // bus (nook-db's event-bus seam — Postgres LISTEN/NOTIFY under the hood) so a
    // post on any instance reaches subscribers on all of them (AC-3). Held in
    // memory — a restart drops subscriptions, and clients reconnect and backfill
    // from history.
    let registry = Arc::new(registry::Registry::new());
    let state = AppState {
        channels: Arc::new(repo::channels::DbChannelRepository::new(db.clone())),
        messages: Arc::new(repo::messages::DbMessageRepository::new(db.clone())),
        dms: Arc::new(repo::dms::DbDmRepository::new(db.clone())),
        db: db.clone(),
        registry: registry.clone(),
    };
    // The listener reads each announced message back through the same
    // repository the handlers use, so a peer's payload is built exactly once.
    bus::start(registry, db, state.messages.clone());
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/livez", get(livez))
        // A minimal authenticated endpoint proving auth reuse (AC-4): it
        // resolves the SAME user+tenant the control plane would.
        .route("/api/me", get(me))
        // Channels (AC-1) + messages (AC-2/AC-4) + delivery websocket (AC-3),
        // all tenant-scoped via the shared auth (AC-5).
        .route("/api/channels", get(channels::list).post(channels::create))
        .route("/api/channels/{id}", patch(channels::update))
        // Channel categories (MAIN-178): member-visible list; admin-only
        // create/rename/delete/reorder, and a channel's category+position.
        .route(
            "/api/categories",
            get(categories::list).post(categories::create),
        )
        .route("/api/categories/reorder", post(categories::reorder))
        .route(
            "/api/categories/{id}",
            patch(categories::update).delete(categories::delete),
        )
        .route("/api/channels/{id}/placement", patch(channels::place))
        // Advance the caller's read cursor for a channel (MAIN-117 AC-2).
        .route("/api/channels/{id}/read", put(channels::mark_read))
        .route(
            "/api/channels/{id}/messages",
            get(messages::history).post(messages::post),
        )
        // A message's thread: the parent plus a keyset page of its replies
        // (MAIN-114 AC-2), authorized on the parent's channel.
        .route("/api/messages/{id}/thread", get(messages::thread))
        // Edit (author) + soft-delete (author or admin) a message (MAIN-116).
        .route(
            "/api/messages/{id}",
            patch(messages::update).delete(messages::delete),
        )
        // Toggle the caller's reaction on a message (MAIN-116 AC-2).
        .route(
            "/api/messages/{id}/reactions/{emoji}",
            put(messages::add_reaction).delete(messages::remove_reaction),
        )
        // ONE per-user live stream for every channel/DM the caller belongs to,
        // tagged by channel_id (MAIN-117 AC-4/AC-6) — replaces the old
        // per-open-channel socket.
        .route("/api/ws", get(ws::stream))
        // Direct messages (MAIN-113): open-or-create + list the caller's DMs,
        // and the org-scoped people picker that feeds the new-DM affordance.
        .route("/api/dms", get(dms::list).post(dms::open))
        .route("/api/people", get(dms::people));
    let app = with_middleware(app).with_state(state);

    let listener = tokio::net::TcpListener::bind(&cfg.bind).await?;
    tracing::info!(bind = %cfg.bind, "nook-chat listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Create the `chat` schema, tolerating the concurrent-creation race.
///
/// `CREATE SCHEMA IF NOT EXISTS` is NOT atomic: two callers — parallel tests, or
/// two chat instances booting at once — can both pass the existence check and
/// then race the `pg_namespace` insert, and the loser gets `23505`
/// (unique_violation) even though `IF NOT EXISTS` was asked for. The schema
/// exists either way, so a duplicate is success, not an error.
/// Chat's pool for `url`, on whichever engine that URL names (MAIN-294).
///
/// **Postgres** keeps the bespoke construction chat has always had, because it
/// needs a `search_path` of `chat,public` that `nook_db::connect` does not set.
/// `chat` is first, so sqlx creates its ledger as `chat._sqlx_migrations` and
/// chat's tables in `chat.*`, independent of the control plane's `public`
/// schema. `public` is the fallback so the SHARED `nook-auth` session query —
/// unqualified `sessions_auth`, `tenant_members`, `user_tokens`, which live only
/// in `public` — resolves instead of 500ing on `chat.sessions_auth does not
/// exist`. No chat table shares a name with a public one, so `chat` winning
/// first is safe.
///
/// **SQLite** has no schemas and no `search_path`, so there is nothing to
/// separate and the ordinary `nook_db::connect` is exactly right. Chat's SQLite
/// track names its tables `chat_*` outright, which is what makes one namespace
/// safe to share with the control plane's.
pub(crate) async fn open_pool(
    url: &str,
    max_connections: u32,
    search_path: &str,
) -> anyhow::Result<nook_db::DbPool> {
    match nook_db::engine_from_url(url)? {
        nook_db::Engine::Postgres => {
            let opts = PgConnectOptions::from_str(url)?.options([("search_path", search_path)]);
            Ok(nook_db::EnginePool::from_pg(
                PgPoolOptions::new()
                    .max_connections(max_connections)
                    .connect_with(opts)
                    .await?,
            ))
        }
        nook_db::Engine::Sqlite => Ok(nook_db::connect(url, max_connections).await?),
    }
}

pub(crate) async fn ensure_chat_schema(pool: &nook_db::DbPool) -> anyhow::Result<()> {
    // SQLite has no schemas: chat's tables are named `chat_*` in the one
    // namespace the file has, so there is nothing to create. Returning early
    // rather than letting `CREATE SCHEMA` fail keeps the caller engine-blind.
    if pool.engine() == nook_db::Engine::Sqlite {
        return Ok(());
    }
    match pool
        .exec("CREATE SCHEMA IF NOT EXISTS chat", params![])
        .await
    {
        Ok(_) => Ok(()),
        // A concurrent `CREATE SCHEMA IF NOT EXISTS` can still lose the
        // `pg_namespace` race and come back 23505. The schema exists either
        // way, so a duplicate is success. Asked through `DbError` now rather
        // than by reaching for the driver's SQLSTATE (MAIN-269).
        Err(e) if e.is_unique_violation() => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Readiness: the DB is reachable. Mirrors the control plane's `/healthz`.
async fn healthz(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    state
        .db
        .exec("SELECT 1", params![])
        .await
        .map_err(|_| internal())?;
    Ok(Json(json!({ "status": "ok" })))
}

/// Liveness: the process is up. Never touches the DB.
async fn livez() -> Json<Value> {
    Json(json!({ "status": "alive" }))
}

async fn me(State(state): State<AppState>, caller: Caller) -> Result<Json<Value>, ApiError> {
    // The caller's tenant role, so the frontend can gate channel management to
    // admins (MAIN-94 AC-5) — `null` for a caller with no `users` row.
    let role = tenant_role(&*state.channels, caller.user_id, caller.tenant_id).await?;
    // The caller's PERSON (cross-tenant identity), so the DM UI can name a
    // conversation by its *other* participants (MAIN-113 AC-5). `null` if the
    // user row has no person (pre-MAIN-130 rows).
    let person_id: Option<Uuid> = state
        .dms
        .person_of(caller.user_id)
        .await
        .map_err(|_| internal())?;
    Ok(Json(json!({
        "user_id": caller.user_id,
        "tenant_id": caller.tenant_id,
        "person_id": person_id,
        "cookie_session": caller.cookie_session,
        "role": role,
    })))
}

/// The caller's role in their tenant, read from the shared `public.users`
/// (reachable through the `chat,public` search_path). `None` when there is no
/// membership row. This is the ONLY role source chat uses (NG-5): the existing
/// per-tenant `users.role`, not a new permission catalog.
pub(crate) async fn tenant_role(
    repo: &dyn repo::channels::ChannelRepository,
    user: Uuid,
    tenant: Uuid,
) -> Result<Option<String>, ApiError> {
    repo.tenant_role(user, tenant).await.map_err(|_| internal())
}

/// Owner and admin manage channels; everyone else is a member who can read and
/// post but not create/rename/archive. Mirrors the control plane's
/// `require_tenant_admin` (MAIN-94 AC-5, NG-5).
pub(crate) fn role_is_admin(role: Option<&str>) -> bool {
    matches!(role, Some("owner") | Some("admin"))
}

/// Refuse a caller who is not a tenant owner/admin with a 403 — the gate on
/// every channel-management handler (MAIN-94 AC-5).
pub(crate) async fn require_admin(
    repo: &dyn repo::channels::ChannelRepository,
    caller: &Caller,
) -> Result<(), ApiError> {
    let role = tenant_role(repo, caller.user_id, caller.tenant_id).await?;
    if role_is_admin(role.as_deref()) {
        Ok(())
    } else {
        Err(ApiError::Forbidden)
    }
}

/// A validated caller, resolved through `nook-auth` exactly as the control plane
/// does. The credential plumbing (bearer header / session cookie) lives here;
/// the database check is the shared one.
pub(crate) struct Caller {
    pub(crate) user_id: Uuid,
    pub(crate) tenant_id: Uuid,
    pub(crate) cookie_session: bool,
}

impl FromRequestParts<AppState> for Caller {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // 1. `Authorization: Bearer nook_user_…` — a personal access token.
        if let Some(tok) = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .filter(|t| t.starts_with(nook_auth::USER_TOKEN_PREFIX))
        {
            let r = nook_auth::resolve_bearer(&state.db, tok)
                .await
                .map_err(ApiError::from)?;
            return Ok(Self::from(r));
        }

        // 2. The `nook_session` cookie (its value is the plaintext session id).
        let jar = CookieJar::from_headers(&parts.headers);
        let sid = jar
            .get(nook_auth::SESSION_COOKIE)
            .and_then(|c| c.value().parse::<Uuid>().ok())
            .ok_or(ApiError::Unauthorized)?;
        let r = nook_auth::resolve_session(&state.db, sid)
            .await
            .map_err(ApiError::from)?;
        Ok(Self::from(r))
    }
}

impl From<nook_auth::Resolved> for Caller {
    fn from(r: nook_auth::Resolved) -> Self {
        Self {
            user_id: r.user_id,
            tenant_id: r.tenant_id,
            cookie_session: r.cookie_session,
        }
    }
}

/// Chat's "something broke and it is not the caller's fault" error.
///
/// `internal()` was a unit variant that carried nothing and logged
/// nothing (MAIN-274). The shared `ApiError::Internal` carries an
/// `anyhow::Error` and logs it, so the body a client sees is unchanged —
/// `{"error":"internal error"}`, 500 — while the server finally records which
/// call failed. Call sites pass what they know; `internal()` is the bare form
/// for the many that previously discarded the cause entirely.
pub(crate) fn internal() -> ApiError {
    ApiError::Internal(anyhow::anyhow!("chat: internal error"))
}

/// The outer middleware every chat route is served through.
///
/// Named rather than inlined so a test can drive the REAL stack without an
/// `AppState` — a test that assembled its own replica would keep passing if
/// somebody deleted a layer from here (MAIN-273).
fn with_middleware<S>(router: axum::Router<S>) -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    router
        // INSIDE the trace layer, not outside it: a panic anywhere in a handler
        // or extractor becomes a 500 the trace layer then records like any
        // other response, rather than unwinding past it.
        .layer(tower_http::catch_panic::CatchPanicLayer::custom(
            nook_errors::panic_response,
        ))
        .layer(tower_http::trace::TraceLayer::new_for_http())
}

#[cfg(test)]
mod panic_layer_tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    // The unwrap IS the subject: clippy is right that it always panics, which
    // is exactly the bug this net has to survive.
    #[allow(clippy::unnecessary_literal_unwrap)]
    async fn boom() -> &'static str {
        let nothing: Option<u8> = None;
        let _ = nothing.expect("a chat handler bug");
        "unreachable"
    }

    /// MAIN-273 AC-1 for this service: `nook-errors` proves what the layer does;
    /// this proves chat's router is wired to it. Deleting the layer from
    /// `with_middleware` fails here and nowhere else.
    #[tokio::test]
    async fn the_real_middleware_stack_catches_a_handler_panic() {
        let app = super::with_middleware(Router::new().route("/boom", get(boom))).with_state(());
        let res = app
            .oneshot(Request::builder().uri("/boom").body(Body::empty()).unwrap())
            .await
            .expect("the real stack answers rather than dropping the connection");
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .expect("a readable body — a dropped connection has none");
        assert_eq!(
            String::from_utf8(body.to_vec()).unwrap(),
            r#"{"error":"internal error"}"#
        );
    }
}

/// SIGTERM/SIGINT → graceful shutdown, like the control plane.
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = term.recv() => {},
        _ = int.recv() => {},
    }
    tracing::info!("shutdown signal received");
}

#[cfg(test)]
mod tests {
    use super::role_is_admin;

    /// Only owner/admin manage channels; a member or an unknown caller does not
    /// (MAIN-94 AC-5). Getting this backwards would silently open management to
    /// everyone or lock it from the admins.
    #[test]
    fn admin_roles_are_owner_and_admin_only() {
        assert!(role_is_admin(Some("owner")));
        assert!(role_is_admin(Some("admin")));
        assert!(!role_is_admin(Some("member")));
        assert!(!role_is_admin(Some("")));
        assert!(!role_is_admin(None));
    }
}
