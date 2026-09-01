//! MAIN-26 — the authenticated request path.
//!
//! Three correctness properties, DB-backed:
//!   AC-1 — `AuthCtx` resolves the session and its membership in ONE query,
//!          keeping 401 (no session) distinct from 403 (grant revoked).
//!   AC-2 — a tenant switch is auditable from BOTH tenants' event logs.
//!   AC-3 — `switch_tenant` tells a user token (400, browser-only) from a cookie
//!          session whose row vanished mid-request (401).

use axum::extract::{FromRequestParts, State};
use axum::Json;
use nook_control::auth::{AuthCtx, Principal};
use nook_control::error::ApiError;
use nook_control::state::AppState;
use nook_db::dialect::time_math;
use nook_db::{params, Db, EnginePool};
use nook_testkit::TestBed;
use nook_types::{AuthSessionId, SwitchTenantRequest, TenantId, UserId};
use uuid::Uuid;

async fn seed_tenant(bed: &TestBed) -> TenantId {
    let id = TenantId::new();
    bed.db()
        .exec(
            "INSERT INTO tenants (id, slug, name) VALUES ($1, $2, $2)",
            params![id, format!("authp-{}", Uuid::now_v7().simple())],
        )
        .await
        .expect("seed tenant");
    id
}

/// A member user in `tenant`, linked to `person`, with a live grant.
async fn seed_member(bed: &TestBed, tenant: TenantId, person: Uuid) -> UserId {
    let id = UserId::new();
    bed.db()
        .exec(
            "INSERT INTO users (id, tenant_id, display_name, email, role, person_id)
         VALUES ($1, $2, 'P', $3, 'member', $4)",
            params![
                id,
                tenant,
                format!("{}@example.test", Uuid::now_v7().simple()),
                person
            ],
        )
        .await
        .expect("user");
    grant(bed, tenant, id).await;
    id
}

async fn grant(bed: &TestBed, tenant: TenantId, user: UserId) {
    bed.db()
        .exec(
            "INSERT INTO tenant_members (id, tenant_id, principal_type, principal_id, role)
         VALUES ($1, $2, 'user', $3, 'member')",
            params![Uuid::new_v4(), tenant, user],
        )
        .await
        .expect("grant");
}

async fn revoke(bed: &TestBed, tenant: TenantId, user: UserId) {
    bed.db()
        .exec(
            "DELETE FROM tenant_members WHERE tenant_id = $1 AND principal_id = $2",
            params![tenant, user],
        )
        .await
        .expect("revoke");
}

async fn seed_session(bed: &TestBed, user: UserId, tenant: TenantId) -> Uuid {
    let sid = Uuid::new_v4();
    let expires = time_math(bed.engine()).now_plus("1 hour");
    bed.db()
        .exec(
            &format!(
                "INSERT INTO sessions_auth (id, user_id, tenant_id, expires_at)
         VALUES ($1, $2, $3, {expires})"
            ),
            params![sid, user, tenant],
        )
        .await
        .expect("session");
    sid
}

/// Extract `AuthCtx` from a request bearing `nook_session=<sid>` — exercises the
/// real cookie path, including the folded session+membership query.
async fn extract(state: &AppState, sid: Uuid) -> Result<AuthCtx, ApiError> {
    let req = axum::http::Request::builder()
        .header(axum::http::header::COOKIE, format!("nook_session={sid}"))
        .body(axum::body::Body::empty())
        .unwrap();
    let (mut parts, _) = req.into_parts();
    AuthCtx::from_request_parts(&mut parts, state).await
}

/// AC-1: one query, but 401 (no session) and 403 (grant revoked) stay distinct.
#[tokio::test]
async fn one_query_keeps_401_no_session_distinct_from_403_revoked_grant() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let t = seed_tenant(&bed).await;
    let me = seed_member(&bed, t, Uuid::new_v4()).await;
    let sid = seed_session(&bed, me, t).await;

    // A live session + grant resolves.
    let ok = extract(&state, sid).await.expect("member resolves");
    assert_eq!(ok.tenant_id, t);
    assert_eq!(ok.user_id, me);
    assert!(ok.cookie_session, "a cookie session is marked as one");

    // Grant revoked, session still valid → 403 (NOT 401): the fold must not
    // collapse a live-session-without-grant into "no session".
    revoke(&bed, t, me).await;
    let forbidden = extract(&state, sid).await.expect_err("revoked → error");
    assert!(
        matches!(forbidden, ApiError::Forbidden),
        "a revoked grant on a live session is 403, got {forbidden:?}"
    );

    // No session row at all → 401.
    grant(&bed, t, me).await; // re-grant, so only the session is missing
    let gone = extract(&state, Uuid::new_v4())
        .await
        .expect_err("unknown session → error");
    assert!(
        matches!(gone, ApiError::Unauthorized),
        "a missing session is 401, got {gone:?}"
    );

    // An expired session → 401 (the `expires_at > now()` guard survived the fold).
    let past = time_math(bed.engine()).now_minus("1 minute");
    bed.db()
        .exec(
            &format!("UPDATE sessions_auth SET expires_at = {past} WHERE id = $1"),
            params![sid],
        )
        .await
        .unwrap();
    let expired = extract(&state, sid).await.expect_err("expired → error");
    assert!(
        matches!(expired, ApiError::Unauthorized),
        "an expired session is 401, got {expired:?}"
    );

    bed.teardown().await;
}

