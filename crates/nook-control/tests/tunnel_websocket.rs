//! MAIN-10 — WebSockets through a tunnel.
//!
//! `tunnel_surface.rs` drives the HTTP surface with `oneshot`, which cannot
//! reach an upgrade: the `101` is only half of one, and the half that matters —
//! the frames after it — exists only once a connection has actually been
//! upgraded. So these tests serve the shipping router on a loopback port and
//! dial it with a real WebSocket client.
//!
//! The NODE is still a channel. What is under test is this side of the tunnel:
//! that the upgrade is negotiated at all (AC-1), that payloads survive it
//! (AC-2), that concurrent sockets stay apart (AC-3), that either end closing
//! closes the other (AC-4), that a tunnel ending takes its sockets with it
//! (AC-5) and that an unauthenticated upgrade never reaches the node (AC-6).
//! The node's own half is tested in `nook-node`'s `conn::tests`, against a real
//! upstream server.

use axum::http::StatusCode;
use futures_util::{SinkExt, StreamExt};
use nook_control::state::AppState;
use nook_control::ws::registry::{NodeHandle, Tunnel};
use nook_infra::Config;
use nook_proto::{ControlToNode, NodeToControl};
use nook_testkit::TestBed;
use nook_types::{NodeId, TenantId, UserId};
use std::net::SocketAddr;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

const ZONE: &str = "tunnels.test";
const LABEL: &str = "hmr";
/// Long enough that a loaded machine does not fail a test, short enough that
/// "nothing arrives" is answered in a test's lifetime rather than a timeout's.
const BEAT: std::time::Duration = std::time::Duration::from_secs(5);

type Client =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The router on a real port, a tunnel pointing at a node that is a channel,
/// and a member's token to open it with.
struct Harness {
    state: AppState,
    addr: SocketAddr,
    node: mpsc::Receiver<ControlToNode>,
    token: String,
}

