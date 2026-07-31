pub mod auth;
pub mod boards;
pub mod bulk;
pub mod dispatcher;
pub mod dist;
pub mod events;
pub mod feedback;
pub mod gitops;
pub mod health;
pub mod interactions;
pub mod invites;
pub mod jobs;
pub mod join;
pub mod labels;
pub mod local_auth;
pub mod managed;
pub mod nodes;
pub mod notebook;
pub mod notes;
pub mod notifications;
pub mod oidc_exchange;
pub mod operator;
pub mod overview;
pub mod runtime_auth;
pub mod schedule;
pub mod sessions;
pub mod settings;
pub mod skills;
pub mod task_detail;
pub mod task_query;
pub mod taskwork;
pub mod tenant_ca;
pub mod tenants;
pub mod themes;
pub mod tokens;
pub mod vault;
pub mod verify_email;
pub mod workspaces;

use axum::response::IntoResponse;
use axum::routing::{delete as delete_route, get, patch, post, put};
use axum::{Json, Router};
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::trace::TraceLayer;
use utoipa::OpenApi;

use crate::openapi::ApiDoc;
use crate::state::AppState;

/// The agent's own door: only what a machine needs, and nothing a browser does.
///
/// Deliberately tiny. This listener is the one that will require a client
/// certificate once mTLS lands, and its TLS has to terminate here rather than
/// at the edge proxy — only the control plane knows which tenant CA to verify
/// against. Keeping it to two routes means that handshake guards a surface
/// small enough to reason about:
///
/// - `/api/v1/nodes/join` — first contact. Unauthenticated by design: a machine
///   that has not joined has only its join token, and this is where it trades
///   that token for an identity.
/// - `/api/v1/ws/node` — the single persistent outbound connection.
///
/// `/healthz` rides along so a load balancer can probe this port too.
///
/// These routes stay mounted on the main router as well for now, so the
/// existing fleet keeps connecting while machines migrate to the agent port.
/// That duplication is transitional and goes away with mTLS, at which point
/// the main port stops accepting node connections entirely.
pub fn build_agent_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(health::healthz))
        .route("/livez", get(health::livez))
        .nest(
            "/api/v1",
            Router::new()
                .route("/nodes/join", post(join::join))
                .route("/tenant/cas", get(tenant_ca::list).post(tenant_ca::stage))
                .route("/tenant/cas/{id}", delete_route(tenant_ca::retire))
                .route("/tenant/cas/{id}/promote", post(tenant_ca::promote))
                .route("/nodes/{id}/revoke", post(tenant_ca::revoke_node))
                .route("/nodes/enroll", post(join::enroll))
                .route("/nodes/renew", post(join::renew))
                .route("/ws/node", get(crate::ws::node::node_ws)),
        )
        .with_state(state)
}

