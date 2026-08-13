//! MAIN-11: the tunnel tools. Driven at the `NookBackend` layer with a resolved
//! `McpCaller` — the call the tools make once `mcp_auth` has resolved the
//! caller — so the tenant scoping under test is the real one.
//!
//! The point of the surface is that it is a second DOOR and not a second
//! implementation, so the assertions worth making are the ones a reimplementation
//! would fail: the same label the endpoint derives, the same collision suffix,
//! the same refusal when the deployment has no zone, and a not-found for a
//! session in somebody else's tenant.
//!
//! Needs Postgres: set `DATABASE_URL` (`NOOK_REQUIRE_DB=1` in the suite).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use nook_control::mcp_backend::McpBackend;
use nook_control::state::AppState;
use nook_control::ws::registry::NodeHandle;
use nook_db::{params, Db};
use nook_infra::Config;
use nook_mcp::{McpCaller, NookBackend};
use nook_proto::ControlToNode;
use nook_testkit::TestBed;
use nook_types::*;
use tower::ServiceExt;
use uuid::Uuid;

const ZONE: &str = "tunnels.test";
const PORT: u16 = 5173;

async fn state(bed: &TestBed, zone: Option<&str>) -> AppState {
    let mut cfg = Config::for_test();
    cfg.tunnel_domain = zone.map(str::to_string);
    AppState::new(bed.db(), cfg, None).await
}

/// A tenant with a member, an ONLINE node they own, a workspace and a session on
/// it — everything opening a tunnel needs, and the caller identity the tools get.
struct Fixture {
    caller: McpCaller,
    session: SessionId,
    node_name: String,
    workspace_name: String,
}

async fn fixture(bed: &TestBed, state: &AppState, workspace_name: &str) -> Fixture {
    let tenant = bed.tenant("mcptun").await;
    let (user, person) = bed.user(tenant, "member").await;
    state
        .identity
        .grant_membership(tenant, user, "member")
        .await
        .expect("membership");
    let node = bed.node(tenant, person).await;
    let (tx, _rx) = tokio::sync::mpsc::channel::<ControlToNode>(4);
    state.registry.register_node(
        node,
        NodeHandle {
            tenant_id: tenant,
            tx,
        },
    );
    let node_name = state
        .nodes
        .get(tenant, node)
        .await
        .expect("node")
        .expect("node row")
        .name;

    let workspace = WorkspaceId::new();
    bed.db()
        .exec(
            "INSERT INTO workspaces (id, tenant_id, name, slug) VALUES ($1, $2, $3, $3)",
            params![workspace, tenant, workspace_name.to_string()],
        )
        .await
        .expect("workspace");

    let session = SessionId::new();
    bed.db()
        .exec(
            "INSERT INTO sessions (id, tenant_id, workspace_id, node_id, name, runtime, status)
             VALUES ($1, $2, $3, $4, 'dev', 'bash', 'running')",
            params![session, tenant, workspace, node],
        )
        .await
        .expect("session");

    Fixture {
        caller: McpCaller {
            person_id: person,
            user_id: user,
            tenant_id: tenant,
        },
        session,
        node_name,
        workspace_name: workspace_name.to_string(),
    }
}

fn backend(state: &AppState) -> McpBackend {
    McpBackend {
        state: state.clone(),
    }
}

/// AC-1/AC-2/AC-3, and AC-4's naming: the tool opens a tunnel on the SESSION's
/// node without being told which machine that is, under the label the shared
/// derivation produces; it lists with every field `nook tunnel list` prints; and
/// stopping it takes the URL down for good.
#[tokio::test]
async fn the_tools_open_list_and_stop_a_tunnel() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = state(&bed, Some(ZONE)).await;
    let f = fixture(&bed, &state, "api").await;
    let mcp = backend(&state);

    let opened = mcp
        .open_tunnel(f.caller.clone(), f.session.to_string(), PORT)
        .await
        .expect("open");
    assert_eq!(
        opened.label,
        nook_proto::tunnel::subdomain_for(&f.workspace_name, &f.node_name),
        "the label is the endpoint's own derivation, not a second one"
    );
    assert_eq!(opened.url, format!("http://{}.{ZONE}", opened.label));
    assert_eq!(opened.port, PORT);
    assert_eq!(opened.session_id, Some(f.session));
    assert_eq!(opened.node_name, f.node_name);

    let listed = mcp.list_tunnels(f.caller.clone()).await.expect("list");
    assert_eq!(listed.len(), 1, "{listed:?}");
    let row = &listed[0];
    assert_eq!(row.label, opened.label);
    assert_eq!(row.url, opened.url);
    assert_eq!(row.node_name, f.node_name);
    assert_eq!(row.port, PORT);
    assert_eq!(row.session_id, Some(f.session));
    assert_eq!(row.created_at, opened.created_at, "the age is reported");

    mcp.stop_tunnel(f.caller.clone(), opened.label.clone())
        .await
        .expect("stop");
    assert!(state.registry.tunnel_route(&opened.label).is_none());
    assert!(mcp
        .list_tunnels(f.caller.clone())
        .await
        .expect("list")
        .is_empty());

    // AC-3: stopping it a second time is an error, not a silent success.
    let err = mcp
        .stop_tunnel(f.caller, opened.label)
        .await
        .expect_err("gone is gone");
    assert!(
        matches!(
            err.downcast_ref::<nook_control::error::ApiError>(),
            Some(nook_control::error::ApiError::NotFound)
        ),
        "{err}"
    );

    bed.teardown().await;
}