async fn harness(bed: &TestBed) -> Harness {
    let mut cfg = Config::for_test();
    cfg.tunnel_domain = Some(ZONE.to_string());
    let state = AppState::new(bed.db(), cfg, None).await;

    let tenant = bed.tenant("tun").await;
    let (user, person) = bed.user(tenant, "member").await;
    state
        .identity
        .grant_membership(tenant, user, "member")
        .await
        .expect("membership");
    let node = bed.node(tenant, person).await;

    let (tx, node_rx) = mpsc::channel(64);
    state.registry.register_node(
        node,
        NodeHandle {
            tenant_id: tenant,
            tx,
        },
    );
    state.registry.put_tunnel_route(Tunnel {
        label: LABEL.to_string(),
        tenant_id: tenant,
        node_id: node,
        node_name: "beelink".into(),
        port: 5173,
        session_id: None,
        created_at: chrono::Utc::now(),
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = nook_control::routes::build_router(state.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    Harness {
        token: token_for(&state, tenant, user).await,
        state,
        addr,
        node: node_rx,
    }
}

async fn token_for(state: &AppState, tenant: TenantId, user: UserId) -> String {
    let token = format!("nook_user_{}", Uuid::now_v7().simple());
    state
        .identity
        .create_user_token(nook_control::repo::identity::NewUserToken {
            id: Uuid::now_v7(),
            tenant,
            user_id: user,
            token_hash: nook_auth::hash_token(&token),
            name: "tunnel ws test".into(),
            expires_at: None,
        })
        .await
        .expect("mint token");
    token
}

/// An upgrade aimed at the tunnel host but dialled at the loopback port the
/// router is on — which is all wildcard DNS ever was: a `Host` header.
fn upgrade_request(
    addr: SocketAddr,
    path: &str,
    auth: Option<&str>,
    protocol: Option<&str>,
    cookie: Option<&str>,
) -> tokio_tungstenite::tungstenite::handshake::client::Request {
    let mut req = format!("ws://{addr}{path}").into_client_request().unwrap();
    let headers = req.headers_mut();
    headers.insert("host", format!("{LABEL}.{ZONE}").parse().unwrap());
    if let Some(token) = auth {
        headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
    }
    if let Some(protocol) = protocol {
        headers.insert("sec-websocket-protocol", protocol.parse().unwrap());
    }
    if let Some(cookie) = cookie {
        headers.insert("cookie", cookie.parse().unwrap());
    }
    req
}

/// Drive one upgrade to a live socket: dial, answer as the node would, and hand
/// back the visitor's end plus the request id everything after it is keyed by.
async fn connect(h: &mut Harness, path: &str, protocol: Option<&str>) -> (Client, Uuid) {
    let req = upgrade_request(h.addr, path, Some(&h.token), protocol, None);
    let dial = tokio::spawn(async move { tokio_tungstenite::connect_async(req).await });

    let ControlToNode::TunnelUpgrade { request_id, .. } = next_node_frame(&mut h.node).await else {
        panic!("an upgrade is forwarded as TunnelUpgrade");
    };
    h.state.registry.tunnel_frame(
        request_id,
        NodeToControl::TunnelUpgraded {
            request_id,
            version: nook_proto::TUNNEL_PROTOCOL_VERSION,
            headers: protocol
                .map(|p| vec![("sec-websocket-protocol".to_string(), p.to_string())])
                .unwrap_or_default(),
        },
    );

    let (socket, response) = tokio::time::timeout(BEAT, dial)
        .await
        .expect("the handshake completes once the node has answered")
        .unwrap()
        .expect("the upgrade is accepted");
    assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
    (socket, request_id)
}

async fn next_node_frame(node: &mut mpsc::Receiver<ControlToNode>) -> ControlToNode {
    tokio::time::timeout(BEAT, node.recv())
        .await
        .expect("a frame reaches the node")
        .expect("the node's lane is open")
}

async fn next_visitor_frame(socket: &mut Client) -> Message {
    loop {
        let msg = tokio::time::timeout(BEAT, socket.next())
            .await
            .expect("a frame reaches the visitor")
            .expect("the socket is open")
            .expect("a readable frame");
        // The keepalive is the transport's, not the app's.
        if !matches!(msg, Message::Ping(_) | Message::Pong(_)) {
            return msg;
        }
    }
}

fn b64(bytes: &[u8]) -> String {
    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes)
}

fn unb64(text: &str) -> Vec<u8> {
    base64::Engine::decode(&base64::engine::general_purpose::STANDARD, text).expect("base64")
}

/// AC-1, AC-2 and AC-6's second half, in one exchange: the upgrade is
/// negotiated end to end, frames cross both ways with non-UTF-8 payloads
/// intact, and no NookOS credential travels on the handshake.
#[tokio::test]
async fn an_upgrade_is_negotiated_end_to_end_and_carries_bytes_unchanged() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let mut h = harness(&bed).await;

    let req = upgrade_request(
        h.addr,
        "/hmr?token=abc",
        Some(&h.token),
        Some("vite-hmr"),
        Some(&format!("theme=dark; nook_session={}", Uuid::now_v7())),
    );
    let dial = tokio::spawn(async move { tokio_tungstenite::connect_async(req).await });

    let ControlToNode::TunnelUpgrade {
        request_id,
        port,
        path,
        headers,
        version,
    } = next_node_frame(&mut h.node).await
    else {
        panic!("an upgrade is forwarded as TunnelUpgrade, not as a request");
    };
    assert_eq!((version, port, path.as_str()), (1, 5173, "/hmr?token=abc"));
    assert_eq!(
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("sec-websocket-protocol"))
            .map(|(_, v)| v.as_str()),
        Some("vite-hmr"),
        "the app behind the tunnel is the one that picks a subprotocol, so it \
         has to be told what was asked for"
    );
    // AC-6: what the node receives carries the app's own cookie and none of ours.
    assert_eq!(
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("cookie"))
            .map(|(_, v)| v.as_str()),
        Some("theme=dark")
    );
    assert!(
        !headers.iter().any(|(_, v)| v.contains("nook_session")),
        "a NookOS credential reached the app on the handshake: {headers:?}"
    );

    h.state.registry.tunnel_frame(
        request_id,
        NodeToControl::TunnelUpgraded {
            request_id,
            version: nook_proto::TUNNEL_PROTOCOL_VERSION,
            headers: vec![("sec-websocket-protocol".into(), "vite-hmr".into())],
        },
    );

    let (mut socket, response) = tokio::time::timeout(BEAT, dial)
        .await
        .expect("the handshake completes")
        .unwrap()
        .expect("the visitor gets 101, not the HTTP-only fallback");
    assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
    assert_eq!(
        response
            .headers()
            .get("sec-websocket-protocol")
            .and_then(|v| v.to_str().ok()),
        Some("vite-hmr"),
        "a browser that asked for a subprotocol and is answered without one \
         fails the connection — which is how an HMR socket dies"
    );

    // Visitor → upstream. The binary payload is deliberately not valid UTF-8.
    let raw: &[u8] = &[0xff, 0x00, 0xfe, 0x80, 0xed];
    socket
        .send(Message::Binary(raw.to_vec().into()))
        .await
        .unwrap();
    let ControlToNode::TunnelWsData {
        data_b64,
        binary,
        request_id: id,
    } = next_node_frame(&mut h.node).await
    else {
        panic!("a visitor frame travels as TunnelWsData");
    };
    assert_eq!(id, request_id);
    assert!(binary);
    assert_eq!(unb64(&data_b64), raw);

    socket.send(Message::Text("hello".into())).await.unwrap();
    let ControlToNode::TunnelWsData {
        data_b64, binary, ..
    } = next_node_frame(&mut h.node).await
    else {
        panic!("a visitor frame travels as TunnelWsData");
    };
    assert!(
        !binary,
        "text stays text, or an app reads a Blob where it wanted a string"
    );
    assert_eq!(unb64(&data_b64), b"hello");

    // Upstream → visitor, same two shapes.
    h.state.registry.tunnel_frame(
        request_id,
        NodeToControl::TunnelWsData {
            request_id,
            data_b64: b64(raw),
            binary: true,
        },
    );
    assert_eq!(
        next_visitor_frame(&mut socket).await,
        Message::Binary(raw.to_vec().into())
    );

    h.state.registry.tunnel_frame(
        request_id,
        NodeToControl::TunnelWsData {
            request_id,
            data_b64: b64(b"reload"),
            binary: false,
        },
    );
    assert_eq!(
        next_visitor_frame(&mut socket).await,
        Message::Text("reload".into())
    );

    bed.teardown().await;
}

