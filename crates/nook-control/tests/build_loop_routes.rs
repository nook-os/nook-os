//! MAIN-641: the build loop is ONE route, and the paths it replaced are gone.
//!
//! Every assertion here is about the ROUTE TABLE rather than about a handler,
//! because that is the half a handler test cannot reach: `set_build_loop` can
//! be perfect while `/build-loop-settings` is still wired to it, and the
//! epic's Direction forbids the alias (NG-3). The requests are signed in on
//! purpose — an anonymous one is refused by the auth layer before matching, so
//! it would report 401 for a path that exists and a path that does not alike,
//! and prove nothing about either.
//!
//! Engine-neutral (MAIN-264): nothing here names a `sqlx` type.

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use nook_db::dialect::time_math;
use nook_db::{params, Db};
use nook_testkit::TestBed;
use tower::ServiceExt;
use uuid::Uuid;

/// A tenant, a member of it, a workspace and a live cookie session. The
/// membership grant matters as much as the session row: without it the
/// extractor answers 403 and every status below would be that instead.
async fn seed(bed: &TestBed) -> (Uuid, nook_types::WorkspaceId) {
    let tenant = bed.tenant("blroutes").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    bed.db()
        .exec(
            "INSERT INTO tenant_members (id, tenant_id, principal_type, principal_id, role)
             VALUES ($1, $2, 'user', $3, 'member')",
            params![Uuid::new_v4(), tenant, user],
        )
        .await
        .expect("grant");

    let sid = Uuid::new_v4();
    // Through the dialect seam production uses (MAIN-438): SQLite has no
    // `interval`, and this suite runs on both engines.
    let expires = time_math(bed.db().engine()).now_plus_scaled("$4", "1 hour");
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
    (sid, ws)
}

fn signed_in(
    method: Method,
    uri: &str,
    sid: Uuid,
    body: Option<serde_json::Value>,
) -> Request<Body> {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::COOKIE, format!("nook_session={sid}"));
    match body {
        None => req.body(Body::empty()).unwrap(),
        Some(v) => req
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
    }
}

/// AC-1 and AC-4: the two paths the build loop now has, and the two it no
/// longer does.
#[tokio::test]
async fn the_old_paths_are_gone_and_the_new_ones_answer() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let app = nook_control::routes::build_router(state);
    let (sid, ws) = seed(&bed).await;

    for path in [
        format!("/api/v1/workspaces/{ws}/build-loop"),
        format!("/api/v1/workspaces/{ws}/build-loop/status"),
    ] {
        let res = app
            .clone()
            .oneshot(signed_in(Method::GET, &path, sid, None))
            .await
            .expect("routed");
        assert_eq!(res.status(), StatusCode::OK, "{path} answers");
    }

    // Not aliased, not redirected — gone (NG-3).
    for path in [
        format!("/api/v1/workspaces/{ws}/build-loop-settings"),
        format!("/api/v1/workspaces/{ws}/build-loop-status"),
    ] {
        let res = app
            .clone()
            .oneshot(signed_in(Method::GET, &path, sid, None))
            .await
            .expect("responds");
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "{path} is deleted");
    }

    bed.teardown().await;
}

/// AC-3: PATCH is the write, and no PUT remains on the path. A 405 rather than
/// a 404 is exactly the distinction worth pinning — the path exists, the verb
/// does not, which is what "partial semantics on a replace verb" ended as.
#[tokio::test]
async fn the_write_is_patch_and_put_is_not_routed() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let app = nook_control::routes::build_router(state);
    let (sid, ws) = seed(&bed).await;
    let path = format!("/api/v1/workspaces/{ws}/build-loop");

    let patched = app
        .clone()
        .oneshot(signed_in(
            Method::PATCH,
            &path,
            sid,
            Some(serde_json::json!({ "concurrency": 3 })),
        ))
        .await
        .expect("routed");
    assert_eq!(patched.status(), StatusCode::OK);

    let put = app
        .clone()
        .oneshot(signed_in(
            Method::PUT,
            &path,
            sid,
            Some(serde_json::json!({ "concurrency": 3 })),
        ))
        .await
        .expect("responds");
    assert_eq!(
        put.status(),
        StatusCode::METHOD_NOT_ALLOWED,
        "the path is there; PUT is not"
    );

    // And the PATCH landed: the read reports the declaration it just wrote.
    let read = app
        .oneshot(signed_in(Method::GET, &path, sid, None))
        .await
        .expect("routed");
    assert_eq!(read.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(read.into_body(), 64 * 1024)
        .await
        .expect("body");
    let got: nook_types::BuildLoopSettings = serde_json::from_slice(&bytes).expect("declaration");
    assert_eq!(got.concurrency, Some(3));

    bed.teardown().await;
}