/// AC-3: a user token cannot switch (browser-only 400), decided by the explicit
/// marker — not inferred from a zero-row UPDATE.
#[tokio::test]
async fn a_user_token_switch_is_refused_browser_only() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let t = seed_tenant(&bed).await;
    let me = seed_member(&bed, t, Uuid::new_v4()).await;

    // A user token: Principal::User, but NOT a cookie session.
    let token_ctx = AuthCtx {
        session_id: AuthSessionId::new(),
        user_id: me,
        tenant_id: t,
        principal: Principal::User,
        cookie_session: false,
    };
    let err = nook_control::routes::auth::switch_tenant(
        State(state),
        token_ctx,
        Json(SwitchTenantRequest { tenant_id: t }),
    )
    .await
    .expect_err("a token cannot switch");
    assert!(
        matches!(err, ApiError::BadRequest(_)),
        "a user token switch is a browser-only 400, got {err:?}"
    );

    bed.teardown().await;
}

/// AC-3: a cookie session that authenticated but whose row vanished mid-request
/// is 401 (session gone), not the token 400.
#[tokio::test]
async fn a_vanished_cookie_session_switch_is_401() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let t = seed_tenant(&bed).await;
    let me = seed_member(&bed, t, Uuid::new_v4()).await;

    // A cookie session (marker true), member of `t`, but its sessions_auth row
    // does not exist — so the membership check passes and the UPDATE hits 0 rows.
    let ctx = AuthCtx {
        session_id: AuthSessionId::new(), // no row for this id
        user_id: me,
        tenant_id: t,
        principal: Principal::User,
        cookie_session: true,
    };
    let err = nook_control::routes::auth::switch_tenant(
        State(state),
        ctx,
        Json(SwitchTenantRequest { tenant_id: t }),
    )
    .await
    .expect_err("a vanished session cannot switch");
    assert!(
        matches!(err, ApiError::Unauthorized),
        "a vanished cookie session is 401, got {err:?}"
    );

    bed.teardown().await;
}

/// AC-2: a real crossing is auditable from BOTH tenants' event logs.
#[tokio::test]
async fn a_switch_is_audited_from_both_tenants() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let person = Uuid::new_v4();
    let a = seed_tenant(&bed).await; // source
    let b = seed_tenant(&bed).await; // destination
    let me_a = seed_member(&bed, a, person).await;
    let _me_b = seed_member(&bed, b, person).await; // same person, member of b too
    let sid = seed_session(&bed, me_a, a).await;

    let ctx = AuthCtx {
        session_id: AuthSessionId(sid),
        user_id: me_a,
        tenant_id: a,
        principal: Principal::User,
        cookie_session: true,
    };
    let _ok = nook_control::routes::auth::switch_tenant(
        State(state),
        ctx,
        Json(SwitchTenantRequest { tenant_id: b }),
    )
    .await
    .expect("the switch succeeds");

    // The whole `user.tenant_switched` payload in a tenant's log (exactly one
    // is written per side in this scenario).
    // An owned pool clone the per-tenant closure can capture without borrowing
    // `bed` (teardown needs `&mut bed` after these run).
    let db: EnginePool = bed.db();
    let payload = |t: TenantId| {
        let db = db.clone();
        async move {
            let (n,): (i64,) = db
                .query_one(
                    "SELECT count(*) FROM events WHERE tenant_id = $1 AND kind = 'user.tenant_switched'",
                    params![t],
                )
                .await
                .unwrap();
            let (p,): (serde_json::Value,) = db
                .query_one(
                    "SELECT payload FROM events WHERE tenant_id = $1 AND kind = 'user.tenant_switched'",
                    params![t],
                )
                .await
                .unwrap();
            (n, p)
        }
    };
    let (dest_count, dest) = payload(b).await;
    let (source_count, source) = payload(a).await;

    bed.teardown().await;

    assert_eq!(dest_count, 1, "the destination records the arrival");
    assert_eq!(source_count, 1, "the source records the departure (AC-2)");

    // Arrival, in B: self-describing — direction "in", both tenants named
    // (AC-1). `from`/`to` are the same on both sides; only `direction` differs.
    assert_eq!(dest["direction"], "in", "arrival is direction=in");
    assert_eq!(
        dest["from_tenant"],
        a.to_string(),
        "arrival names the source"
    );
    assert_eq!(
        dest["to_tenant"],
        b.to_string(),
        "arrival names the destination"
    );

    // Departure, in A: direction "out", the same two tenants (AC-2).
    assert_eq!(source["direction"], "out", "departure is direction=out");
    assert_eq!(
        source["from_tenant"],
        a.to_string(),
        "departure names the source"
    );
    assert_eq!(
        source["to_tenant"],
        b.to_string(),
        "departure names the destination"
    );

    // The ad-hoc keys are gone; direction alone disambiguates (AC-3).
    for p in [&dest, &source] {
        assert!(p.get("tenant_id").is_none(), "old `tenant_id` key removed");
        assert!(
            p.get("left_for_tenant").is_none(),
            "old `left_for_tenant` key removed"
        );
    }
}