/// AC-3: two sockets through the one tunnel, and a frame for one is never seen
/// by the other. Keyed by request id in both directions, which is the whole
/// mechanism — so the test that matters is the one that reads nothing.
#[tokio::test]
async fn concurrent_sockets_never_see_each_others_frames() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let mut h = harness(&bed).await;

    let (mut first, first_id) = connect(&mut h, "/one", None).await;
    let (mut second, second_id) = connect(&mut h, "/two", None).await;
    assert_ne!(first_id, second_id);

    h.state.registry.tunnel_frame(
        first_id,
        NodeToControl::TunnelWsData {
            request_id: first_id,
            data_b64: b64(b"for the first"),
            binary: false,
        },
    );
    assert_eq!(
        next_visitor_frame(&mut first).await,
        Message::Text("for the first".into())
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(250), second.next())
            .await
            .is_err(),
        "the other socket must hear nothing at all"
    );

    // And the same going the other way: the id on the frame is the second's.
    second
        .send(Message::Text("from the second".into()))
        .await
        .unwrap();
    let ControlToNode::TunnelWsData {
        request_id,
        data_b64,
        ..
    } = next_node_frame(&mut h.node).await
    else {
        panic!("a visitor frame travels as TunnelWsData");
    };
    assert_eq!(request_id, second_id);
    assert_eq!(unb64(&data_b64), b"from the second");

    bed.teardown().await;
}

/// AC-4, both directions. A visitor closing is told to the node, so the upstream
/// connection is released rather than held by a tab nobody has open; a node
/// closing lands on the visitor as a close rather than a socket that goes quiet.
#[tokio::test]
async fn closing_either_end_closes_the_other() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let mut h = harness(&bed).await;

    let (mut visitor, request_id) = connect(&mut h, "/one", None).await;
    visitor.close(None).await.unwrap();
    let ControlToNode::TunnelWsClose {
        request_id: closed, ..
    } = next_node_frame(&mut h.node).await
    else {
        panic!("the node is told the visitor has gone, or it holds the upstream forever");
    };
    assert_eq!(closed, request_id);

    let (mut visitor, request_id) = connect(&mut h, "/two", None).await;
    h.state.registry.tunnel_frame(
        request_id,
        NodeToControl::TunnelWsClose {
            request_id,
            code: Some(1001),
            reason: Some("going away".into()),
        },
    );
    let Message::Close(Some(frame)) = next_visitor_frame(&mut visitor).await else {
        panic!("the upstream closing reaches the visitor as a close");
    };
    assert_eq!(u16::from(frame.code), 1001);
    assert_eq!(frame.reason.as_str(), "going away");

    bed.teardown().await;
}

