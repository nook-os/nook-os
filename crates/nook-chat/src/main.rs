//! `nook-chat` — the team-chat service skeleton (MAIN-48).
//!
//! A separate binary and container from the control plane, because chat's
//! real-time fan-out has a very different load profile. It ships no user-facing
//! chat yet: this is the foundation the messaging and UI tickets build on — a
//! service that boots, checks its health, owns its tables in the shared
//! Postgres (in a dedicated `chat` schema so its migration ledger is isolated),
//! and authenticates callers by REUSING NookOS auth via the shared `nook-auth`
//! crate rather than a second login.

mod config;

use std::str::FromStr;

use axum::extract::{FromRequestParts, State};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use axum_extra::extract::CookieJar;
use serde_json::{json, Value};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::PgPool;
use uuid::Uuid;

/// Chat's own migration set, embedded at compile time — applied into the `chat`
/// schema, so it never touches the control plane's `public._sqlx_migrations`.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

#[derive(Clone)]
struct AppState {
    db: PgPool,
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

    // Pin every connection's search_path to `chat` (libpq `-c search_path=chat`).
    // sqlx then creates its ledger as `chat._sqlx_migrations` and chat's tables
    // in `chat.*`, independent of the control plane's `public` schema (AC-5).
    let opts = PgConnectOptions::from_str(&cfg.database_url)?.options([("search_path", "chat")]);
    let db = PgPoolOptions::new()
        .max_connections(10)
        .connect_with(opts)
        .await?;
    // The schema must exist before the migrator creates chat._sqlx_migrations.
    sqlx::query("CREATE SCHEMA IF NOT EXISTS chat")
        .execute(&db)
        .await?;
    MIGRATOR.run(&db).await?;

    let state = AppState { db };
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/livez", get(livez))
        // A minimal authenticated endpoint proving auth reuse (AC-4): it
        // resolves the SAME user+tenant the control plane would.
        .route("/api/me", get(me))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&cfg.bind).await?;
    tracing::info!(bind = %cfg.bind, "nook-chat listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
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

async fn me(caller: Caller) -> Json<Value> {
    Json(json!({
        "user_id": caller.user_id,
        "tenant_id": caller.tenant_id,
        "cookie_session": caller.cookie_session,
    }))
}

/// A validated caller, resolved through `nook-auth` exactly as the control plane
/// does. The credential plumbing (bearer header / session cookie) lives here;
/// the database check is the shared one.
struct Caller {
    user_id: Uuid,
    tenant_id: Uuid,
    cookie_session: bool,
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

/// Chat's error → HTTP mapping. Preserves NookOS's 401/403/500 split.
enum ChatError {
    Unauthorized,
    Forbidden,
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
            ChatError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            ChatError::Forbidden => (StatusCode::FORBIDDEN, "forbidden"),
            ChatError::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
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