/// AC-2/NG-4: re-selecting the tenant you are already in records the arrival but
/// NO departure — there is no crossing to record.
#[tokio::test]
async fn reselecting_the_current_tenant_records_no_departure() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let person = Uuid::new_v4();
    let a = seed_tenant(&bed).await;
    let me_a = seed_member(&bed, a, person).await;
    let sid = seed_session(&bed, me_a, a).await;

    let ctx = AuthCtx {
        session_id: AuthSessionId(sid),
        user_id: me_a,
        tenant_id: a,
        principal: Principal::User,
        cookie_session: true,
    };
    // Switch INTO the tenant already active.
    let _ok = nook_control::routes::auth::switch_tenant(
        State(state),
        ctx,
        Json(SwitchTenantRequest { tenant_id: a }),
    )
    .await
    .expect("re-selecting the current tenant succeeds");

    let directions: Vec<(String,)> = bed
        .db()
        .query_all(
            "SELECT payload->>'direction' FROM events
         WHERE tenant_id = $1 AND kind = 'user.tenant_switched'",
            params![a],
        )
        .await
        .unwrap();

    bed.teardown().await;

    // Exactly the arrival, and nothing with direction "out".
    assert_eq!(directions.len(), 1, "only the arrival is recorded");
    assert_eq!(
        directions[0].0, "in",
        "and it is an arrival, not a departure"
    );
}

/// MAIN-48 AC-4: the shared `nook-auth` crate resolves a valid session and a
/// valid bearer token to the RIGHT user+tenant, and refuses an unknown one —
/// the same code chat uses, tested here where the schema is provisioned.
#[tokio::test]
async fn nook_auth_resolves_session_and_bearer() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let t = seed_tenant(&bed).await;
    let me = seed_member(&bed, t, Uuid::new_v4()).await;
    let sid = seed_session(&bed, me, t).await;

    // Session cookie → the right user + tenant, marked cookie_session.
    let s = nook_auth::resolve_session(&bed.db(), sid)
        .await
        .expect("valid session resolves");
    assert_eq!(s.user_id, me.0);
    assert_eq!(s.tenant_id, t.0);
    assert!(s.cookie_session);

    // A bearer token → the right user + tenant, NOT a cookie session.
    let token = "nook_user_test_abc123";
    bed.db()
        .exec(
            "INSERT INTO user_tokens (id, tenant_id, user_id, token_hash, name)
         VALUES ($1, $2, $3, $4, 'test')",
            params![Uuid::new_v4(), t, me, nook_auth::hash_token(token)],
        )
        .await
        .unwrap();
    let b = nook_auth::resolve_bearer(&bed.db(), token)
        .await
        .expect("valid token resolves");
    assert_eq!(b.user_id, me.0);
    assert_eq!(b.tenant_id, t.0);
    assert!(!b.cookie_session);

    // Unknown credentials are refused.
    assert!(matches!(
        nook_auth::resolve_session(&bed.db(), Uuid::new_v4()).await,
        Err(nook_auth::AuthError::Unauthorized)
    ));
    assert!(matches!(
        nook_auth::resolve_bearer(&bed.db(), "nook_user_nope").await,
        Err(nook_auth::AuthError::Unauthorized)
    ));

    bed.teardown().await;
}