/// AC-3: a name nothing was ever opened under is the same clear error.
#[tokio::test]
async fn stopping_a_name_that_never_existed_is_an_error() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = state(&bed, Some(ZONE)).await;
    let f = fixture(&bed, &state, "api").await;

    let err = backend(&state)
        .stop_tunnel(f.caller, "no-such-tunnel".into())
        .await
        .expect_err("no such tunnel");
    assert!(
        matches!(
            err.downcast_ref::<nook_control::error::ApiError>(),
            Some(nook_control::error::ApiError::NotFound)
        ),
        "{err}"
    );

    bed.teardown().await;
}

/// NG-3: a session in another tenant is NOT FOUND, and nothing is opened. The
/// caller learns nothing about whether that session exists.
#[tokio::test]
async fn a_session_in_another_tenant_is_refused() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = state(&bed, Some(ZONE)).await;
    let mine = fixture(&bed, &state, "mine").await;
    let theirs = fixture(&bed, &state, "theirs").await;

    let err = backend(&state)
        .open_tunnel(mine.caller.clone(), theirs.session.to_string(), PORT)
        .await
        .expect_err("not my session");
    assert!(
        matches!(
            err.downcast_ref::<nook_control::error::ApiError>(),
            Some(nook_control::error::ApiError::NotFound)
        ),
        "{err}"
    );
    assert!(
        backend(&state)
            .list_tunnels(mine.caller)
            .await
            .expect("list")
            .is_empty(),
        "and nothing was opened"
    );

    bed.teardown().await;
}

/// AC-4: with no `TUNNEL_DOMAIN` the tool refuses with the very message the CLI
/// gives, because it is the endpoint's refusal and not a copy of it.
#[tokio::test]
async fn with_no_zone_configured_the_tool_refuses_like_the_cli() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = state(&bed, None).await;
    let f = fixture(&bed, &state, "api").await;
    let mcp = backend(&state);

    let err = mcp
        .open_tunnel(f.caller.clone(), f.session.to_string(), PORT)
        .await
        .expect_err("no zone, no tunnel");
    assert!(err.to_string().contains("TUNNEL_DOMAIN"), "{err}");
    assert!(
        matches!(
            err.downcast_ref::<nook_control::error::ApiError>(),
            Some(nook_control::error::ApiError::BadRequest(_))
        ),
        "{err}"
    );

    // The listing says the same thing rather than answering "no tunnels".
    let err = mcp.list_tunnels(f.caller).await.expect_err("no zone");
    assert!(err.to_string().contains("TUNNEL_DOMAIN"), "{err}");

    bed.teardown().await;
}

/// AC-4, the assertion the whole card rests on: a tunnel opened through MCP and
/// one opened through `POST /api/v1/tunnels` — the call `nook tunnel` makes —
/// land on the same stem, and the second one collides the same way.
#[tokio::test]
async fn mcp_and_the_cli_endpoint_name_tunnels_identically() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = state(&bed, Some(ZONE)).await;
    let f = fixture(&bed, &state, "api").await;

    let through_mcp = backend(&state)
        .open_tunnel(f.caller.clone(), f.session.to_string(), PORT)
        .await
        .expect("open through mcp");

    let token = token_for(&state, f.caller.tenant_id, f.caller.user_id).await;
    let res = nook_control::routes::build_router(state.clone())
        .oneshot(post_json(
            &token,
            "/api/v1/tunnels",
            serde_json::json!({
                "port": PORT,
                "node_id": state.registry.tunnel_route(&through_mcp.label).expect("open").node_id,
                "session_id": f.session,
            }),
        ))
        .await
        .expect("the router answers");
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024)
        .await
        .expect("a readable body");
    let through_http: TunnelView = serde_json::from_slice(&bytes).expect("a tunnel");

    let stem = nook_proto::tunnel::subdomain_for(&f.workspace_name, &f.node_name);
    assert_eq!(through_mcp.label, stem, "first one takes the stem");
    assert_eq!(
        through_http.label,
        format!("{stem}-2"),
        "and the second collides onto the numbered suffix, whichever door it came through"
    );

    bed.teardown().await;
}

/// A live `nook_user_` token for `user`, returned in plaintext — the credential
/// `nook tunnel` presents.
async fn token_for(state: &AppState, tenant: TenantId, user: UserId) -> String {
    let token = format!("nook_user_{}", Uuid::now_v7().simple());
    state
        .identity
        .create_user_token(nook_control::repo::identity::NewUserToken {
            id: Uuid::now_v7(),
            tenant,
            user_id: user,
            token_hash: nook_auth::hash_token(&token),
            name: "mcp tunnel test".into(),
            expires_at: None,
        })
        .await
        .expect("mint token");
    token
}

fn post_json(token: &str, path: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header(axum::http::header::HOST, "localhost:8080")
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}
