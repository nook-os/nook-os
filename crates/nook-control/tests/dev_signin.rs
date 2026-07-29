//! MAIN-221: the dev sign-in hatch on a mode-locked instance.
//!
//! The load-bearing pair is here and identical but for the issuer: on a tenant
//! locked to `local`, a **dev** sign-in must succeed (the hatch bypasses the
//! auth-mode lock — AC-1) while a **real** issuer must still be refused with the
//! word-for-word mode-lock message (AC-2, NG-1). The bypass has to be exactly as
//! narrow as the dev gate, so the two cases share every line of setup.
//!
//! Also: the dev-only `test-%` purge deletes only the legacy marker and is
//! idempotent + gated (AC-3), and the account picker caps + searches (AC-4).
//!
//! Every row is test-created and scoped to its own uniquely-named DB via
//! `nook_testkit::TestBed` (NG-4 — no test-suite changes).

use axum::extract::{Query, State};
use axum::Json;
use axum_extra::extract::cookie::CookieJar;
use nook_control::error::ApiError;
use nook_control::routes::auth::{
    dev_accounts, dev_login, purge_test_tenants, DevAccountsQuery, DevLoginRequest,
};
use nook_control::services::identity::{login_identity, IdentityClaims, DEV_ISSUER};
use nook_control::services::local_auth::{self, AuthMode};
use nook_control::{AppState, Config};
use nook_testkit::TestBed;
use nook_types::{TenantId, UserId};
use sqlx::PgPool;
use uuid::Uuid;

/// A tenant already mode-locked to `local` — the state the dev hatch must work
/// against and a real IdP must not.
async fn locked_local_tenant(bed: &TestBed) -> TenantId {
    let id = TenantId::new();
    sqlx::query("INSERT INTO tenants (id, slug, name) VALUES ($1, $2, $2)")
        .bind(id)
        .bind(format!("locked-{}", Uuid::now_v7().simple()))
        .execute(&bed.pool)
        .await
        .expect("tenant");
    local_auth::claim_mode(&bed.db(), id, AuthMode::Local)
        .await
        .expect("lock to local");
    id
}

async fn seed_user(pool: &PgPool, tenant: TenantId, email: &str) -> UserId {
    let id = UserId::new();
    sqlx::query(
        "INSERT INTO users (id, tenant_id, person_id, display_name, email)
         VALUES ($1, $2, gen_random_uuid(), 'Test', $3)",
    )
    .bind(id)
    .bind(tenant)
    .bind(email)
    .execute(pool)
    .await
    .expect("user");
    id
}

async fn seed_identity(pool: &PgPool, user: UserId, issuer: &str, subject: &str) {
    sqlx::query("INSERT INTO identities (id, user_id, issuer, subject) VALUES ($1, $2, $3, $4)")
        .bind(Uuid::now_v7())
        .bind(user)
        .bind(issuer)
        .bind(subject)
        .execute(pool)
        .await
        .expect("identity");
}

fn claims(issuer: &str, subject: &str, email: &str) -> IdentityClaims {
    IdentityClaims {
        issuer: issuer.into(),
        subject: subject.into(),
        email: Some(email.into()),
        // Neither the dev hatch nor this test asserts a verified address.
        email_verified: false,
        display_name: Some("Test".into()),
        avatar_url: None,
        raw_claims: serde_json::json!({}),
    }
}

#[tokio::test]
async fn dev_issuer_signs_in_on_a_local_locked_tenant() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let t = locked_local_tenant(&bed).await;
    let u = seed_user(&bed.pool, t, "dev-me@example.test").await;
    seed_identity(&bed.pool, u, DEV_ISSUER, "dev-me@example.test").await;
    let state = bed.app_state().await;

    let (user, tenant) = login_identity(
        &state,
        claims(DEV_ISSUER, "dev-me@example.test", "dev-me@example.test"),
    )
    .await
    .expect("the dev hatch must sign in even on a local-locked tenant");
    assert_eq!(user.id, u, "signs in as the existing user, same id");
    assert_eq!(tenant.id, t);
    // The dev issuer never claims the mode — the lock is left exactly as it was.
    assert_eq!(
        local_auth::mode_of(&bed.db(), t).await.unwrap(),
        Some(AuthMode::Local),
        "a dev sign-in must not relock or alter the tenant's mode"
    );

    bed.teardown().await;
}

