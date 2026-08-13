//! Everything served under `/api/v1/tunnels`, plus the host surface a tunnel
//! answers on (MAIN-403, MAIN-404).
//!
//! **The host surface (9b).** [`host_dispatch`] is a layer, not a route,
//! because it has to decide BEFORE routing: a request whose `Host` is
//! `<label>.<TUNNEL_DOMAIN>` is answered by this module whatever its path says,
//! so a tunnel can never reach the control plane's own API (403 AC-1).
//! [`authorize`] is the apex half of the cross-subdomain grant flow — the only
//! place a session cookie is read, because the session cookie is host-only and
//! a subdomain never sees it. And [`proxy`] is what actually forwards, after
//! [`crate::tunnels::forwarded_headers`] has taken NookOS's credentials out of
//! the request (403 AC-4).
//!
//! **The lifecycle API (9c).** [`create`], [`list`] and [`stop`] are how a
//! tunnel comes into existence and goes away on purpose (404 AC-1). They are
//! here rather than in a module of their own because they are the same
//! resource: everything under `/api/v1/tunnels` and everything under
//! `*.<TUNNEL_DOMAIN>` reads the one in-memory table, and splitting them would
//! only mean two files that must not disagree.
//!
//! **The upgrade path (MAIN-10).** A request arriving with `Upgrade: websocket`
//! takes [`proxy_upgrade`] instead of [`proxy`] — same access check, same
//! stripped headers, same request id — and what follows it is [`pump`], a run
//! of frames in both directions rather than a response with a body.
//!
//! The rules the host surface applies are in `crate::tunnels`, tested without a
//! database, a node or wildcard DNS.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum::Json;
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use futures_util::{SinkExt, StreamExt};
use nook_proto::{ControlToNode, NodeToControl};
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::{AuthCtx, SESSION_COOKIE};
use crate::error::{ApiError, ApiResult};
use crate::events::{self, EventDraft};
use crate::state::AppState;
use crate::tunnels::{
    self, Access, Credential, AUTHORIZE_PATH, GRANT_PATH, RESERVED_PREFIX, TUNNEL_COOKIE,
};
use crate::ws::registry::{Registry, Tunnel};
use nook_types::{CreateTunnelRequest, NodeId, TenantId, TunnelView, UserId};

/// The most of a request body a tunnel will buffer.
///
/// The request is buffered by design — the node's proxy takes it whole
/// (MAIN-402 AC-2) — so this is the ceiling on what one in-flight tunnel
/// request costs this replica in memory. A larger upload is refused with a
/// stated limit rather than being allowed to decide how much RAM it gets.
const MAX_REQUEST_BODY: usize = 32 * 1024 * 1024;

/// How long to wait for a node to produce a response HEAD before giving up.
/// Only the head: once it arrives the body streams for as long as it takes, so
/// this bounds "the node is not answering", not "the download is slow". A
/// WebSocket handshake is bounded by the same number for the same reason — and
/// it is also what an agent too old to know `TunnelUpgrade` costs a visitor,
/// since such a node skips the frame in silence.
const HEAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Keepalive on a tunnelled WebSocket, the same twenty seconds `ws::attach`
/// uses and for the same reason: an idle socket is what an intermediary reaps,
/// and an HMR channel is idle whenever nobody is typing.
const WS_PING_INTERVAL: std::time::Duration = std::time::Duration::from_secs(20);

/// Answer a tunnel host, or hand the request on to the rest of the router.
///
/// A layer rather than a route: routes are matched by PATH, and what makes a
/// request a tunnel's is its HOST. Wrapped around the whole router so it runs
/// before matching — otherwise `https://api.<zone>/api/v1/tasks` would be
/// served by the control plane's own API to whoever opened the tunnel, which is
/// the failure AC-1 names.
pub async fn host_dispatch(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let Some(domain) = state.cfg.tunnel_domain.as_deref() else {
        return next.run(req).await;
    };
    let Some(label) = request_host(&req).and_then(|h| tunnels::tunnel_label(&h, domain)) else {
        return next.run(req).await;
    };
    serve(state, label, req).await
}

/// The host this request was addressed to. HTTP/2 puts it in the URI's
/// authority and may send no `Host` header at all, so both are consulted.
fn request_host(req: &Request) -> Option<String> {
    req.headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| req.uri().host().map(str::to_string))
}

