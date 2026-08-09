//! `/api/v1/config` is signed-in, and that is the whole reason it is not beside
//! `/auth/providers` (MAIN-171): it carries the operator's Giphy key, so an
//! anonymous caller must not be able to read it. That property rests entirely on
//! the handler's `AuthCtx` argument — which a refactor could drop with nothing
//! going red. This pins it through the real router.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use nook_db::dialect::time_math;
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::{TenantId, UserId};
use tower::ServiceExt;
use uuid::Uuid;

/// A tenant, a member of it, and a live cookie session — the membership grant
/// matters as much as the session row: without it the extractor answers 403.
async fn seed_signed_in(bed: &TestBed, hint: &str) -> Uuid {
    let tenant = bed.tenant(hint).await;
    let (user, _) = bed.user(tenant, "member").await;
    bed.db()
        .exec(
            "INSERT INTO tenant_members (id, tenant_id, principal_type, principal_id, role)
             VALUES ($1, $2, 'user', $3, 'member')",
            params![Uuid::new_v4(), tenant, user],
        )
        .await
        .expect("grant");
    seed_session(bed, user, tenant).await
}

async fn seed_session(bed: &TestBed, user: UserId, tenant: TenantId) -> Uuid {
    let sid = Uuid::new_v4();
    // The expiry goes through the dialect seam production uses (MAIN-438), not
    // a literal `now() + interval '1 hour'` — SQLite has no `interval`, and this
    // suite runs on both engines.
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
    sid
}

#[tokio::test]
async fn app_config_is_401_anonymous_and_carries_the_giphy_key_when_signed_in() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let mut cfg = bed.config();
    cfg.giphy_key = Some("gk-secret".into());
    let state = nook_control::AppState::new(bed.db(), cfg, None).await;
    let app = nook_control::routes::build_router(state);

    let anon = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("config responds");
    assert_eq!(
        anon.status(),
        StatusCode::UNAUTHORIZED,
        "an anonymous caller must not read the operator's key"
    );

    let sid = seed_signed_in(&bed, "cfg").await;
    let signed_in = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/config")
                .header(header::COOKIE, format!("nook_session={sid}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("config responds");
    assert_eq!(signed_in.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(signed_in.into_body(), 64 * 1024)
        .await
        .expect("body");
    let got: nook_types::AppConfig = serde_json::from_slice(&bytes).expect("AppConfig");
    assert_eq!(got.giphy_key.as_deref(), Some("gk-secret"));

    bed.teardown().await;
}

/// AC-3's server half: no key configured is a 200 with `null`, never an error —
/// the frontend decides "no GIF button" from the value, so the request that
/// tells it so has to succeed.
#[tokio::test]
async fn app_config_reports_a_null_key_when_the_operator_has_none() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let app = nook_control::routes::build_router(state);

    let sid = seed_signed_in(&bed, "cfg-none").await;
    let res = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/config")
                .header(header::COOKIE, format!("nook_session={sid}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("config responds");
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024)
        .await
        .expect("body");
    let got: nook_types::AppConfig = serde_json::from_slice(&bytes).expect("AppConfig");
    assert_eq!(got.giphy_key, None);

    bed.teardown().await;
}