/// The bug the operator hit in the browser: clicking a listed account POSTed
/// dev-login (200, cookie set) but the very next /auth/me came back 403, because
/// `resolve_session` refuses a session whose user has no `tenant_members` grant
/// (a memberless session). Legacy accounts have none, so the dev hatch must
/// ensure one — otherwise "click a name" never actually signs you in (AC-1).
#[tokio::test]
async fn dev_login_lands_a_usable_session_for_a_memberless_account() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let t = locked_local_tenant(&bed).await;
    let email = format!("orphan-{}@example.test", Uuid::now_v7().simple());
    let u = seed_user(&bed.pool, t, &email).await;

    let member_count = |bed: &TestBed| {
        let pool = bed.pool.clone();
        async move {
            let (n,): (i64,) = sqlx::query_as(
                "SELECT count(*) FROM tenant_members
                 WHERE tenant_id = $1 AND principal_type = 'user' AND principal_id = $2",
            )
            .bind(t)
            .bind(u)
            .fetch_one(&pool)
            .await
            .unwrap();
            n
        }
    };
    assert_eq!(
        member_count(&bed).await,
        0,
        "the account starts memberless — exactly the state whose session 403'd"
    );

    let state = bed.app_state().await;
    dev_login(
        State(state),
        CookieJar::new(),
        Json(DevLoginRequest {
            email: Some(email),
            display_name: None,
        }),
    )
    .await
    .expect("dev sign-in as an existing account must succeed");

    // With a grant now present, resolve_session accepts the session — the app is
    // actually reachable signed in, not bounced by /auth/me.
    assert_eq!(
        member_count(&bed).await,
        1,
        "the dev hatch ensures a membership so the created session resolves"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn a_real_issuer_is_still_refused_on_a_local_locked_tenant() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    // Identical setup to the dev case — only the issuer differs.
    let t = locked_local_tenant(&bed).await;
    let u = seed_user(&bed.pool, t, "real-me@example.test").await;
    seed_identity(&bed.pool, u, "https://idp.example.test", "real-subject").await;
    let state = bed.app_state().await;

    let err = login_identity(
        &state,
        claims(
            "https://idp.example.test",
            "real-subject",
            "real-me@example.test",
        ),
    )
    .await
    .expect_err("a genuine OIDC sign-in must still hit the mode lock");
    match err {
        ApiError::ForbiddenMsg(m) => assert!(
            m.contains("signs in with local"),
            "must be the word-for-word mode-lock refusal, got: {m}"
        ),
        other => panic!("expected the mode-lock ForbiddenMsg, got {other:?}"),
    }

    bed.teardown().await;
}

#[tokio::test]
async fn purge_removes_only_test_tenants_and_is_idempotent() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let mk = |name: &str, slug: &str| {
        let (name, slug) = (name.to_string(), slug.to_string());
        let pool = bed.pool.clone();
        async move {
            sqlx::query("INSERT INTO tenants (id, slug, name) VALUES ($1, $2, $3)")
                .bind(TenantId::new())
                .bind(slug)
                .bind(name)
                .execute(&pool)
                .await
                .expect("tenant");
        }
    };
    // Two legacy markers (one by name, one by slug) and one real tenant.
    mk("test-alpha", "test-alpha").await;
    mk("Keeper", "test-by-slug").await;
    let real_slug = format!("real-{}", Uuid::now_v7().simple());
    mk("Keeper Two", &real_slug).await;

    let state = bed.app_state().await;
    let deleted = purge_test_tenants(State(state.clone()))
        .await
        .expect("purge")
        .0
        .deleted;
    assert_eq!(deleted, 2, "only the two test-% tenants are removed");

    let (survivors,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM tenants WHERE slug LIKE 'test-%' OR name LIKE 'test-%'",
    )
    .fetch_one(&bed.pool)
    .await
    .unwrap();
    assert_eq!(survivors, 0, "no test-% tenant remains");
    let (real_alive,): (i64,) = sqlx::query_as("SELECT count(*) FROM tenants WHERE slug = $1")
        .bind(&real_slug)
        .fetch_one(&bed.pool)
        .await
        .unwrap();
    assert_eq!(real_alive, 1, "a real tenant is never touched");

    // Idempotent: a second run finds nothing.
    let again = purge_test_tenants(State(state))
        .await
        .expect("purge again")
        .0
        .deleted;
    assert_eq!(again, 0);

    bed.teardown().await;
}

#[tokio::test]
async fn purge_is_refused_when_dev_mode_is_off() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    // Same gate as the rest of the dev hatch: dev mode off ⇒ refused, no delete.
    let mut cfg: Config = bed.config();
    cfg.auth_dev_mode = false;
    let state = AppState::new(bed.db(), cfg, None).await;

    let err = purge_test_tenants(State(state))
        .await
        .expect_err("dev mode off must refuse the purge");
    assert!(matches!(err, ApiError::Forbidden), "expected Forbidden");

    bed.teardown().await;
}

#[tokio::test]
async fn dev_accounts_caps_the_page_and_searches() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let t = locked_local_tenant(&bed).await;
    // A unique marker scopes the assertions to just these rows, regardless of
    // whatever the bootstrap seed created.
    let marker = format!("cap{}", Uuid::now_v7().simple());
    for i in 0..55 {
        seed_user(&bed.pool, t, &format!("{marker}-{i:02}@example.test")).await;
    }
    let state = bed.app_state().await;

    let page = dev_accounts(
        State(state.clone()),
        Query(DevAccountsQuery {
            q: Some(marker.clone()),
        }),
    )
    .await
    .expect("dev_accounts")
    .0;
    assert_eq!(page.total, 55, "total reflects every match, uncapped");
    assert_eq!(page.accounts.len(), 50, "the page itself is capped at 50");

    // A narrower search reaches exactly one account (email substring match).
    let one = format!("{marker}-07@example.test");
    let hit = dev_accounts(
        State(state),
        Query(DevAccountsQuery {
            q: Some(one.clone()),
        }),
    )
    .await
    .expect("search")
    .0;
    assert_eq!(hit.total, 1);
    assert_eq!(hit.accounts.len(), 1);
    assert_eq!(hit.accounts[0].email, one);

    bed.teardown().await;
}