async fn serve(state: AppState, label: String, req: Request) -> Response {
    let Some(tunnel) = state.registry.tunnel_route(&label) else {
        return tunnels::page(
            StatusCode::NOT_FOUND,
            "No such tunnel",
            &format!(
                "There is no tunnel at {label} — it never existed, or it has ended. \
                 Tunnels live in memory, so a control-plane restart ends every one \
                 of them and it has to be opened again."
            ),
        );
    };

    // The reserved prefix is handled BEFORE the access check, because it is how
    // a request without a credential gets one, and it is answered here whatever
    // the app behind the tunnel would have served at that path (AC-5).
    let path = req.uri().path().to_string();
    if path.starts_with(RESERVED_PREFIX) {
        return if path == GRANT_PATH {
            redeem(&state, &tunnel, req.uri().query().unwrap_or_default())
        } else {
            tunnels::page(
                StatusCode::NOT_FOUND,
                "Reserved path",
                "Everything under /__nook/ belongs to NookOS on a tunnel host and is \
                 never forwarded to the application behind it.",
            )
        };
    }

    let upgrade = is_websocket_upgrade(req.headers());
    match access(&state, &tunnel, req.headers()).await {
        Access::Allow => {
            // What the idle sweep measures (MAIN-404 AC-3), counted here rather
            // than on every request to the host: a bounce to the apex or a
            // refusal is somebody failing to reach the app, and a tunnel nobody
            // can get into is exactly one that should be swept.
            if let Some(announce) = announce_interval(&state) {
                state.registry.touch_tunnel(&tunnel.label, announce);
            }
            if upgrade {
                proxy_upgrade(state, tunnel, req).await
            } else {
                proxy(&state, &tunnel, req).await
            }
        }
        // Only a navigation is bounced. A 307 preserves the method, so
        // redirecting a POST would re-post the body to the apex's authorize
        // endpoint, which does not take one — and a fetch() that got a login
        // page back would be no clearer. Anything else is told, not moved.
        //
        // An upgrade is in that "anything else" despite being a GET: a
        // WebSocket handshake does not follow redirects, so bouncing one costs
        // the visitor the reason and gains them nothing (MAIN-10 AC-6).
        Access::Authorize if req.method().is_safe() && !upgrade => {
            Redirect::temporary(&authorize_url(&state, &tunnel.label, &next_of(req.uri())))
                .into_response()
        }
        Access::Authorize => tunnels::page(
            StatusCode::UNAUTHORIZED,
            "Not authorized yet",
            "This tunnel has no credential on this request. Open it in a browser once \
             to authorize, or send a `nook_user_` token in the Authorization header.",
        ),
        Access::Unauthorized => tunnels::page(
            StatusCode::UNAUTHORIZED,
            "Credential not accepted",
            "That token is not valid — it may have been revoked or expired. A browser \
             gets in by signing in to NookOS; a script sends a `nook_user_` token in \
             the Authorization header.",
        ),
        Access::Forbidden => tunnels::page(
            StatusCode::FORBIDDEN,
            "Not your tunnel",
            "You are signed in, but this tunnel belongs to a tenant you are not a \
             member of. Membership is the whole of a tunnel's access policy.",
        ),
    }
}

/// Resolve whatever credential the request carries, then decide.
///
/// The two halves are separate on purpose: this one does the database work, and
/// [`tunnels::decide`] holds the policy where it can be read and tested whole.
async fn access(state: &AppState, tunnel: &Tunnel, headers: &HeaderMap) -> Access {
    let cred = credential(state, tunnel, headers).await;
    tunnels::decide(&cred, &tunnel.label, tunnel.tenant_id.0)
}

async fn credential(state: &AppState, tunnel: &Tunnel, headers: &HeaderMap) -> Credential {
    // A bearer token wins where both are present: it is the explicit one, and a
    // script that sends a token wants to hear about that token rather than be
    // silently served under a cookie its browser session left behind (AC-3).
    if let Some(bearer) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        // Only a USER token, never a node token: a node credential is a machine
        // acting on its own machine, and a person's tunnel is not that.
        if !bearer.starts_with(crate::auth::USER_TOKEN_PREFIX) {
            return Credential::BadBearer;
        }
        return match nook_auth::resolve_bearer(&state.db, bearer).await {
            Ok(r) => Credential::Bearer {
                member: is_member(state, UserId(r.user_id), tunnel.tenant_id).await,
            },
            Err(_) => Credential::BadBearer,
        };
    }

    CookieJar::from_headers(headers)
        .get(TUNNEL_COOKIE)
        .and_then(|c| tunnels::open_cookie(&state.cfg.session_secret, c.value(), now()))
        .map(|claims| Credential::Cookie {
            label: claims.label,
            tenant: claims.tenant,
        })
        .unwrap_or(Credential::None)
}

/// Live membership of the tunnel's tenant, read per request.
///
/// `user_in_tenant` and not a cached list: it resolves the person's row in that
/// tenant AND requires the grant to still exist, so a membership revoked a
/// minute ago closes the tunnel at the next request rather than at the next
/// sign-in. A database error reads as "not a member" — refusing on an error is
/// the only safe direction for an access check.
async fn is_member(state: &AppState, user: UserId, tenant: TenantId) -> bool {
    match state.identity.user_in_tenant(user, tenant).await {
        Ok(found) => found.is_some(),
        Err(e) => {
            tracing::warn!(error = %e, "tunnel membership check failed — refusing");
            false
        }
    }
}

// ── The grant exchange ─────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct AuthorizeParams {
    /// The tunnel label being authorized for.
    to: Option<String>,
    /// Where to land on the tunnel host afterwards.
    next: Option<String>,
}

