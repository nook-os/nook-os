pub mod auth;
pub mod boards;
pub mod dispatcher;
pub mod dist;
pub mod events;
pub mod feedback;
pub mod gitops;
pub mod health;
pub mod join;
pub mod nodes;
pub mod notes;
pub mod schedule;
pub mod sessions;
pub mod settings;
pub mod taskwork;
pub mod tenants;
pub mod themes;
pub mod tokens;
pub mod vault;
pub mod workspaces;

use axum::response::IntoResponse;
use axum::routing::{delete as delete_route, get, patch, post, put};
use axum::{Json, Router};
use tower_http::trace::TraceLayer;
use utoipa::OpenApi;

use crate::openapi::ApiDoc;
use crate::state::AppState;

pub fn build_router(state: AppState) -> Router {
    let api = Router::new()
        .route("/auth/login", get(auth::login))
        .route("/auth/callback", get(auth::callback))
        .route("/auth/dev-login", post(auth::dev_login))
        .route("/auth/logout", post(auth::logout))
        .route("/auth/me", get(auth::me))
        .route("/auth/providers", get(auth::providers))
        .route("/tenants", get(tenants::list))
        .route(
            "/workspaces",
            get(workspaces::list).post(workspaces::create),
        )
        .route(
            "/workspaces/{id}",
            get(workspaces::get_one)
                .patch(workspaces::rename)
                .delete(workspaces::delete),
        )
        .route("/workspaces/{id}/git", get(workspaces::git_status))
        .route("/workspaces/{id}/worktrees", post(gitops::add_worktree))
        .route("/workspaces/{id}/git/commit", post(gitops::git_commit))
        .route("/workspaces/{id}/git/push", post(gitops::git_push))
        .route("/workspaces/{id}/secrets", get(gitops::list_secrets))
        .route(
            "/workspaces/{id}/secrets/{name}",
            get(gitops::get_secret).put(gitops::put_secret),
        )
        .route(
            "/workspaces/{id}/secrets/{name}/open",
            post(gitops::open_secret),
        )
        .route(
            "/workspaces/{id}/secrets/{name}/on-disk",
            get(gitops::secret_on_disk),
        )
        .route(
            "/workspaces/{id}/secrets/{name}/import",
            post(gitops::import_secret),
        )
        .route(
            "/git-credentials",
            get(gitops::list_credentials).post(gitops::create_credential),
        )
        .route(
            "/git-credentials/{id}",
            axum::routing::delete(gitops::delete_credential),
        )
        .route("/nodes/{id}/rescan", post(nodes::rescan))
        .route("/nodes/{id}/terminal", post(sessions::open_terminal))
        .route("/nodes/{id}/clone", post(gitops::clone_repo))
        .route("/nodes/{id}/projects", post(gitops::init_project))
        .route(
            "/workspaces/{id}/worktrees/remove",
            post(gitops::remove_worktree),
        )
        .route(
            "/workspaces/{id}/notes",
            get(notes::list).post(notes::create),
        )
        .route("/notes/{id}", patch(notes::update))
        .route("/nodes", get(nodes::list))
        .route("/nodes/{id}", get(nodes::get_one).delete(nodes::delete))
        .route("/node/releases", get(dist::releases))
        .route(
            "/node/artifacts/{version}/{name}",
            // Node binaries are tens of megabytes; axum's default cap is 2MB,
            // which turns a perfectly good upload into a bewildering 413.
            put(dist::publish).layer(axum::extract::DefaultBodyLimit::max(512 * 1024 * 1024)),
        )
        .route("/nodes/join-tokens", post(join::create_join_token))
        .route("/nodes/join", post(join::join))
        .route("/ws/ui", get(crate::ws::ui::ui_ws))
        .route("/ws/node", get(crate::ws::node::node_ws))
        .route("/boards", get(boards::list).post(boards::create))
        .route(
            "/boards/{id}",
            get(boards::get_one)
                .patch(boards::update_board)
                .delete(boards::delete_board),
        )
        .route("/boards/{id}/columns", post(boards::add_column))
        .route(
            "/columns/{id}",
            patch(boards::update_column).delete(boards::delete_column),
        )
        .route("/boards/{id}/tasks", post(boards::create_task))
        .route(
            "/tasks/{id}",
            patch(boards::update_task).delete(boards::delete_task),
        )
        .route("/tasks/{id}/dispatch", post(taskwork::dispatch))
        .route("/tasks/{id}/start-work", post(taskwork::start_work))
        .route("/tasks/{id}/submit-pr", post(taskwork::submit_pr))
        .route("/tasks/{id}/prune-worktree", post(taskwork::prune_worktree))
        .route("/tasks/{id}/move", post(taskwork::move_task))
        .route("/sessions", get(sessions::list).post(sessions::create))
        .route(
            "/sessions/{id}",
            get(sessions::get_one)
                .patch(sessions::update)
                .delete(sessions::delete),
        )
        .route("/sessions/{id}/windows", post(sessions::windows))
        .route("/tokens", get(tokens::list).post(tokens::create))
        .route("/tokens/{id}", delete_route(tokens::revoke))
        .route("/sessions/{id}/input", post(sessions::input))
        .route("/sessions/{id}/output", post(sessions::output))
        .route("/sessions/{id}/kill", post(sessions::kill))
        .route("/sessions/{id}/restart", post(sessions::restart))
        .route(
            "/ws/sessions/{id}/attach",
            get(crate::ws::attach::attach_ws),
        )
        .route("/dispatcher/suggest", post(dispatcher::suggest))
        .route("/schedule/node", get(schedule::node))
        .route("/events", get(events::list))
        .route("/themes", get(themes::list))
        .route("/themes/{slug}", get(themes::get_one))
        .route("/feedback", get(feedback::list).post(feedback::submit))
        .route(
            "/feedback/target",
            get(feedback::target).put(feedback::set_target),
        )
        .route("/feedback/{id}", patch(feedback::update))
        .route("/vault/status", get(vault::status))
        .route("/vault/passphrase", post(vault::set_passphrase))
        .route("/vault/verify", post(vault::verify))
        .route(
            "/vault/passkeys",
            get(vault::list_passkeys).post(vault::add_passkey),
        )
        .route("/vault/passkeys/{id}", delete_route(vault::delete_passkey))
        .route("/vault/passkeys/{id}/used", post(vault::touch_passkey))
        .route("/settings", get(settings::list))
        .route("/settings/{key}", put(settings::put));

    // MCP: streamable-HTTP service guarded by the static MCP token (dev) or an
    // OIDC access token from the configured issuer.
    let mcp = nook_mcp::router(std::sync::Arc::new(crate::mcp_backend::McpBackend {
        state: state.clone(),
    }))
    .layer(axum::middleware::from_fn_with_state(
        state.clone(),
        mcp_auth,
    ));

    let mut router = Router::new()
        .route("/healthz", get(health::healthz))
        // Both unauthenticated: they are fetched by a machine that has no
        // session yet, holding nothing but a join token.
        .route("/install.sh", get(dist::install_script))
        .route("/dist/{name}", get(dist::download))
        .route("/dist/{version}/{name}", get(dist::download_versioned))
        .route(
            "/.well-known/oauth-protected-resource",
            get(oauth_protected_resource),
        )
        .nest_service("/mcp", mcp)
        .route("/openapi.json", get(|| async { Json(ApiDoc::openapi()) }))
        .nest("/api/v1", api);

    if !state.cfg.is_production() {
        router = router.merge(
            utoipa_swagger_ui::SwaggerUi::new("/docs").url("/docs/openapi.json", ApiDoc::openapi()),
        );
    }

    router.layer(TraceLayer::new_for_http()).with_state(state)
}

