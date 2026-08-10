//! A uuid SQLite stores as TEXT decodes, and `POST /api/v1/nodes/join` proves
//! it end to end (MAIN-472).
//!
//! `tenants.org_id` is the one uuid column no bind ever writes: both `0001`
//! tracks give it a literal DEFAULT, which on SQLite lands as 36-char TEXT
//! rather than the 16-byte blob `DbValue::Uuid` encodes. Decoding it as a bare
//! `uuid::Uuid` therefore failed with `ParseByteLength { len: 36 }`, and the
//! join route — which reads that row for the tenant slug it hands back — 500'd
//! on SQLite, so a desktop install's bundled node could never enrol.
//!
//! The tolerance lives in nook-db's `FromDbColumn for uuid::Uuid` and nothing
//! here names an engine: these are the same assertions on both legs, which is
//! what makes them evidence that the decode is central rather than patched at
//! this call site.

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use nook_control::auth::{AuthCtx, Principal};
use nook_control::state::AppState;
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;
use tower::ServiceExt;
use uuid::Uuid;

/// The org a tenant belongs to until it is moved — written by the schema
/// DEFAULT on both engines, and the value the TEXT decode has to produce.
const DEFAULT_ORG: &str = "00000000-0000-0000-0000-0000000000a1";

/// Mint a join token as `minter`, returning its plaintext.
async fn mint_token(state: &AppState, minter: UserId, tenant: TenantId) -> String {
    let auth = AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: minter,
        tenant_id: tenant,
        principal: Principal::User,
        cookie_session: false,
    };
    nook_control::routes::join::create_join_token(State(state.clone()), auth)
        .await
        .expect("mint token")
        .0
        .token
}

/// The decode itself, on whichever engine this leg runs: the row is read the
/// way production reads it — one `Option<Uuid>` in a tuple — and the tenant was
/// created without naming `org_id`, so the value under test is the one the
/// schema wrote and not one this test bound.
#[tokio::test]
async fn a_default_written_org_id_decodes_as_a_uuid() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping text-uuid decode test — no DATABASE_URL");
        return;
    };
    let tenant = bed.tenant("orgid").await;

    let (org, slug): (Option<Uuid>, String) = bed
        .db()
        .query_one(
            "SELECT org_id, slug FROM tenants WHERE id = $1",
            params![tenant],
        )
        .await
        .expect("org_id decodes as a uuid");

    assert_eq!(org, Some(Uuid::parse_str(DEFAULT_ORG).unwrap()));
    assert!(!slug.is_empty(), "the rest of the row still reads");

    bed.teardown().await;
}

/// AC-2: the measured 500. The route is exercised through the real router
/// rather than the handler, because the failure was a decode error surfacing as
/// a 500 status — the thing a joining node actually saw.
#[tokio::test]
async fn a_node_joins_and_is_told_its_tenant_slug() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping node-join test — no DATABASE_URL");
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("join").await;
    let (owner, _) = bed.user(tenant, "owner").await;
    let token = mint_token(&state, owner, tenant).await;

    let expected_slug: String = bed
        .db()
        .query_scalar("SELECT slug FROM tenants WHERE id = $1", params![tenant])
        .await
        .expect("tenant slug");

    let res = nook_control::routes::build_router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/nodes/join")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "token": token,
                        "name": "",
                        "hostname": "someones-laptop",
                        "platform": "linux",
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .expect("the join route answers");

    let status = res.status();
    let body = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .expect("join body");
    assert_eq!(
        status,
        StatusCode::OK,
        "join failed: {}",
        String::from_utf8_lossy(&body)
    );

    let joined: JoinResponse = serde_json::from_slice(&body).expect("a JoinResponse");
    // The slug is the field `tenant_org_and_slug` reads beside `org_id`, so an
    // empty one would mean the row was skipped rather than decoded.
    assert_eq!(joined.tenant_slug, expected_slug);
    assert!(joined.node_token.starts_with("nook_node_"));
    assert_eq!(
        joined.node_name, "someones-laptop",
        "name falls back to hostname"
    );

    bed.teardown().await;
}