/// `GET /api/v1/tunnels/authorize` — the apex half of the flow (AC-2).
///
/// This runs on the apex because that is the only host holding the session
/// cookie: `session_cookie` is host-only, deliberately, so a tunnel subdomain
/// cannot see it and the person cannot be authenticated there directly. The
/// answer is a short-lived single-use grant carried back in the URL, which the
/// tunnel host exchanges for its own host-only cookie.
///
/// Unauthenticated by construction — it is where a signed-out person arrives —
/// so it reads the session itself rather than taking `AuthCtx`, and sends
/// somebody with no session to sign in first.
pub async fn authorize(
    State(state): State<AppState>,
    Query(params): Query<AuthorizeParams>,
    jar: CookieJar,
) -> Response {
    let Some(domain) = state.cfg.tunnel_domain.clone() else {
        return tunnels::page(
            StatusCode::NOT_FOUND,
            "Tunnels are not enabled",
            "This deployment has no TUNNEL_DOMAIN configured, so there is nothing to \
             authorize for.",
        );
    };
    let Some(label) = params.to.filter(|l| !l.is_empty()) else {
        return tunnels::page(
            StatusCode::BAD_REQUEST,
            "Nothing to authorize",
            "This link is missing the tunnel it is for.",
        );
    };
    let next = safe_next(params.next.as_deref());

    // No session — or one that no longer resolves — means sign in first, then
    // come back HERE. `next` is app-relative, which is all the sign-in page
    // accepts, and `/api/…` is proxied to this server from the app's origin.
    let session = jar
        .get(SESSION_COOKIE)
        .and_then(|c| c.value().parse::<Uuid>().ok());
    let resolved = match session {
        Some(sid) => nook_auth::resolve_session_identity(&state.db, sid)
            .await
            .ok()
            .map(|(r, _)| r),
        None => None,
    };
    let Some(resolved) = resolved else {
        let back = format!(
            "{AUTHORIZE_PATH}?{}",
            query(&[("to", &label), ("next", &next)])
        );
        return Redirect::temporary(&format!(
            "{}/login?{}",
            state.cfg.public_base_url.trim_end_matches('/'),
            query(&[("next", &back)])
        ))
        .into_response();
    };

    let Some(tunnel) = state.registry.tunnel_route(&label) else {
        return tunnels::page(
            StatusCode::NOT_FOUND,
            "No such tunnel",
            "There is no tunnel by that name — it never existed, or it has ended.",
        );
    };
    if !is_member(&state, UserId(resolved.user_id), tunnel.tenant_id).await {
        return tunnels::page(
            StatusCode::FORBIDDEN,
            "Not your tunnel",
            "This tunnel belongs to a tenant you are not a member of.",
        );
    }

    let grant = tunnels::mint_grant(
        &state.cfg.session_secret,
        &tunnel.label,
        resolved.user_id,
        tunnel.tenant_id.0,
        now(),
    );
    Redirect::temporary(&format!(
        "{}{GRANT_PATH}?{}",
        origin(&state, &tunnel.label, &domain),
        query(&[("g", &grant), ("next", &next)])
    ))
    .into_response()
}

/// `GET /__nook/tunnel/grant` on the tunnel host: spend the grant, set the
/// host-only cookie, and continue to where the person was going.
fn redeem(state: &AppState, tunnel: &Tunnel, raw_query: &str) -> Response {
    let params: Vec<(String, String)> = form_urlencoded::parse(raw_query.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let get = |name: &str| {
        params
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };
    let next = safe_next(get("next"));

    let claims = get("g")
        .and_then(|g| tunnels::open_grant(&state.cfg.session_secret, g, &tunnel.label, now()));
    // The grant must also still be UNSPENT. Signature and expiry are what stop
    // a forged or stale one; this is what stops the same one being used twice —
    // it travelled in a URL, so copies of it exist in history and in logs.
    let spent_ok = claims.as_ref().is_some_and(|c| {
        c.jti.is_some_and(|jti| {
            state.registry.spend_grant(
                jti,
                std::time::Duration::from_secs(tunnels::GRANT_TTL_SECS as u64),
            )
        })
    });
    let Some(claims) = claims.filter(|_| spent_ok) else {
        // Deliberately a page and not another redirect to the apex: a bounce
        // that can fail is a bounce that can loop, and a person staring at a
        // spinning tab learns nothing. Going to the tunnel again starts a fresh
        // flow, because THAT request has no credential either.
        return tunnels::page(
            StatusCode::UNAUTHORIZED,
            "Authorization expired",
            "That authorization is no longer valid — one lasts a minute and works once. \
             Open the tunnel's own URL again to get a new one.",
        );
    };
    if claims.tenant != tunnel.tenant_id.0 {
        // The tunnel changed hands between the grant being issued and redeemed.
        return tunnels::page(
            StatusCode::FORBIDDEN,
            "Not your tunnel",
            "This tunnel belongs to a tenant you are not a member of.",
        );
    }

    let value = tunnels::mint_cookie(
        &state.cfg.session_secret,
        &tunnel.label,
        claims.user,
        claims.tenant,
        now(),
    );
    // HOST-ONLY: no `Domain` attribute, so this credential exists on exactly
    // one tunnel host. A domain-wide cookie would hand every tunnel in the zone
    // — including other tenants' — one credential.
    let cookie = Cookie::build((TUNNEL_COOKIE, value))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .secure(state.cfg.public_base_url.starts_with("https"))
        .max_age(cookie::time::Duration::seconds(tunnels::COOKIE_TTL_SECS))
        .build();
    (CookieJar::new().add(cookie), Redirect::temporary(&next)).into_response()
}

// ── Lifecycle: create, list, stop (MAIN-404 AC-1) ──────────────────────────

/// `POST /api/v1/tunnels` — open a tunnel to a port on a machine.
///
/// The label is DERIVED, never supplied: a name a person types is a name that
/// collides with another tenant's in the same zone and cannot be guessed back
/// (MAIN-402 AC-5). What the caller chooses is the port and the machine.
#[utoipa::path(post, path = "/api/v1/tunnels",
    operation_id = "create_tunnel",
    request_body = CreateTunnelRequest,
    responses((status = 200, body = TunnelView), (status = 400), (status = 403), (status = 404)))]
