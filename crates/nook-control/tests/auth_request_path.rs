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
use nook_types::{AuthSessionId, SwitchTenantRequest, TenantId, UserId};
use sqlx::PgPool;
use uuid::Uuid;

mod common;
use common::test_pool;

fn test_config() -> nook_control::config::Config {
    nook_control::config::Config {
        app_env: "test".into(),
        bind: "127.0.0.1:0".into(),
        shutdown_grace_secs: 25,
        agent_bind: "127.0.0.1:0".into(),
        agent_public_url: None,
        agent_tls_cert: None,
        agent_tls_key: None,
        public_base_url: "http://localhost:8080".into(),
        web_origin: "http://localhost:5173".into(),
        database_url: std::env::var("DATABASE_URL").unwrap_or_default(),
        oidc_issuer_url: None,
        oidc_client_id: None,
        oidc_device_client_id: None,
        oidc_device_authorization_endpoint: None,
        oidc_client_secret: None,
        oidc_redirect_url: None,
        oidc_scopes: "openid profile email".into(),
        session_secret: "0".repeat(64),
        session_ttl_hours: 168,
        default_tenant_name: "dev".into(),
        auth_dev_mode: true,
        mcp_token: None,
        dev_join_token: None,
        dist_dir: "/nonexistent".into(),
        releases_repo: "nook-os/nook-os".into(),
        artifact_store: "disk".into(),
        artifact_prefix: "nook".into(),
        artifact_redirect: false,
        s3_bucket: None,
        s3_endpoint: None,
        s3_region: None,
        s3_access_key_id: None,
        s3_secret_access_key: None,
        s3_path_style: true,
        cache_provider: "memory".into(),
        mail_provider: "capture".into(),
        smtp_host: None,
        smtp_port: 587,
        smtp_tls: "starttls".into(),
        smtp_from: "NookOS <no-reply@localhost>".into(),
        smtp_username: None,
        smtp_password: None,
        postmark_token: None,
        postmark_api_url: "https://api.postmarkapp.com/email".into(),
        mail_from: "NookOS <no-reply@localhost>".into(),
        mail_send_enabled: false,
        mail_notifications_enabled: false,
        mail_max_per_month: Some(100),
        mail_max_per_day: None,
    }
}

async fn seed_tenant(pool: &PgPool) -> TenantId {
    let _ = sqlx::query(
        "DELETE FROM tenants WHERE slug LIKE 'authp-%' AND created_at < now() - interval '1 hour'",
    )
    .execute(pool)
    .await;
    let id = TenantId::new();
    sqlx::query("INSERT INTO tenants (id, slug, name) VALUES ($1, $2, $2)")
        .bind(id)
        .bind(format!("authp-{}", Uuid::now_v7().simple()))
        .execute(pool)
        .await
        .expect("seed tenant");
    id
}

/// A member user in `tenant`, linked to `person`, with a live grant.
async fn seed_member(pool: &PgPool, tenant: TenantId, person: Uuid) -> UserId {
    let id = UserId::new();
    sqlx::query(
        "INSERT INTO users (id, tenant_id, display_name, email, role, person_id)
         VALUES ($1, $2, 'P', $3, 'member', $4)",
    )
    .bind(id)
    .bind(tenant)
    .bind(format!("{}@example.test", Uuid::now_v7().simple()))
    .bind(person)
    .execute(pool)
    .await
    .expect("user");
    grant(pool, tenant, id).await;
    id
}

async fn grant(pool: &PgPool, tenant: TenantId, user: UserId) {
    sqlx::query(
        "INSERT INTO tenant_members (id, tenant_id, principal_type, principal_id, role)
         VALUES ($1, $2, 'user', $3, 'member')",
    )
    .bind(Uuid::new_v4())
    .bind(tenant)
    .bind(user)
    .execute(pool)
    .await
    .expect("grant");
}

async fn revoke(pool: &PgPool, tenant: TenantId, user: UserId) {
    sqlx::query("DELETE FROM tenant_members WHERE tenant_id = $1 AND principal_id = $2")
        .bind(tenant)
        .bind(user)
        .execute(pool)
        .await
        .expect("revoke");
}

async fn seed_session(pool: &PgPool, user: UserId, tenant: TenantId) -> Uuid {
    let sid = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO sessions_auth (id, user_id, tenant_id, expires_at)
         VALUES ($1, $2, $3, now() + interval '1 hour')",
    )
    .bind(sid)
    .bind(user)
    .bind(tenant)
    .execute(pool)
    .await
    .expect("session");
    sid
}