pub fn build_router(state: AppState) -> Router {
    let api = Router::new()
        .route("/auth/login", get(auth::login))
        .route("/auth/callback", get(auth::callback))
        .route("/auth/dev-login", post(auth::dev_login))
        .route("/auth/dev-accounts", get(auth::dev_accounts))
        .route("/auth/purge-test-tenants", post(auth::purge_test_tenants))
        .route("/auth/logout", post(auth::logout))
        .route("/auth/me", get(auth::me))
        .route("/me/tenants", get(auth::my_tenants))
        .route("/me/tenant", post(auth::switch_tenant))
        .route("/auth/providers", get(auth::providers))
        .route("/auth/local/status", get(local_auth::status))
        .route("/auth/oidc/exchange", post(oidc_exchange::exchange))
        .route("/auth/local/login", post(local_auth::login))
        .route("/auth/local/bootstrap", post(local_auth::bootstrap))
        .route("/auth/local/users", post(local_auth::create_user))
        .route("/auth/local/password", post(local_auth::change_password))
        .route("/auth/verify-email/status", get(verify_email::status))
        .route("/auth/verify-email/request", post(verify_email::request))
        .route("/auth/verify-email/confirm", post(verify_email::confirm))
        .route("/tenants", get(tenants::list))
        .route("/tenants/{id}/members", get(tenants::list_members))
        .route(
            "/tenants/{id}/members/{pid}",
            patch(tenants::change_member_role).delete(tenants::remove_member),
        )
        .route("/tenants/{id}/leave", post(tenants::leave_tenant))
        .route(
            "/tenants/{id}/invites",
            get(invites::list).post(invites::create),
        )
        .route(
            "/tenants/{id}/invites/{invite}",
            delete_route(invites::revoke),
        )
        .route(
            "/tenants/{id}/invites/{invite}/resend",
            post(invites::resend),
        )
        .route("/invites/accept", post(invites::accept))
        // Unauthenticated: the signed-out /accept landing reads it (MAIN-97).
        .route("/invites/preview", get(invites::preview))
        // Unauthenticated: the /accept landing lets a local invitee register an
        // account against the invite (MAIN-98).
        .route("/invites/register", post(invites::register))
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
        .route("/overview", get(overview::overview))
        .route(
            "/workspaces/{id}/session-spec",
            get(workspaces::get_session_spec).put(workspaces::set_session_spec),
        )
        .route("/workspaces/{id}/git", get(workspaces::git_status))
        .route("/workspaces/{id}/clone", post(workspaces::clone_to_node))
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
        // ── the operator surface ──
        //
        // One prefix, read-only, and deliberately containing ZERO session
        // routes. A pull request adding one here is visible at a glance in a
        // way the same route added to `sessions.rs` would not be — which is
        // the entire reason these are grouped rather than scattered.
        .route("/operator/tenants", get(operator::tenants))
        .route("/operator/nodes", get(operator::nodes))
        .route("/operator/audit", get(operator::audit_log))
        .route(
            "/operator/bindings",
            get(operator::bindings).post(operator::grant),
        )
        .route(
            "/operator/orgs",
            get(operator::orgs).post(operator::create_org),
        )
        .route("/operator/orgs/{id}", patch(operator::rename_org))
        .route("/operator/tenants/{id}/org", post(operator::move_tenant))
        .route("/operator/tenants/{id}/ca", post(operator::stage_ca))
        .route(
            "/operator/tenants/{id}/ca/{ca}/promote",
            post(operator::promote_ca),
        )
        .route("/operator/nodes/{id}/revoke", post(operator::revoke_node))
        .route("/operator/nodes/{id}", delete_route(operator::remove_node))
        .route(
            "/operator/orgs/{id}/policy",
            get(operator::get_policy).post(operator::set_policy),
        )
        .route("/notify", post(notifications::notify_now))
        .route(
            "/notifications",
            get(notifications::list).delete(notifications::clear),
        )
        .route("/notifications/read", post(notifications::mark_read))
        .route(
            "/notifications/kinds",
            get(notifications::notification_kinds),
        )
        .route(
            "/notification-channels",
            get(notifications::list_channels).post(notifications::create_channel),
        )
        .route("/notification-channels/kinds", get(notifications::kinds))
        .route(
            "/notification-channels/{id}",
            patch(notifications::update_channel).delete(notifications::delete_channel),
        )
        .route(
            "/notification-channels/{id}/test",
            post(notifications::test_channel),
        )
        .route("/labels", get(labels::list).post(labels::create))
        .route("/labels/{id}", delete_route(labels::delete))
        .route("/tasks", get(task_query::query))
        .route("/tasks/{id}", get(task_detail::get_task))
        .route("/tasks/{id}/claim", post(task_query::claim))
        .route("/tasks/{id}/release", post(task_query::release))
        .route(
            "/tasks/{id}/labels/{label}",
            put(labels::add).delete(labels::remove),
        )
        .route(
            "/tasks/{id}/comments",
            get(task_detail::list_comments).post(task_detail::create_comment),
        )
        .route(
            "/comments/{id}",
            patch(task_detail::update_comment).delete(task_detail::delete_comment),
        )
        .route("/tasks/{id}/relations", post(task_detail::create_relation))
        .route(
            "/relations/{id}",
            delete_route(task_detail::delete_relation),
        )
        .route("/skills", get(skills::list).post(skills::teach))
        .route(
            "/skills/{name}",
            get(skills::get_one).delete(skills::unteach),
        )
        .route("/managed/skills", get(managed::list_skills))
        .route("/managed/hooks", get(managed::get_hooks))
        .route("/nodes/{id}/shared", post(nodes::set_shared))
        .route(
            "/nodes/{id}/placement",
            get(nodes::get_placement).put(nodes::set_placement),
        )
        .route("/nodes/{id}/authorize", post(nodes::authorize))
        // The sessionless replacement (MAIN-290). Coexists with the line
        // above until C5 retires it.
        .route("/runtime-auth", post(runtime_auth::start))
        .route("/nodes/{id}/rescan", post(nodes::rescan))
        .route("/nodes/{id}/migrate-paths", post(nodes::migrate_paths))
        .route("/nodes/{id}/update", post(nodes::update))
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
        // Personal notebook (MAIN-66): person-owned, cross-org, operator-invisible.
        .route(
            "/notebook/notes",
            get(notebook::list_notes).post(notebook::create_note),
        )
        .route(
            "/notebook/notes/{id}",
            get(notebook::get_note)
                .patch(notebook::update_note)
                .delete(notebook::delete_note),
        )
        // Zero-knowledge note sealing + the person-level app-password vault (MAIN-100).
        .route("/notebook/notes/{id}/seal", post(notebook::seal_note))
        .route("/notebook/notes/{id}/unseal", post(notebook::unseal_note))
        .route("/notebook/vault/status", get(notebook::vault_status))
        .route(
            "/notebook/vault/passphrase",
            post(notebook::set_vault_passphrase),
        )
        .route(
            "/notebook/vault/verify",
            post(notebook::verify_vault_passphrase),
        )
        .route(
            "/notebook/folders",
            get(notebook::list_folders).post(notebook::create_folder),
        )
        .route(
            "/notebook/folders/{id}",
            patch(notebook::update_folder).delete(notebook::delete_folder),
        )
        .route("/nodes", get(nodes::list))
        .route("/nodes/{id}", get(nodes::get_one).delete(nodes::delete))
        .route("/node/releases", get(dist::releases))
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
        .route("/tasks/{id}/archive", post(boards::archive_task))
        .route("/tasks/{id}/unarchive", post(boards::unarchive_task))
        .route(
            "/columns/{id}/archive-completed",
            post(boards::archive_completed_in_column),
        )
        .route("/tasks/{id}/dispatch", post(taskwork::dispatch))
        .route("/tasks/{id}/start-work", post(taskwork::start_work))
        .route("/tasks/{id}/submit-pr", post(taskwork::submit_pr))
        .route("/tasks/{id}/prune-worktree", post(taskwork::prune_worktree))
        .route("/tasks/{id}/move", post(taskwork::move_task))
        .route("/tasks/bulk", post(bulk::bulk_tasks))
        // Loop jobs (MAIN-127): detached spec/decompose work riding the queue.
        .route("/jobs", post(jobs::create))
        .route("/jobs/{id}", get(jobs::get))
        .route("/jobs/{id}/cancel", post(jobs::cancel))
        .route("/jobs/{id}/rerun", post(jobs::rerun))
        // MAIN-231: unsolicited human → agent steering on a live run.
        .route("/jobs/{id}/messages", post(jobs::message))
        .route("/tasks/{task_id}/jobs", get(jobs::list_for_task))
        .route(
            "/interactions",
            get(interactions::list_pending).post(interactions::create),
        )
        .route("/interactions/{id}", get(interactions::get))
        .route("/interactions/{id}/answer", post(interactions::answer))
        .route("/interactions/{id}/cancel", post(interactions::cancel))
        .route("/sessions", get(sessions::list).post(sessions::create))
        // Before `/sessions/{id}` so the literal path is not captured as an id.
        .route("/sessions/agent-states", get(sessions::agent_states))
        .route(
            "/sessions/{id}/agent-state",
            post(sessions::report_agent_state),
        )
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
    //
    // rmcp's DNS-rebinding protection rejects any Host header not on its
    // allowlist, and its default is loopback-only — which 403'd every real
    // client (ChatGPT at the public hostname, in-cluster agents at the compose
    // service name) AFTER their auth succeeded (MAIN-190). Thread the hosts we
    // actually serve; entries carry no port, so any port on these names passes.
    let mcp_allowed_hosts = {
        let mut hosts = vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
            "::1".to_string(),
            // The compose service name in-cluster callers dial.
            "control-plane".to_string(),
        ];
        if let Some(host) = host_of(&state.cfg.public_base_url) {
            if !hosts.contains(&host) {
                hosts.push(host);
            }
        }
        hosts
    };
    let mcp = nook_mcp::router(
        std::sync::Arc::new(crate::mcp_backend::McpBackend {
            state: state.clone(),
        }),
        mcp_allowed_hosts,
    )
    .layer(axum::middleware::from_fn_with_state(
        state.clone(),
        mcp_auth,
    ));

    let mut router = Router::new()
        .route("/healthz", get(health::healthz))
        // Liveness beside readiness: `/livez` never touches the DB, so a probe
        // can tell "process up" from "DB reachable" and only the latter recycles
        // a pod. Unauthenticated like `/healthz` — a kubelet carries no session.
        .route("/livez", get(health::livez))
        // Both unauthenticated: they are fetched by a machine that has no
        // session yet, holding nothing but a join token.
        .route("/install.sh", get(dist::install_script))
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

    with_middleware(router).with_state(state)
}