pub async fn create(
    State(state): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<CreateTunnelRequest>,
) -> ApiResult<Json<TunnelView>> {
    Ok(Json(open(&state, auth, req).await?))
}

/// The whole of "open a tunnel", callable in process.
///
/// Split from the handler so the MCP tools reach the naming, the zone check and
/// the node gate by CALLING them rather than by restating them (MAIN-11 AC-4) —
/// the one thing an agent-facing second door must not do.
pub async fn open(
    state: &AppState,
    auth: AuthCtx,
    req: CreateTunnelRequest,
) -> ApiResult<TunnelView> {
    let domain = zone(state)?;
    if req.port == 0 {
        return Err(ApiError::BadRequest("port must be 1-65535".into()));
    }

    // A node credential is already confined to one machine, so it need not name
    // it; a person must, because they can reach several.
    let node_id = match (req.node_id, auth.principal) {
        (Some(id), _) => id,
        (None, crate::auth::Principal::Node(self_id)) => self_id,
        (None, crate::auth::Principal::User) => {
            return Err(ApiError::BadRequest(
                "say which machine to tunnel from: node_id".into(),
            ))
        }
    };
    // The same gate session start uses: you may expose a port on a machine you
    // may run the thing behind it on. Not `require_node_owner` — a shared
    // operator node is the normal place work runs, and a tunnel you cannot open
    // to your own dev server there is a tunnel for nothing.
    auth.require_node_may_use(state, node_id).await?;

    let node = state
        .nodes
        .get(auth.tenant_id, node_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !state.registry.node_online(node_id) {
        return Err(ApiError::BadRequest(format!(
            "node {} is not connected, so a tunnel to it would answer nothing",
            node.name
        )));
    }

    // A session binds the tunnel's LIFETIME (AC-4), so it has to be one of this
    // tenant's, on this node. Otherwise a caller could hang their tunnel off
    // somebody else's terminal and have it torn down with theirs.
    let session = match req.session_id {
        Some(id) => {
            let s = state
                .sessions
                .by_id_unscoped(id)
                .await?
                .filter(|s| s.tenant_id == auth.tenant_id && s.node_id == node_id)
                .ok_or(ApiError::NotFound)?;
            Some(s)
        }
        None => None,
    };

    // The stem: the workspace a person is looking for, disambiguated by the
    // machine. With no session — a bare port on a box — there is no repository
    // to name it after, so the port does the naming instead of a blank.
    let workspace = match session.as_ref().and_then(|s| s.workspace_id) {
        Some(id) => state
            .workspaces
            .name_and_remote(id, auth.tenant_id)
            .await?
            .map(|(name, _)| name),
        None => None,
    };
    let stem = nook_proto::tunnel::subdomain_for(
        &workspace.unwrap_or_else(|| format!("port-{}", req.port)),
        &node.name,
    );

    let created_at = chrono::Utc::now();
    let tunnel = state.registry.open_tunnel_route(&stem, |label| Tunnel {
        label,
        tenant_id: auth.tenant_id,
        node_id,
        node_name: node.name.clone(),
        port: req.port,
        session_id: session.as_ref().map(|s| s.id),
        created_at,
    });

    events::record(
        state,
        auth.tenant_id,
        EventDraft::new("tunnel.opened")
            .node(node_id)
            .payload(serde_json::json!({
                "label": tunnel.label,
                "port": tunnel.port,
                "session_id": tunnel.session_id,
            })),
    )
    .await;
    tracing::info!(
        label = %tunnel.label, port = tunnel.port, node = %node.name,
        "tunnel opened"
    );

    Ok(view(state, &tunnel, Duration::ZERO, &domain))
}

/// `GET /api/v1/tunnels` — every tunnel open in the caller's tenant.
///
/// Readable by any member, and that is not a widening: membership of the
/// tunnel's tenant is the whole of a tunnel's access policy (MAIN-9 NG-6), so
/// everyone this lists it to could already reach every one of them.
#[utoipa::path(get, path = "/api/v1/tunnels",
    operation_id = "list_tunnels",
    responses((status = 200, body = [TunnelView])))]
pub async fn list(
    State(state): State<AppState>,
    auth: AuthCtx,
) -> ApiResult<Json<Vec<TunnelView>>> {
    Ok(Json(live(&state, auth)?))
}

/// Every tunnel open in `auth`'s tenant, callable in process (MAIN-11 AC-2).
pub fn live(state: &AppState, auth: AuthCtx) -> ApiResult<Vec<TunnelView>> {
    let domain = zone(state)?;
    Ok(state
        .registry
        .tunnels_for_tenant(auth.tenant_id)
        .iter()
        .map(|(t, idle)| view(state, t, *idle, &domain))
        .collect())
}

