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
mod ws;

use std::str::FromStr;
use std::sync::Arc;

use axum::extract::{FromRequestParts, State};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post, put};
use axum::{Json, Router};
use axum_extra::extract::CookieJar;
use nook_db::DbPool;
use serde_json::{json, Value};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use uuid::Uuid;

/// Chat's own migration set, embedded at compile time — applied into the `chat`
/// schema, so it never touches the control plane's `public._sqlx_migrations`.
/// Embedded: 0001_chat_init, 0002_chat_channel_archive, 0003_chat_dm,
/// 0004_chat_threads, 0005_chat_reactions, 0006_chat_categories,
/// 0007_chat_read_cursors.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) db: DbPool,
    /// Per-channel live fan-out for the delivery websocket (AC-3).
    pub(crate) registry: Arc<registry::Registry>,
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

    let cfg = config::Config::from_env()?;
    tracing::info!(
        bind = %cfg.bind,
        web_origin = %cfg.web_origin,
        public_base_url = %cfg.public_base_url,
        "nook-chat starting",
    );

    // Pin every connection's search_path to `chat,public` (libpq `-c
    // search_path=chat,public`). `chat` is first, so sqlx creates its ledger as
    // `chat._sqlx_migrations` and chat's own tables in `chat.*`, independent of
    // the control plane's `public` schema (AC-5). `public` is the fallback so the
    // SHARED `nook-auth` session query — unqualified `sessions_auth`,
    // `tenant_members`, `user_tokens`, which live only in `public` — resolves
    // instead of 500ing on `chat.sessions_auth does not exist`. No chat table
    // shares a name with a public one, so `chat` winning first is safe.
    let opts =
        PgConnectOptions::from_str(&cfg.database_url)?.options([("search_path", "chat,public")]);
    let db = PgPoolOptions::new()
        .max_connections(10)
        .connect_with(opts)
        .await?;
    // The schema must exist before the migrator creates chat._sqlx_migrations.
    ensure_chat_schema(&db).await?;
    MIGRATOR.run(&db).await?;

    // Live fan-out: a local per-channel broadcast registry, plus a Postgres
    // LISTEN/NOTIFY bus so a post on any instance reaches subscribers on all of
    // them (AC-3). Held in memory — a restart drops subscriptions, and clients
    // reconnect and backfill from history.
    let registry = Arc::new(registry::Registry::new());
    bus::start(registry.clone(), db.clone());

    let state = AppState { db, registry };
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
        .route("/api/people", get(dms::people))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state);

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
pub(crate) async fn ensure_chat_schema(pool: &nook_db::DbPool) -> Result<(), sqlx::Error> {
    match sqlx::query("CREATE SCHEMA IF NOT EXISTS chat")
        .execute(pool)
        .await
    {
        Ok(_) => Ok(()),
        Err(e) if e.as_database_error().and_then(|d| d.code()).as_deref() == Some("23505") => {
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Readiness: the DB is reachable. Mirrors the control plane's `/healthz`.
async fn healthz(State(state): State<AppState>) -> Result<Json<Value>, ChatError> {
    sqlx::query("SELECT 1")
        .execute(&state.db)
        .await
        .map_err(|_| ChatError::Internal)?;
    Ok(Json(json!({ "status": "ok" })))
}

/// Liveness: the process is up. Never touches the DB.
async fn livez() -> Json<Value> {
    Json(json!({ "status": "alive" }))
}

async fn me(State(state): State<AppState>, caller: Caller) -> Result<Json<Value>, ChatError> {
    // The caller's tenant role, so the frontend can gate channel management to
    // admins (MAIN-94 AC-5) — `null` for a caller with no `users` row.
    let role = tenant_role(&state.db, caller.user_id, caller.tenant_id).await?;
    // The caller's PERSON (cross-tenant identity), so the DM UI can name a
    // conversation by its *other* participants (MAIN-113 AC-5). `null` if the
    // user row has no person (pre-MAIN-130 rows).
    let person_id: Option<Uuid> = sqlx::query_scalar("SELECT person_id FROM users WHERE id = $1")
        .bind(caller.user_id)
        .fetch_optional(&state.db)
        .await
        .map_err(|_| ChatError::Internal)?
        .flatten();
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
    db: &DbPool,
    user: Uuid,
    tenant: Uuid,
) -> Result<Option<String>, ChatError> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT role FROM users WHERE id = $1 AND tenant_id = $2")
            .bind(user)
            .bind(tenant)
            .fetch_optional(db)
            .await
            .map_err(|_| ChatError::Internal)?;
    Ok(row.map(|(r,)| r))
}

/// Owner and admin manage channels; everyone else is a member who can read and
/// post but not create/rename/archive. Mirrors the control plane's
/// `require_tenant_admin` (MAIN-94 AC-5, NG-5).
pub(crate) fn role_is_admin(role: Option<&str>) -> bool {
    matches!(role, Some("owner") | Some("admin"))
}

/// Refuse a caller who is not a tenant owner/admin with a 403 — the gate on
/// every channel-management handler (MAIN-94 AC-5).
pub(crate) async fn require_admin(db: &DbPool, caller: &Caller) -> Result<(), ChatError> {
    let role = tenant_role(db, caller.user_id, caller.tenant_id).await?;
    if role_is_admin(role.as_deref()) {
        Ok(())
    } else {
        Err(ChatError::Forbidden)
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
    type Rejection = ChatError;

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
                .map_err(ChatError::from)?;
            return Ok(Self::from(r));
        }

        // 2. The `nook_session` cookie (its value is the plaintext session id).
        let jar = CookieJar::from_headers(&parts.headers);
        let sid = jar
            .get(nook_auth::SESSION_COOKIE)
            .and_then(|c| c.value().parse::<Uuid>().ok())
            .ok_or(ChatError::Unauthorized)?;
        let r = nook_auth::resolve_session(&state.db, sid)
            .await
            .map_err(ChatError::from)?;
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

/// Chat's error → HTTP mapping. Preserves NookOS's 401/403/500 split, plus the
/// request-shape errors the messaging routes need.
#[derive(Debug)]
pub(crate) enum ChatError {
    Unauthorized,
    Forbidden,
    /// The channel does not exist (distinct from another tenant's channel, which
    /// is `Forbidden` per AC-5).
    NotFound,
    BadRequest(String),
    /// A rule was violated that is not the client's malformed input — a
    /// duplicate channel name, a post to an archived channel.
    Conflict(String),
    Internal,
}

impl From<nook_auth::AuthError> for ChatError {
    fn from(e: nook_auth::AuthError) -> Self {
        match e {
            nook_auth::AuthError::Unauthorized => ChatError::Unauthorized,
            nook_auth::AuthError::Forbidden => ChatError::Forbidden,
            nook_auth::AuthError::Db(_) => ChatError::Internal,
        }
    }
}

impl IntoResponse for ChatError {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            ChatError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized".to_string()),
            ChatError::Forbidden => (StatusCode::FORBIDDEN, "forbidden".to_string()),
            ChatError::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
            ChatError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            ChatError::Conflict(m) => (StatusCode::CONFLICT, m),
            ChatError::Internal => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error".to_string(),
            ),
        };
        (status, Json(json!({ "error": msg }))).into_response()
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
