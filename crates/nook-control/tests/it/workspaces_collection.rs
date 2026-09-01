//! What `GET /api/v1/workspaces` answers, through the real router (MAIN-606).
//!
//! The collection used to have a paged twin at `/workspaces/page` and return a
//! bare unbounded array itself. Folding the two together is a WIRE change, and
//! only a request can tell that the fold happened: a handler test would pass
//! just as happily with `/page` still mounted beside it, and with the envelope
//! wrapped around a query that walks nothing.
//!
//! So every assertion here goes through `build_router` with a session cookie —
//! which also makes the authorization assertion mean something (AC-8): what a
//! caller with no credential gets is a property of the stack, not the handler.
//!
//! Needs a database: `NOOK_REQUIRE_DB=1` in the suite.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;
use tower::ServiceExt;
use uuid::Uuid;

/// A member of `tenant` with a live session cookie — what every read below is
/// made as.
async fn signed_in(bed: &TestBed, tenant: TenantId) -> Uuid {
    let (user, _) = bed.user(tenant, "owner").await;
    bed.db()
        .exec(
            "INSERT INTO tenant_members (id, tenant_id, principal_type, principal_id, role)
             VALUES ($1, $2, 'user', $3, 'owner')",
            params![Uuid::new_v4(), tenant, user],
        )
        .await
        .expect("grant");
    let sid = Uuid::new_v4();
    let expires = nook_db::dialect::time_math(bed.db().engine()).now_plus_scaled("$4", "1 hour");
    bed.db()
        .exec(
            &format!(
                "INSERT INTO sessions_auth (id, user_id, tenant_id, expires_at)
                 VALUES ($1, $2, $3, {expires})"
            ),
            params![sid, user, tenant, 1_i32],
        )
        .await
        .expect("session");
    sid
}

/// `GET uri` as `sid`, or with no credential at all when `sid` is `None`.
async fn get(bed: &TestBed, sid: Option<Uuid>, uri: &str) -> (StatusCode, serde_json::Value) {
    let mut req = Request::builder().uri(uri);
    if let Some(sid) = sid {
        req = req.header(header::COOKIE, format!("nook_session={sid}"));
    }
    let resp = nook_control::routes::build_router(bed.app_state().await)
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .expect("the route answers");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 256 * 1024)
        .await
        .expect("body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

/// The page's rows, as `WorkspaceDetail` — which also proves the envelope holds
/// the same row shape the bare array did (NG-3).
fn rows(body: &serde_json::Value) -> Vec<WorkspaceDetail> {
    serde_json::from_value(body["rows"].clone()).expect("rows are workspace details")
}

#[tokio::test]
async fn the_collection_answers_the_envelope_with_no_parameters() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ws-collection").await;
    let sid = signed_in(&bed, tenant).await;
    let ws = bed.workspace(tenant).await;

    let (status, body) = get(&bed, Some(sid), "/api/v1/workspaces").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.get("rows").is_some() && body.get("next_cursor").is_some(),
        "a bare read is the paged envelope, not an array: {body}"
    );
    let rows = rows(&body);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].workspace.id, ws);
    // One page holds this tenant whole, so the walk is over — `null`, not a
    // cursor onto an empty page.
    assert!(body["next_cursor"].is_null());

    bed.teardown().await;
}

#[tokio::test]
async fn a_limit_and_its_cursor_walk_the_collection_once() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ws-walk").await;
    let sid = signed_in(&bed, tenant).await;
    let mut made = Vec::new();
    for _ in 0..3 {
        made.push(bed.workspace(tenant).await);
    }

    let mut seen: Vec<WorkspaceId> = Vec::new();
    let mut uri = "/api/v1/workspaces?limit=1".to_string();
    for _ in 0..10 {
        let (status, body) = get(&bed, Some(sid), &uri).await;
        assert_eq!(status, StatusCode::OK);
        let page = rows(&body);
        assert!(page.len() <= 1, "the limit is honoured");
        seen.extend(page.iter().map(|r| r.workspace.id));
        match body["next_cursor"].as_str() {
            // The cursor goes back VERBATIM — the contract's opacity is the
            // whole reason this endpoint can change mechanism later.
            Some(c) => uri = format!("/api/v1/workspaces?limit=1&after={c}"),
            None => break,
        }
    }

    made.sort();
    let mut walked = seen.clone();
    walked.sort();
    walked.dedup();
    assert_eq!(walked.len(), seen.len(), "no row is handed out twice");
    assert_eq!(walked, made, "and no row is skipped across a page boundary");

    bed.teardown().await;
}

#[tokio::test]
async fn a_page_never_carries_another_tenants_workspace() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let mine = bed.tenant("ws-mine").await;
    let theirs = bed.tenant("ws-theirs").await;
    let sid = signed_in(&bed, mine).await;
    let ours = bed.workspace(mine).await;
    for _ in 0..3 {
        bed.workspace(theirs).await;
    }

    // Asked for far more rows than this tenant has, so a leak has room to show.
    let (status, body) = get(&bed, Some(sid), "/api/v1/workspaces?limit=200").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        rows(&body)
            .iter()
            .map(|r| r.workspace.id)
            .collect::<Vec<_>>(),
        vec![ours]
    );

    bed.teardown().await;
}

#[tokio::test]
async fn the_collection_still_refuses_a_caller_with_no_session() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ws-auth").await;
    bed.workspace(tenant).await;

    let (status, _) = get(&bed, None, "/api/v1/workspaces").await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "the surviving handler gates exactly as the two it replaced did"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn the_page_route_is_gone() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ws-page-gone").await;
    let sid = signed_in(&bed, tenant).await;

    let (status, _) = get(&bed, Some(sid), "/api/v1/workspaces/page").await;
    // Removed, not deprecated (AC-3) — and with the static segment gone, the
    // path falls through to `/workspaces/{id}`, where "page" is a malformed
    // workspace id. So the refusal is a 400 rather than the 404 the card's
    // walkthrough guessed at; routing a dead path to a nicer status would mean
    // mounting something to serve it, which is the shim NG-6 forbids.
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "nothing serves /workspaces/page any more"
    );

    bed.teardown().await;
}