/// `DELETE /api/v1/tunnels/{label}` — close one.
#[utoipa::path(delete, path = "/api/v1/tunnels/{label}",
    operation_id = "stop_tunnel",
    params(("label" = String, Path,)),
    responses((status = 204), (status = 403), (status = 404)))]
pub async fn stop(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(label): Path<String>,
) -> ApiResult<StatusCode> {
    close(&state, auth, &label).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Close one tunnel by label, callable in process. A label this caller has no
/// tunnel under is [`ApiError::NotFound`] — which is what makes stopping a name
/// that does not exist an error rather than a silent success (MAIN-11 AC-3).
pub async fn close(state: &AppState, auth: AuthCtx, label: &str) -> ApiResult<()> {
    // A tunnel in another tenant is NOT FOUND, never forbidden: the labels share
    // one zone, and "you may not stop that" confirms somebody else has it.
    let tunnel = state
        .registry
        .tunnel_route(label)
        .filter(|t| t.tenant_id == auth.tenant_id)
        .ok_or(ApiError::NotFound)?;
    auth.require_node_may_use(state, tunnel.node_id).await?;

    // Re-read through the take: between the lookup above and here the sweep or
    // a node disconnect may have closed it, and reporting a close we did not
    // perform would be a small lie in the audit trail.
    let Some(gone) = state.registry.take_tunnel_route(label) else {
        return Ok(());
    };
    closed(state, &gone, "stopped").await;
    Ok(())
}

/// Announce a tunnel's end, wherever it came from — the API, the idle sweep, a
/// session exiting or a node disconnecting. One place, so an operator reading
/// the activity feed sees the same shape of record for all four and the reason
/// is the only difference.
pub async fn closed(state: &AppState, tunnel: &Tunnel, reason: &str) {
    tracing::info!(
        label = %tunnel.label, port = tunnel.port, node = %tunnel.node_name, reason,
        "tunnel closed"
    );
    events::record(
        state,
        tunnel.tenant_id,
        EventDraft::new("tunnel.closed")
            .node(tunnel.node_id)
            .payload(serde_json::json!({
                "label": tunnel.label,
                "port": tunnel.port,
                "session_id": tunnel.session_id,
                "reason": reason,
            })),
    )
    .await;
}

/// The configured zone, or the refusal that says the deployment has not set one
/// up. A 400 and not a 500: nothing is broken, the feature is simply off.
fn zone(state: &AppState) -> ApiResult<String> {
    state.cfg.tunnel_domain.clone().ok_or_else(|| {
        ApiError::BadRequest(
            "tunnels are not enabled here — this deployment has no TUNNEL_DOMAIN, which \
             also needs a wildcard DNS record and a certificate for *.<domain>"
                .into(),
        )
    })
}

fn view(state: &AppState, tunnel: &Tunnel, idle: Duration, domain: &str) -> TunnelView {
    TunnelView {
        url: origin(state, &tunnel.label, domain),
        label: tunnel.label.clone(),
        node_id: tunnel.node_id,
        node_name: tunnel.node_name.clone(),
        port: tunnel.port,
        session_id: tunnel.session_id,
        created_at: tunnel.created_at,
        idle_secs: idle.as_secs(),
    }
}

/// How often a replica re-announces that a tunnel is in use: a quarter of the
/// idle window, so a tunnel being used somewhere is refreshed everywhere at
/// least three times before any replica could consider it idle.
///
/// `None` with the sweep off — nothing reads the idle clock then, and a
/// quarter of zero would be a NOTIFY per second per busy tunnel for a number
/// nobody looks at.
fn announce_interval(state: &AppState) -> Option<Duration> {
    (state.cfg.tunnel_idle_secs > 0).then(|| Duration::from_secs(state.cfg.tunnel_idle_secs / 4))
}

// ── The proxy ──────────────────────────────────────────────────────────────

/// Forward one request to the node and stream its answer back.
async fn proxy(state: &AppState, tunnel: &Tunnel, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let path = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    let headers = tunnels::forwarded_headers(&parts.headers);
    let body = match axum::body::to_bytes(body, MAX_REQUEST_BODY).await {
        Ok(b) => b,
        Err(_) => {
            return tunnels::page(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Request too large",
                &format!(
                    "A tunnel buffers the request before sending it to the node, and this \
                     one is over the {} MiB limit.",
                    MAX_REQUEST_BODY / (1024 * 1024)
                ),
            )
        }
    };

    let request_id = Uuid::now_v7();
    // Registered BEFORE the send: the node can answer faster than we could
    // register, and a frame with no entry is dropped.
    let mut rx = state.registry.open_tunnel(request_id);
    let inflight = InFlight {
        registry: state.registry.clone(),
        request_id,
    };
    let sent = state.registry.send_to_node(
        tunnel.node_id,
        ControlToNode::TunnelRequest {
            version: nook_proto::TUNNEL_PROTOCOL_VERSION,
            request_id,
            port: tunnel.port,
            method: parts.method.as_str().to_string(),
            path,
            headers,
            body_b64: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &body),
        },
    );
    if !sent {
        return bad_gateway(format!(
            "Node {} is not connected to the control plane, so nothing can reach port {} \
             on it.",
            tunnel.node_name, tunnel.port
        ));
    }

    match tokio::time::timeout(HEAD_TIMEOUT, rx.recv()).await {
        Ok(Some(NodeToControl::TunnelResponse {
            status, headers, ..
        })) => stream_response(status, headers, rx, inflight),
        Ok(Some(NodeToControl::TunnelFailed { message, .. })) => bad_gateway(format!(
            "Nothing is listening on port {} on node {} — {message}.",
            tunnel.port, tunnel.node_name
        )),
        // The node's socket died, or it answered with something that is not the
        // head of a response. Both mean this exchange has no answer coming.
        Ok(_) => bad_gateway(format!(
            "Node {} ended the exchange for port {} without answering.",
            tunnel.node_name, tunnel.port
        )),
        Err(_) => tunnels::page(
            StatusCode::GATEWAY_TIMEOUT,
            "The node did not answer",
            &format!(
                "Node {} accepted the request for port {} but sent no response within {} \
                 seconds.",
                tunnel.node_name,
                tunnel.port,
                HEAD_TIMEOUT.as_secs()
            ),
        ),
    }
}