/// The outer middleware every route is served through.
///
/// A named function rather than a chain inlined into [`build_router`] so a test
/// can drive the REAL stack (`panic_layer_tests` below) without an `AppState` or
/// a database. A test that assembled its own replica would keep passing if
/// somebody deleted a layer from here — which is the regression worth catching.
fn with_middleware<S>(router: Router<S>) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    router
        .layer(desktop_cors())
        // INSIDE the trace layer, not outside it (MAIN-273). Layers wrap
        // outward, so this sits under `TraceLayer` and above everything else:
        // a panic anywhere in a handler, extractor or the auth middleware
        // becomes a 500 that the trace layer then records like any other
        // response. Outside it, the panic would unwind past tracing and the
        // request would vanish from the log it belongs in.
        .layer(CatchPanicLayer::custom(nook_errors::panic_response))
        .layer(TraceLayer::new_for_http())
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
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let bearer = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string);
    if let Some(token) = bearer {
        // The static MCP_TOKEN is valid but carries no person — notebook tools
        // refuse it with a pointed error (MAIN-102 AC-3).
        if state.cfg.mcp_token.as_deref() == Some(token.as_str()) {
            return next.run(req).await;
        }
        // An OIDC access token is validated against userinfo and resolved to the
        // caller's NookOS identity, which rides in the request extensions into
        // the MCP tool context (MAIN-102 AC-1).
        if let Some(caller) = resolve_mcp_caller(&state, &token).await {
            req.extensions_mut().insert(caller);
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

/// Validate an OIDC access token against the issuer's userinfo endpoint AND
/// resolve it to the caller's NookOS identity (MAIN-102). Returns `None` when
/// there is no OIDC context, the token is rejected, or the userinfo body has no
/// `sub`. Successful resolutions are cached (by token hash) for a minute so a
/// busy MCP client does not re-hit userinfo on every tool call.
///
/// The userinfo claims are mapped with the same `login_identity` path the login
/// callback uses, keyed on the configured issuer + the `sub`, so an MCP caller
/// lands on the very same `users` row as their browser session.
async fn resolve_mcp_caller(state: &AppState, token: &str) -> Option<nook_mcp::McpCaller> {
    use std::hash::{Hash, Hasher};
    let oidc = state.oidc.current()?;
    let endpoint = oidc.metadata.userinfo_endpoint()?;

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    token.hash(&mut hasher);
    let key = hasher.finish();
    if let Some(entry) = state.mcp_auth_cache.get(&key) {
        if entry.0.elapsed() < std::time::Duration::from_secs(60) {
            return Some(entry.1.clone());
        }
    }

    let resp = oidc
        .http
        .get(endpoint.as_str())
        .bearer_auth(token)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: serde_json::Value = resp.json().await.ok()?;
    let subject = body.get("sub").and_then(|v| v.as_str())?.to_string();
    let claims = crate::services::identity::IdentityClaims {
        issuer: oidc.metadata.issuer().to_string(),
        subject,
        email: body
            .get("email")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        email_verified: body
            .get("email_verified")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        display_name: body
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        avatar_url: body
            .get("picture")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        raw_claims: body,
    };
    let (user, tenant) = crate::services::identity::login_identity(state, claims)
        .await
        .ok()?;
    let person_id = crate::auth::person_id_of(state, user.id).await.ok()?;
    let caller = nook_mcp::McpCaller {
        person_id,
        user_id: user.id,
        tenant_id: tenant.id,
    };

    // Opportunistic pruning keeps the cache bounded.
    if state.mcp_auth_cache.len() > 1024 {
        state
            .mcp_auth_cache
            .retain(|_, e| e.0.elapsed() < std::time::Duration::from_secs(300));
    }
    state
        .mcp_auth_cache
        .insert(key, (std::time::Instant::now(), caller.clone()));
    Some(caller)
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

/// Let the packaged desktop app talk to us.
///
/// The web UI is same-origin and needs none of this. A Tauri build is served
/// from `tauri://localhost` (`http://tauri.localhost` on Windows), which is a
/// different origin by every definition a browser engine has — so without an
/// allowance every request from the desktop app fails before it is sent.
///
/// Credentials are deliberately NOT allowed. The desktop app authenticates
/// with a bearer token rather than the session cookie, because a cookie set by
/// one origin for another, from a custom scheme, is a fight with each
/// platform's webview that has no upside. Refusing credentials here keeps that
/// decision honest instead of half-working on one platform.
/// The bare hostname of a URL — no scheme, port, userinfo, or path — for the
/// MCP transport's Host allowlist (MAIN-190). Hand-rolled because pulling in a
/// URL crate for one field is heavier than the parse.
fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    // Bracketed IPv6: `[::1]:8080` → `::1`.
    if let Some(inner) = authority.strip_prefix('[') {
        return inner.split_once(']').map(|(h, _)| h.to_string());
    }
    let host = authority.split(':').next().unwrap_or(authority);
    (!host.is_empty()).then(|| host.to_string())
}

fn desktop_cors() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(|origin, _| {
            origin
                .to_str()
                .map(|o| {
                    o == "tauri://localhost"
                        || o == "http://tauri.localhost"
                        // Tauri's dev server, when running `tauri dev`.
                        || o.starts_with("http://localhost:")
                })
                .unwrap_or(false)
        }))
        .allow_methods(tower_http::cors::Any)
        .allow_headers(tower_http::cors::Any)
}