/// AC-5: a tunnel that stops existing takes its live sockets with it, rather
/// than leaving them attached to a name that no longer resolves. Stopping is
/// the sharpest of the four ways it happens — the sweep, a session exiting and
/// a node disconnecting all withdraw the route through the same path.
#[tokio::test]
async fn a_stopped_tunnel_drops_the_sockets_riding_it() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let mut h = harness(&bed).await;

    let (mut visitor, request_id) = connect(&mut h, "/hmr", None).await;
    assert!(h.state.registry.has_sockets(LABEL));

    assert!(h.state.registry.take_tunnel_route(LABEL).is_some());

    let ControlToNode::TunnelWsClose {
        request_id: closed, ..
    } = next_node_frame(&mut h.node).await
    else {
        panic!("the node is told to release the upstream of a tunnel that has ended");
    };
    assert_eq!(closed, request_id);
    assert!(
        tokio::time::timeout(BEAT, visitor.next())
            .await
            .expect("the visitor's socket is closed, not left hanging")
            .is_none_or(|m| matches!(m, Ok(Message::Close(_)) | Err(_))),
        "a socket outliving its tunnel is exactly what AC-5 forbids"
    );
    assert!(!h.state.registry.has_sockets(LABEL));

    bed.teardown().await;
}

/// AC-6: an upgrade with no credential is refused HERE, and nothing about it
/// reaches the node — not even the dial that would open an upstream socket.
///
/// It is answered rather than redirected on purpose: a WebSocket handshake does
/// not follow redirects, so the bounce a navigation gets would cost the visitor
/// the reason and gain them nothing.
#[tokio::test]
async fn an_unauthenticated_upgrade_never_reaches_the_node() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let mut h = harness(&bed).await;

    let anonymous = upgrade_request(h.addr, "/hmr", None, Some("vite-hmr"), None);
    let err = tokio_tungstenite::connect_async(anonymous)
        .await
        .expect_err("an anonymous upgrade is refused");
    let tokio_tungstenite::tungstenite::Error::Http(response) = err else {
        panic!("the refusal is an HTTP answer, not a transport error: {err}");
    };
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // A member of another tenant is refused too, and just as silently.
    let other = bed.tenant("other").await;
    let (stranger, _) = bed.user(other, "member").await;
    h.state
        .identity
        .grant_membership(other, stranger, "member")
        .await
        .expect("membership");
    let theirs = token_for(&h.state, other, stranger).await;
    let err = tokio_tungstenite::connect_async(upgrade_request(
        h.addr,
        "/hmr",
        Some(&theirs),
        None,
        None,
    ))
    .await
    .expect_err("another tenant's member is refused");
    let tokio_tungstenite::tungstenite::Error::Http(response) = err else {
        panic!("the refusal is an HTTP answer: {err}");
    };
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(250), h.node.recv())
            .await
            .is_err(),
        "a refused upgrade must not reach the node at all"
    );

    bed.teardown().await;
}

/// A tunnel whose node has gone answers the 502 the HTTP path answers, rather
/// than leaving a browser on a handshake nobody will finish.
#[tokio::test]
async fn an_upgrade_to_a_disconnected_node_is_refused_not_hung() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let h = harness(&bed).await;
    let gone = NodeId(Uuid::now_v7());
    let mut tunnel = h.state.registry.tunnel_route(LABEL).expect("the tunnel");
    tunnel.node_id = gone;
    h.state.registry.put_tunnel_route(tunnel);

    let err = tokio_tungstenite::connect_async(upgrade_request(
        h.addr,
        "/hmr",
        Some(&h.token),
        None,
        None,
    ))
    .await
    .expect_err("nothing can answer, so the handshake fails");
    let tokio_tungstenite::tungstenite::Error::Http(response) = err else {
        panic!("the refusal is an HTTP answer: {err}");
    };
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

    bed.teardown().await;
}