/// The 502 (AC-6): a tunnel that exists, pointing at something that does not
/// answer. Every caller names the port and the node in its own words, so a
/// crashed app reads differently from a machine that has gone — the two are
/// fixed in different places, and one page for both would say neither.
fn bad_gateway(detail: String) -> Response {
    tunnels::page(StatusCode::BAD_GATEWAY, "Nothing answered", &detail)
}

fn stream_response(
    status: u16,
    headers: Vec<(String, String)>,
    rx: tokio::sync::mpsc::Receiver<NodeToControl>,
    inflight: InFlight,
) -> Response {
    let mut builder = Response::builder().status(StatusCode::from_u16(status).unwrap_or(
        // A status the node made up is not a reason to drop the whole response.
        StatusCode::BAD_GATEWAY,
    ));
    for (name, value) in tunnels::upstream_headers(headers) {
        let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) else {
            continue;
        };
        builder = builder.header(name, value);
    }

    let stream = futures_util::stream::unfold(Some((rx, inflight)), |state| async move {
        let (mut rx, inflight) = state?;
        loop {
            match rx.recv().await {
                Some(NodeToControl::TunnelChunk { data_b64, last, .. }) => {
                    let Ok(bytes) = base64::Engine::decode(
                        &base64::engine::general_purpose::STANDARD,
                        &data_b64,
                    ) else {
                        return Some((
                            Err(std::io::Error::other("undecodable chunk from the node")),
                            None,
                        ));
                    };
                    if last {
                        // Dropping the state here also drops `inflight`, which
                        // is what releases the in-flight entry.
                        return Some((Ok(bytes), None));
                    }
                    if bytes.is_empty() {
                        continue;
                    }
                    return Some((Ok(bytes), Some((rx, inflight))));
                }
                // Mid-stream failure. The status and headers are already on the
                // wire, so the only honest signal left is to break the body:
                // a short response that claims to be complete is worse.
                Some(NodeToControl::TunnelFailed { message, .. }) => {
                    return Some((Err(std::io::Error::other(message)), None))
                }
                Some(_) => continue,
                None => return None,
            }
        }
    });

    builder
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| tunnels::page(StatusCode::BAD_GATEWAY, "Bad response", "unusable"))
}

// ── The upgrade path (MAIN-10) ─────────────────────────────────────────────

/// The one thing that makes a request an upgrade rather than a request.
///
/// Only `Upgrade` is read here. The rest of the handshake — the `Connection`
/// token, the version, the key — is [`WebSocketUpgrade`]'s to judge, and it
/// judges it with the same rules that will produce the `101`.
fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    headers
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("websocket"))
}

