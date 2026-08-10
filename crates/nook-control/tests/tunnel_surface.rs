//! MAIN-403 — the tunnel HTTP surface, driven through the router that ships.
//!
//! The rules themselves are unit-tested in `nook_control::tunnels` with no
//! database at all. What can only be checked here is that they are WIRED: that
//! a tunnel host is claimed ahead of the API (AC-1), that the grant flow's two
//! halves fit together across the subdomain boundary (AC-2), that a bearer
//! token is honoured (AC-3), and — the one worth the whole file — that neither
//! NookOS cookie survives into the request the node receives (AC-4). That last
//! assertion is made on the FORWARDED frame, not on the answer, because a
//! response cannot show you what was sent.
//!
//! No wildcard DNS is involved: a `Host` header is all "which tunnel" ever
//! meant, so the surface is exercisable with an ordinary request.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use nook_control::state::AppState;
use nook_control::tunnels::{self, TUNNEL_COOKIE};
use nook_control::ws::registry::{NodeHandle, Tunnel};
use nook_infra::Config;
use nook_proto::{ControlToNode, NodeToControl};
use nook_testkit::TestBed;
use nook_types::{NodeId, TenantId, UserId};
use tower::ServiceExt;
use uuid::Uuid;

const ZONE: &str = "tunnels.test";

async fn state(bed: &TestBed) -> AppState {
    let mut cfg = Config::for_test();
    cfg.tunnel_domain = Some(ZONE.to_string());
    AppState::new(bed.db(), cfg, None).await
}

fn get(host: &str, path: &str) -> Request<Body> {
    Request::builder()
        .uri(path)
        .header(axum::http::header::HOST, host)
        .body(Body::empty())
        .unwrap()
}

async fn send(state: &AppState, req: Request<Body>) -> axum::response::Response {
    nook_control::routes::build_router(state.clone())
        .oneshot(req)
        .await
        .expect("the router answers")
}

fn location(res: &axum::response::Response) -> String {
    res.headers()
        .get(axum::http::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

/// A tunnel on `<label>.tunnels.test`, plus the tenant and member behind it.
struct Fixture {
    tenant: TenantId,
    user: UserId,
    node: NodeId,
    label: String,
}

async fn fixture(bed: &TestBed, state: &AppState, label: &str) -> Fixture {
    let tenant = bed.tenant("tun").await;
    let (user, person) = bed.user(tenant, "member").await;
    state
        .identity
        .grant_membership(tenant, user, "member")
        .await
        .expect("membership");
    let node = bed.node(tenant, person).await;
    state.registry.put_tunnel_route(Tunnel {
        label: label.to_string(),
        tenant_id: tenant,
        node_id: node,
        node_name: "beelink".into(),
        port: 3000,
        session_id: None,
    });
    Fixture {
        tenant,
        user,
        node,
        label: label.to_string(),
    }
}

/// A live `nook_user_` token for `user`, returned in plaintext.
async fn token_for(state: &AppState, tenant: TenantId, user: UserId) -> String {
    let token = format!("nook_user_{}", Uuid::now_v7().simple());
    state
        .identity
        .create_user_token(nook_control::repo::identity::NewUserToken {
            id: Uuid::now_v7(),
            tenant,
            user_id: user,
            token_hash: nook_auth::hash_token(&token),
            name: "tunnel test".into(),
            expires_at: None,
        })
        .await
        .expect("mint token");
    token
}

/// AC-1: a host under the zone is answered by the tunnel surface WHATEVER its
/// path says. `/api/v1/auth/me` is the sharpest case — an existing API route,
/// which a tunnel visitor must never reach.
#[tokio::test]
async fn a_tunnel_host_never_falls_through_to_the_api() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = state(&bed).await;

    let res = send(&state, get(&format!("nothing.{ZONE}"), "/api/v1/auth/me")).await;
    assert_eq!(
        res.status(),
        StatusCode::NOT_FOUND,
        "an unknown tunnel is 404, not the API's 401"
    );
    assert_eq!(
        res.headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/html; charset=utf-8"),
        "a person who typed a URL gets a page, not an error object"
    );

    // The apex is untouched: the same request on the API's own host still
    // reaches the API and is refused by it.
    let res = send(&state, get("localhost:8080", "/api/v1/auth/me")).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    bed.teardown().await;
}

/// AC-5: the reserved prefix answers on a tunnel host and is never proxied, so
/// the app behind a tunnel cannot serve its own grant endpoint and read the
/// grants meant for NookOS.
#[tokio::test]
async fn the_reserved_prefix_belongs_to_nook_on_a_tunnel_host() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = state(&bed).await;
    let f = fixture(&bed, &state, "api").await;

    // No node is connected, so anything that got as far as forwarding would be
    // a 502 — this being a 404 is what proves it was answered here instead.
    let res = send(
        &state,
        get(&format!("{}.{ZONE}", f.label), "/__nook/anything"),
    )
    .await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    bed.teardown().await;
}