/// Bearer gate for the MCP endpoint. Two accepted credentials:
/// 1. The static `MCP_TOKEN` (dev / simple header-configured clients).
/// 2. An OIDC access token from the configured issuer, validated against the
///    provider's userinfo endpoint — provider-agnostic (works for JWT and
///    opaque tokens alike) and cached briefly.
///
/// Failures answer with RFC 9728 discovery (`WWW-Authenticate` pointing at
/// `/.well-known/oauth-protected-resource`) so OAuth-capable MCP clients
/// (ChatGPT connectors, Claude custom connectors) can find the issuer.
async fn mcp_auth(
    axum::extract::State(state): axum::extract::State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let bearer = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string);
    if let Some(token) = bearer {
        if state.cfg.mcp_token.as_deref() == Some(token.as_str())
            || oidc_token_valid(&state, &token).await
        {
            return next.run(req).await;
        }
    }
    let metadata_url = format!(
        "{}/.well-known/oauth-protected-resource",
        request_base(req.headers())
    );
    let mut resp = crate::error::ApiError::Unauthorized.into_response();
    if let Ok(v) =
        format!("Bearer resource_metadata=\"{metadata_url}\"").parse::<axum::http::HeaderValue>()
    {
        resp.headers_mut()
            .insert(axum::http::header::WWW_AUTHENTICATE, v);
    }
    resp
}

/// Validate an OIDC access token by presenting it to the issuer's userinfo
/// endpoint. Successful validations are cached (by hash) for a minute.
async fn oidc_token_valid(state: &AppState, token: &str) -> bool {
    use std::hash::{Hash, Hasher};
    let Some(oidc) = state.oidc.as_ref() else {
        return false;
    };
    let Some(endpoint) = oidc.metadata.userinfo_endpoint() else {
        return false;
    };
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    token.hash(&mut hasher);
    let key = hasher.finish();
    if let Some(at) = state.mcp_auth_cache.get(&key) {
        if at.elapsed() < std::time::Duration::from_secs(60) {
            return true;
        }
    }
    let ok = oidc
        .http
        .get(endpoint.as_str())
        .bearer_auth(token)
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false);
    if ok {
        // Opportunistic pruning keeps the cache bounded.
        if state.mcp_auth_cache.len() > 1024 {
            state
                .mcp_auth_cache
                .retain(|_, at| at.elapsed() < std::time::Duration::from_secs(300));
        }
        state.mcp_auth_cache.insert(key, std::time::Instant::now());
    }
    ok
}

/// The base URL clients used to reach us (honors reverse-proxy headers).
fn request_base(headers: &axum::http::HeaderMap) -> String {
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("http");
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost:8080");
    format!("{proto}://{host}")
}

/// RFC 9728 protected-resource metadata: tells OAuth-capable MCP clients that
/// `/mcp` is protected by the configured OIDC issuer.
async fn oauth_protected_resource(
    axum::extract::State(state): axum::extract::State<AppState>,
    headers: axum::http::HeaderMap,
) -> axum::Json<serde_json::Value> {
    let base = request_base(&headers);
    let servers: Vec<String> = state.cfg.oidc_issuer_url.iter().cloned().collect();
    axum::Json(serde_json::json!({
        "resource": format!("{base}/mcp"),
        "authorization_servers": servers,
        "bearer_methods_supported": ["header"],
    }))
}