async fn cleanup(pool: &PgPool, tenants: &[TenantId]) {
    for t in tenants {
        for tbl in ["events", "sessions_auth", "tenant_members", "users"] {
            let _ = sqlx::query(&format!("DELETE FROM {tbl} WHERE tenant_id = $1"))
                .bind(t)
                .execute(pool)
                .await;
        }
        let _ = sqlx::query("DELETE FROM tenants WHERE id = $1")
            .bind(t)
            .execute(pool)
            .await;
    }
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
    let Some(pool) = test_pool().await else {
        return;
    };
    let state = AppState::new(pool.clone(), test_config(), None).await;
    let t = seed_tenant(&pool).await;
    let me = seed_member(&pool, t, Uuid::new_v4()).await;
    let sid = seed_session(&pool, me, t).await;

    // A live session + grant resolves.
    let ok = extract(&state, sid).await.expect("member resolves");
    assert_eq!(ok.tenant_id, t);
    assert_eq!(ok.user_id, me);
    assert!(ok.cookie_session, "a cookie session is marked as one");

    // Grant revoked, session still valid → 403 (NOT 401): the fold must not
    // collapse a live-session-without-grant into "no session".
    revoke(&pool, t, me).await;
    let forbidden = extract(&state, sid).await.expect_err("revoked → error");
    assert!(
        matches!(forbidden, ApiError::Forbidden),
        "a revoked grant on a live session is 403, got {forbidden:?}"
    );

    // No session row at all → 401.
    grant(&pool, t, me).await; // re-grant, so only the session is missing
    let gone = extract(&state, Uuid::new_v4())
        .await
        .expect_err("unknown session → error");
    assert!(
        matches!(gone, ApiError::Unauthorized),
        "a missing session is 401, got {gone:?}"
    );

    // An expired session → 401 (the `expires_at > now()` guard survived the fold).
    sqlx::query("UPDATE sessions_auth SET expires_at = now() - interval '1 minute' WHERE id = $1")
        .bind(sid)
        .execute(&pool)
        .await
        .unwrap();
    let expired = extract(&state, sid).await.expect_err("expired → error");
    assert!(
        matches!(expired, ApiError::Unauthorized),
        "an expired session is 401, got {expired:?}"
    );

    cleanup(&pool, &[t]).await;
}

/// AC-3: a user token cannot switch (browser-only 400), decided by the explicit
/// marker — not inferred from a zero-row UPDATE.
#[tokio::test]
async fn a_user_token_switch_is_refused_browser_only() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let state = AppState::new(pool.clone(), test_config(), None).await;
    let t = seed_tenant(&pool).await;
    let me = seed_member(&pool, t, Uuid::new_v4()).await;

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

    cleanup(&pool, &[t]).await;
}

/// AC-3: a cookie session that authenticated but whose row vanished mid-request
/// is 401 (session gone), not the token 400.
#[tokio::test]
async fn a_vanished_cookie_session_switch_is_401() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let state = AppState::new(pool.clone(), test_config(), None).await;
    let t = seed_tenant(&pool).await;
    let me = seed_member(&pool, t, Uuid::new_v4()).await;

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

    cleanup(&pool, &[t]).await;
}

/// AC-2: a real crossing is auditable from BOTH tenants' event logs.
#[tokio::test]
async fn a_switch_is_audited_from_both_tenants() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let state = AppState::new(pool.clone(), test_config(), None).await;
    let person = Uuid::new_v4();
    let a = seed_tenant(&pool).await; // source
    let b = seed_tenant(&pool).await; // destination
    let me_a = seed_member(&pool, a, person).await;
    let _me_b = seed_member(&pool, b, person).await; // same person, member of b too
    let sid = seed_session(&pool, me_a, a).await;

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
    let payload = |t: TenantId| {
        let pool = pool.clone();
        async move {
            let (n,): (i64,) = sqlx::query_as(
                "SELECT count(*) FROM events WHERE tenant_id = $1 AND kind = 'user.tenant_switched'",
            )
            .bind(t)
            .fetch_one(&pool)
            .await
            .unwrap();
            let (p,): (serde_json::Value,) = sqlx::query_as(
                "SELECT payload FROM events WHERE tenant_id = $1 AND kind = 'user.tenant_switched'",
            )
            .bind(t)
            .fetch_one(&pool)
            .await
            .unwrap();
            (n, p)
        }
    };
    let (dest_count, dest) = payload(b).await;
    let (source_count, source) = payload(a).await;

    cleanup(&pool, &[a, b]).await;

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
    let Some(pool) = test_pool().await else {
        return;
    };
    let state = AppState::new(pool.clone(), test_config(), None).await;
    let person = Uuid::new_v4();
    let a = seed_tenant(&pool).await;
    let me_a = seed_member(&pool, a, person).await;
    let sid = seed_session(&pool, me_a, a).await;

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

    let directions: Vec<(String,)> = sqlx::query_as(
        "SELECT payload->>'direction' FROM events
         WHERE tenant_id = $1 AND kind = 'user.tenant_switched'",
    )
    .bind(a)
    .fetch_all(&pool)
    .await
    .unwrap();

    cleanup(&pool, &[a]).await;

    // Exactly the arrival, and nothing with direction "out".
    assert_eq!(directions.len(), 1, "only the arrival is recorded");
    assert_eq!(
        directions[0].0, "in",
        "and it is an arrival, not a departure"
    );
}