#[cfg(test)]
mod host_of_tests {
    use super::host_of;

    #[test]
    fn host_of_extracts_the_bare_hostname() {
        assert_eq!(
            host_of("https://nook.hein.network").as_deref(),
            Some("nook.hein.network")
        );
        assert_eq!(
            host_of("http://localhost:8080/base").as_deref(),
            Some("localhost")
        );
        assert_eq!(host_of("http://[::1]:8080").as_deref(), Some("::1"));
        assert_eq!(
            host_of("https://u:p@h.example:443/x?y#z").as_deref(),
            Some("h.example")
        );
        assert_eq!(host_of(""), None);
    }
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
        let _ = nothing.expect("a value that was not there");
        "unreachable"
    }

    /// MAIN-273 AC-1, against the middleware stack this crate actually serves.
    ///
    /// `tests/panic_safety_net.rs` pins what the layer DOES; this pins that the
    /// layer is WIRED. Deleting `CatchPanicLayer` from `with_middleware` fails
    /// here and nowhere else.
    #[tokio::test]
    async fn the_real_middleware_stack_catches_a_handler_panic() {
        let app = super::with_middleware(Router::new().route("/boom", get(boom)))
            // No `AppState` is needed: the panic never reaches state, and this
            // keeps the assertion about middleware rather than about wiring up
            // a database.
            // Generic over state on purpose: none of these layers touch it,
            // so the test needs no `AppState` and no database.
            .with_state(());

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