/// Negotiate a WebSocket end to end: this server to the visitor, the node to
/// whatever is listening on the port (AC-1).
///
/// The order is the whole design. The node dials FIRST and the `101` is only
/// written once it has answered, for two reasons: a visitor told the upgrade
/// succeeded and then handed a dead socket has no way to hear why, and the
/// subprotocol in the `101` is the upstream's answer — which cannot be known
/// before asking it.
async fn proxy_upgrade(state: AppState, tunnel: Tunnel, req: Request) -> Response {
    let (mut parts, _body) = req.into_parts();
    let path = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    // The same stripping the HTTP path does, and the same reason: the app
    // behind a tunnel must not be handed the credential that opens NookOS
    // (MAIN-403 AC-4, restated for the handshake by AC-6).
    let headers = tunnels::forwarded_headers(&parts.headers);

    // Taken before the node is asked. A handshake this server could not
    // complete is refused here rather than after an upstream socket exists
    // with nobody to hand it to.
    let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &state).await {
        Ok(u) => u,
        Err(rejection) => return rejection.into_response(),
    };

    let request_id = Uuid::now_v7();
    let mut rx = state.registry.open_tunnel(request_id);
    // Live from here, not from the pump. Every way out of this function that
    // is not a running socket — a refusal, a timeout, a visitor who vanished
    // between the `101` and the upgrade completing — has to release the
    // upstream the node may already hold (AC-4), and a guard is the only
    // shape that covers the paths nobody remembered to write.
    let guard = WsInFlight {
        registry: state.registry.clone(),
        node_id: tunnel.node_id,
        request_id,
        label: tunnel.label.clone(),
        close: Some((None, None)),
    };
    let sent = state.registry.send_to_node(
        tunnel.node_id,
        ControlToNode::TunnelUpgrade {
            version: nook_proto::TUNNEL_PROTOCOL_VERSION,
            request_id,
            port: tunnel.port,
            path,
            headers,
        },
    );
    if !sent {
        return bad_gateway(format!(
            "Node {} is not connected to the control plane, so nothing can reach port {} \
             on it.",
            tunnel.node_name, tunnel.port
        ));
    }

    let upstream = match tokio::time::timeout(HEAD_TIMEOUT, rx.recv()).await {
        Ok(Some(NodeToControl::TunnelUpgraded { headers, .. })) => headers,
        Ok(Some(NodeToControl::TunnelFailed { message, .. })) => {
            return bad_gateway(format!(
                "Nothing accepted a WebSocket on port {} on node {} — {message}.",
                tunnel.port, tunnel.node_name
            ))
        }
        Ok(_) => {
            return bad_gateway(format!(
                "Node {} ended the exchange for port {} without accepting the upgrade.",
                tunnel.node_name, tunnel.port
            ))
        }
        Err(_) => {
            return tunnels::page(
                StatusCode::GATEWAY_TIMEOUT,
                "The node did not answer",
                &format!(
                    "Node {} was asked for a WebSocket to port {} and did not answer within \
                     {} seconds. An agent older than this control plane answers no upgrade \
                     at all — check its version.",
                    tunnel.node_name,
                    tunnel.port,
                    HEAD_TIMEOUT.as_secs()
                ),
            )
        }
    };

    // Registered before the `101`, so a tunnel stopped from this moment on
    // finds the socket and drops it (AC-5). A tunnel stopped BEFORE it is why
    // this can fail: there is no longer anything to upgrade into.
    if !state.registry.attach_socket(&tunnel.label, request_id) {
        return tunnels::page(
            StatusCode::NOT_FOUND,
            "No such tunnel",
            "That tunnel ended while its WebSocket was being negotiated.",
        );
    }

    // Echoing the upstream's subprotocol is not cosmetic: a browser that asked
    // for one and is answered without it FAILS the handshake, which is exactly
    // how a Vite HMR socket dies (AC-7). `protocols` still filters against
    // what the visitor asked for, so an upstream naming something else is
    // answered with nothing rather than with a protocol nobody offered.
    let upgrade = match upstream
        .into_iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("sec-websocket-protocol"))
    {
        Some((_, chosen)) => upgrade.protocols([chosen]),
        None => upgrade,
    };
    let announce = announce_interval(&state);
    upgrade.on_upgrade(move |socket| pump(socket, rx, guard, announce))
}

/// Carry frames between the visitor's socket and the node's, until one of them
/// stops (AC-2, AC-4).
///
/// Nothing here is keyed by anything but `guard.request_id`, which is what
/// keeps concurrent sockets apart (AC-3): each has its own receiver, registered
/// under its own id, and neither this task nor the node ever consults a table
/// that could confuse them.
async fn pump(
    socket: WebSocket,
    mut rx: tokio::sync::mpsc::Receiver<NodeToControl>,
    mut guard: WsInFlight,
    announce: Option<Duration>,
) {
    let (mut sink, mut stream) = socket.split();
    let mut ping = tokio::time::interval(WS_PING_INTERVAL);
    ping.tick().await;

    loop {
        tokio::select! {
            _ = ping.tick() => {
                // A live socket has to hold its own tunnel open. The idle sweep
                // measures REQUESTS (MAIN-404 AC-3) and an HMR channel makes
                // none for hours, so without this the sweep closes the tunnel
                // under the socket that is in use — and AC-5 then dutifully
                // drops the socket.
                if let Some(after) = announce {
                    guard.registry.touch_tunnel(&guard.label, after);
                }
                if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
            frame = rx.recv() => match frame {
                Some(NodeToControl::TunnelWsData { data_b64, binary, .. }) => {
                    let Some(msg) = visitor_message(&data_b64, binary) else { break };
                    if sink.send(msg).await.is_err() {
                        break;
                    }
                }
                Some(NodeToControl::TunnelWsClose { code, reason, .. }) => {
                    let _ = sink.send(Message::Close(close_frame(code, reason))).await;
                    // The node ended it, so it needs telling nothing.
                    guard.close = None;
                    break;
                }
                Some(NodeToControl::TunnelFailed { message, .. }) => {
                    tracing::debug!(label = %guard.label, message, "tunnelled socket failed");
                    guard.close = None;
                    break;
                }
                Some(_) => continue,
                // Either the terminal frame was consumed above, or the tunnel
                // itself has gone (AC-5). In the second case the guard still
                // has a close to send, which is what releases the upstream —
                // and the visitor is told properly rather than left to read a
                // dropped connection as `1006`.
                None => {
                    let _ = sink.send(Message::Close(None)).await;
                    break;
                }
            },
            msg = stream.next() => match msg {
                Some(Ok(Message::Text(text))) => {
                    if !to_node(&guard, text.as_bytes(), false) {
                        break;
                    }
                }
                Some(Ok(Message::Binary(bytes))) => {
                    if !to_node(&guard, &bytes, true) {
                        break;
                    }
                }
                // axum answers a ping itself, and a pong is the answer to ours.
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                Some(Ok(Message::Close(frame))) => {
                    guard.close = Some(
                        frame
                            .map(|f| (Some(f.code), Some(f.reason.to_string())))
                            .unwrap_or((None, None)),
                    );
                    break;
                }
                Some(Err(_)) | None => break,
            },
        }
    }
}

/// One visitor frame, on its way to the node. `false` when the node is no
/// longer reachable, which ends the socket rather than dropping the frame.
fn to_node(guard: &WsInFlight, payload: &[u8], binary: bool) -> bool {
    guard.registry.send_to_node(
        guard.node_id,
        ControlToNode::TunnelWsData {
            request_id: guard.request_id,
            data_b64: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, payload),
            binary,
        },
    )
}