/// AC-3 and AC-7's decision, through the real surface: no credential redirects,
/// a non-member is refused, and a member's bearer token is let through with no
/// browser flow at all.
#[tokio::test]
async fn the_three_answers_a_tunnel_gives_a_request() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = state(&bed).await;
    let f = fixture(&bed, &state, "api").await;
    let host = format!("{}.{ZONE}", f.label);

    // No credential → bounced to the apex to authorize, carrying where it was
    // going. Not a 401 body: the person is probably signed in already.
    let res = send(&state, get(&host, "/dashboard?tab=1")).await;
    assert_eq!(res.status(), StatusCode::TEMPORARY_REDIRECT);
    let to = location(&res);
    assert!(
        to.contains("/api/v1/tunnels/authorize") && to.contains("to=api"),
        "{to}"
    );
    assert!(to.contains("next=%2Fdashboard%3Ftab%3D1"), "{to}");

    // A POST is NOT bounced: a 307 preserves the method, so the redirect would
    // re-post the body at an endpoint that takes none.
    let res = send(
        &state,
        Request::builder()
            .method("POST")
            .uri("/save")
            .header(axum::http::header::HOST, &host)
            .body(Body::from("x"))
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    // A member of ANOTHER tenant: signed in, resolvable, and not entitled.
    let other = bed.tenant("other").await;
    let (stranger, _) = bed.user(other, "member").await;
    state
        .identity
        .grant_membership(other, stranger, "member")
        .await
        .expect("membership");
    let theirs = token_for(&state, other, stranger).await;
    let res = send(
        &state,
        Request::builder()
            .uri("/")
            .header(axum::http::header::HOST, &host)
            .header(
                axum::http::header::AUTHORIZATION,
                format!("Bearer {theirs}"),
            )
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    // A revoked or unknown token is answered, never redirected — bouncing a
    // script into a sign-in page helps nobody.
    let res = send(
        &state,
        Request::builder()
            .uri("/")
            .header(axum::http::header::HOST, &host)
            .header(
                axum::http::header::AUTHORIZATION,
                "Bearer nook_user_not_a_real_token",
            )
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    // The member's own token: past the gate. Nothing is connected for this
    // node, so the answer is the 502 page — which names the port and the node,
    // and is only reachable from the far side of the access check.
    let mine = token_for(&state, f.tenant, f.user).await;
    let res = send(
        &state,
        Request::builder()
            .uri("/")
            .header(axum::http::header::HOST, &host)
            .header(axum::http::header::AUTHORIZATION, format!("Bearer {mine}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
    let body = body_string(res).await;
    assert!(
        body.contains("port 3000") && body.contains("beelink"),
        "{body}"
    );

    bed.teardown().await;
}

/// AC-2: the cross-subdomain grant, both halves. The apex verifies the session
/// and the membership and hands back a grant; the tunnel host exchanges it for
/// a host-only cookie — once.
#[tokio::test]
async fn the_grant_crosses_the_subdomain_boundary_exactly_once() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = state(&bed).await;
    let f = fixture(&bed, &state, "api").await;
    let host = format!("{}.{ZONE}", f.label);

    // Signed out at the apex: sign in first, and come back here afterwards.
    let res = send(
        &state,
        get("localhost:8080", "/api/v1/tunnels/authorize?to=api&next=/x"),
    )
    .await;
    assert_eq!(res.status(), StatusCode::TEMPORARY_REDIRECT);
    assert!(
        location(&res).contains("/login?next="),
        "{}",
        location(&res)
    );

    // Signed in as a member: a grant, aimed back at the tunnel host.
    let session = nook_control::auth::create_auth_session(&state, f.user, f.tenant)
        .await
        .expect("session");
    let res = send(
        &state,
        Request::builder()
            .uri("/api/v1/tunnels/authorize?to=api&next=/x")
            .header(axum::http::header::HOST, "localhost:8080")
            .header(
                axum::http::header::COOKIE,
                format!("nook_session={session}"),
            )
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::TEMPORARY_REDIRECT);
    let grant_url = location(&res);
    assert!(
        grant_url.starts_with(&format!("http://{host}/__nook/tunnel/grant?")),
        "{grant_url}"
    );

    // The tunnel host exchanges it for its own cookie and continues.
    let path = &grant_url[format!("http://{host}").len()..];
    let res = send(&state, get(&host, path)).await;
    assert_eq!(res.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(location(&res), "/x");
    let set = res
        .headers()
        .get(axum::http::header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(set.starts_with(TUNNEL_COOKIE), "{set}");
    assert!(
        !set.to_ascii_lowercase().contains("domain="),
        "the tunnel cookie is HOST-ONLY, or one tunnel's credential opens the zone: {set}"
    );
    assert!(set.contains("HttpOnly"), "{set}");

    // Single use: the copy left in history, a referrer or a proxy log is dead.
    let res = send(&state, get(&host, path)).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    // And the cookie it minted opens the tunnel — no node connected, so the
    // proof it was accepted is that the answer is the 502 rather than a bounce.
    let cookie = set.split(';').next().unwrap().to_string();
    let res = send(
        &state,
        Request::builder()
            .uri("/")
            .header(axum::http::header::HOST, &host)
            .header(axum::http::header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::BAD_GATEWAY);

    bed.teardown().await;
}

/// AC-4, the reason this card was split out: what the node receives carries
/// none of NookOS's own credentials.
///
/// Asserted on the FRAME the control plane sends, which is the only place the
/// question can be answered — the response says nothing about what was
/// forwarded, and the node is where the leak would land.
#[tokio::test]
async fn neither_nook_cookie_nor_a_nook_token_reaches_the_app_behind_the_tunnel() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = state(&bed).await;
    let f = fixture(&bed, &state, "api").await;
    let host = format!("{}.{ZONE}", f.label);

    // Stand in for the node's socket: whatever the control plane would send it
    // arrives here instead.
    let (tx, mut node_rx) = tokio::sync::mpsc::channel(8);
    state.registry.register_node(
        f.node,
        NodeHandle {
            tenant_id: f.tenant,
            tx,
        },
    );

    let cookie_value = tunnels::mint_cookie(
        &state.cfg.session_secret,
        &f.label,
        f.user.0,
        f.tenant.0,
        chrono::Utc::now().timestamp(),
    );
    let req = Request::builder()
        .method("POST")
        .uri("/upload?x=1")
        .header(axum::http::header::HOST, &host)
        .header(
            axum::http::header::COOKIE,
            format!(
                "theme=dark; nook_session={}; {TUNNEL_COOKIE}={cookie_value}",
                Uuid::now_v7()
            ),
        )
        .body(Body::from("hello"))
        .unwrap();

    let state2 = state.clone();
    let call = tokio::spawn(async move { send(&state2, req).await });

    let frame = tokio::time::timeout(std::time::Duration::from_secs(5), node_rx.recv())
        .await
        .expect("the request reaches the node")
        .expect("a frame");
    let ControlToNode::TunnelRequest {
        request_id,
        port,
        method,
        path,
        headers,
        body_b64,
        ..
    } = frame
    else {
        panic!("the tunnel forwards a TunnelRequest, got {frame:?}");
    };
    assert_eq!(
        (port, method.as_str(), path.as_str()),
        (3000, "POST", "/upload?x=1")
    );
    assert_eq!(
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &body_b64).unwrap(),
        b"hello"
    );

    let cookie = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("cookie"))
        .map(|(_, v)| v.clone())
        .expect("the app's own cookies still travel");
    assert_eq!(cookie, "theme=dark");
    assert!(
        !headers
            .iter()
            .any(|(_, v)| v.contains("nook_session") || v.contains(TUNNEL_COOKIE)),
        "a NookOS credential reached the app behind the tunnel: {headers:?}"
    );

    // Answer as the node would, so the exchange completes rather than timing
    // out — and the response comes back through the streaming path.
    state.registry.tunnel_frame(
        request_id,
        NodeToControl::TunnelResponse {
            request_id,
            version: nook_proto::TUNNEL_PROTOCOL_VERSION,
            status: 201,
            headers: vec![
                ("content-type".into(), "text/plain".into()),
                // An app trying to widen a cookie onto the apex, which is where
                // NookOS's own session lives.
                ("set-cookie".into(), "app=1; Domain=tunnels.test".into()),
                ("transfer-encoding".into(), "chunked".into()),
            ],
        },
    );
    state.registry.tunnel_frame(
        request_id,
        NodeToControl::TunnelChunk {
            request_id,
            seq: 0,
            data_b64: base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                b"upstream body",
            ),
            last: false,
        },
    );
    state.registry.tunnel_frame(
        request_id,
        NodeToControl::TunnelChunk {
            request_id,
            seq: 1,
            data_b64: String::new(),
            last: true,
        },
    );

    let res = call.await.expect("the request completes");
    assert_eq!(res.status(), StatusCode::CREATED);
    let set = res
        .headers()
        .get(axum::http::header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert_eq!(
        set, "app=1",
        "an app behind a tunnel must not set a cookie the apex will send back"
    );
    assert!(
        res.headers().get("transfer-encoding").is_none(),
        "hop-by-hop headers describe a connection that does not exist here"
    );
    assert_eq!(body_string(res).await, "upstream body");

    bed.teardown().await;
}

async fn body_string(res: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024)
        .await
        .expect("a readable body");
    String::from_utf8(bytes.to_vec()).expect("utf-8")
}