/// One node frame, as the visitor's socket will carry it — or `None` for a
/// payload this socket cannot carry, which ends it.
///
/// Base64 in both directions is what keeps AC-2's binary case honest: bytes
/// that are not valid UTF-8 survive the JSON frame untouched, and `binary`
/// carries the distinction the encoding erases.
fn visitor_message(data_b64: &str, binary: bool) -> Option<Message> {
    let bytes =
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data_b64).ok()?;
    Some(if binary {
        Message::Binary(bytes.into())
    } else {
        // A text frame's payload is UTF-8 by definition. One that is not can
        // only be a bug on the node, and breaking the socket says so where
        // substituting replacement characters would hide it.
        Message::Text(String::from_utf8(bytes).ok()?.into())
    })
}

fn close_frame(code: Option<u16>, reason: Option<String>) -> Option<axum::extract::ws::CloseFrame> {
    code.map(|code| axum::extract::ws::CloseFrame {
        code,
        reason: reason.unwrap_or_default().into(),
    })
}

/// Releases BOTH ends of a tunnelled WebSocket, however the pump ended.
///
/// The mirror of [`InFlight`], and it has to do more: an HTTP exchange the
/// control plane abandons is one the node finishes on its own, while a
/// WebSocket the control plane abandons is an upstream connection the node
/// holds open forever. So the close travels on the way out — from the pump
/// ending, from a refusal above it, and from the upgrade never happening at
/// all (AC-4).
struct WsInFlight {
    registry: Arc<Registry>,
    node_id: NodeId,
    request_id: Uuid,
    label: String,
    /// What to tell the node on the way out, or `None` once the node is the
    /// one that ended it and needs telling nothing.
    close: Option<(Option<u16>, Option<String>)>,
}

impl Drop for WsInFlight {
    fn drop(&mut self) {
        if let Some((code, reason)) = self.close.take() {
            self.registry.send_to_node(
                self.node_id,
                ControlToNode::TunnelWsClose {
                    request_id: self.request_id,
                    code,
                    reason,
                },
            );
        }
        self.registry.detach_socket(&self.label, self.request_id);
    }
}

/// Releases the in-flight entry when the response body is dropped.
///
/// Without this a client that disconnects mid-download leaves an entry behind
/// until the node sends a terminal frame — and a node that has died never
/// sends one, so the map would grow by one for every abandoned request.
struct InFlight {
    registry: Arc<Registry>,
    request_id: Uuid,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.registry.close_tunnel(self.request_id);
    }
}

// ── Small shared pieces ────────────────────────────────────────────────────

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn query(pairs: &[(&str, &str)]) -> String {
    let mut ser = form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        ser.append_pair(k, v);
    }
    ser.finish()
}

fn origin(state: &AppState, label: &str, domain: &str) -> String {
    // The zone's scheme follows the apex's: one deployment, one certificate
    // story. A tunnel served over http while the app is https would be a
    // downgrade nobody asked for.
    let scheme = if state.cfg.public_base_url.starts_with("https") {
        "https"
    } else {
        "http"
    };
    format!("{scheme}://{label}.{domain}")
}

fn authorize_url(state: &AppState, label: &str, next: &str) -> String {
    format!(
        "{}{AUTHORIZE_PATH}?{}",
        state.cfg.public_base_url.trim_end_matches('/'),
        query(&[("to", label), ("next", next)])
    )
}

/// Where a redirect may send the browser: an app-relative path, never a URL.
///
/// The same rule the sign-in flow uses. Without it, `next` is an open redirect —
/// and this one is reached with a NookOS credential in hand, which is exactly
/// the kind an attacker wants to bounce somebody through.
fn safe_next(next: Option<&str>) -> String {
    next.filter(|n| n.starts_with('/') && !n.starts_with("//"))
        .unwrap_or("/")
        .to_string()
}

fn next_of(uri: &axum::http::Uri) -> String {
    uri.path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_is_app_relative_or_the_root() {
        assert_eq!(safe_next(Some("/dashboard?tab=1")), "/dashboard?tab=1");
        // The open-redirect shapes: an absolute URL and a protocol-relative one.
        assert_eq!(safe_next(Some("https://evil.example/steal")), "/");
        assert_eq!(safe_next(Some("//evil.example/steal")), "/");
        assert_eq!(safe_next(None), "/");
    }
}
