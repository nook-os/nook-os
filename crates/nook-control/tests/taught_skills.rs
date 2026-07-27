//! Taught-skills fleet-mutation gate (MAIN-106 AC-4) against a live Postgres.
//! Set `DATABASE_URL`.
//!
//! teach (`POST /skills`) and unteach (`DELETE /skills/{name}`) write to every
//! machine in the fleet, so they take `node.manage` — reads stay open to any
//! signed-in user. Scope every assertion to rows this test creates.

use axum::extract::{Path, State};
use axum::Json;
use nook_control::auth::{AuthCtx, Principal};
use nook_control::config::Config;
use nook_control::routes::skills;
use nook_control::state::AppState;
use nook_types::*;
use sqlx::PgPool;
use uuid::Uuid;

mod common;
use common::test_pool;

fn test_config() -> Config {
    Config {
        app_env: "test".into(),
        bind: "127.0.0.1:0".into(),
        shutdown_grace_secs: 25,
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
        default_tenant_name: format!("test-{}", Uuid::now_v7().simple()),
        auth_dev_mode: true,
        mcp_token: None,
        dev_join_token: None,
        dist_dir: "/nonexistent".into(),
        agent_bind: "127.0.0.1:0".into(),
        agent_public_url: None,
        agent_tls_cert: None,
        agent_tls_key: None,
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
        trusted_proxies: Vec::new(),
    }
}

/// A fresh tenant and a plain signed-in user (no role grant).
async fn tenant_user(pool: &PgPool) -> (TenantId, UserId, AuthCtx) {
    let tenant = TenantId(Uuid::now_v7());
    sqlx::query("INSERT INTO tenants (id, name, slug) VALUES ($1, $2, $2)")
        .bind(tenant)
        .bind(format!("t-{}", tenant.0.simple()))
        .execute(pool)
        .await
        .unwrap();
    let user = UserId::new();
    sqlx::query(
        "INSERT INTO users (id, tenant_id, person_id, display_name, email)
         VALUES ($1, $2, gen_random_uuid(), 'Op', $3)",
    )
    .bind(user)
    .bind(tenant)
    .bind(format!("op-{}@example.test", user.0.simple()))
    .execute(pool)
    .await
    .unwrap();
    let ctx = AuthCtx {
        session_id: AuthSessionId::new(),
        user_id: user,
        tenant_id: tenant,
        principal: Principal::User,
        cookie_session: false,
    };
    (tenant, user, ctx)
}

/// Grant the caller `operator` at deployment scope — the seed role that carries
/// `node.manage`, exactly as `read_endpoints_require_node_manage` does.
async fn grant_operator(pool: &PgPool, user: UserId) {
    sqlx::query(
        "INSERT INTO role_bindings (id, subject_type, subject_id, role_key, scope_type, scope_id)
         VALUES (gen_random_uuid(), 'user', $1, 'operator', 'deployment', NULL)",
    )
    .bind(user.0)
    .execute(pool)
    .await
    .unwrap();
}

const SKILL: &str = "---\nname: main106-probe\ndescription: a test skill\n---\n\n# Body\n";

#[tokio::test]
async fn teach_and_unteach_require_node_manage_reads_stay_open() {
    let Some(pool) = test_pool().await else {
        eprintln!("skipping taught-skills gate test — no DATABASE_URL");
        return;
    };
    let state = AppState::new(pool.clone(), test_config(), None).await;
    let (tenant, user, ctx) = tenant_user(&pool).await;

    // Reads are open to any signed-in user, grant or not (AC-4).
    assert!(
        skills::list(State(state.clone()), ctx).await.is_ok(),
        "listing skills must be open to any user"
    );

    // Without node.manage: teaching is refused (a 403, not a 500) and nothing
    // is written — scoped to this tenant's rows.
    let req = || {
        Json(TeachRequest {
            name: None,
            content: SKILL.to_string(),
        })
    };
    assert!(
        skills::teach(State(state.clone()), ctx, req())
            .await
            .is_err(),
        "a plain user must not be able to teach the fleet"
    );
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM skills WHERE tenant_id = $1 AND name = $2")
            .bind(tenant)
            .bind("main106-probe")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 0, "the refused teach must not have written a row");

    // With the grant: teaching succeeds and the row exists.
    grant_operator(&pool, user).await;
    let taught = skills::teach(State(state.clone()), ctx, req()).await;
    assert!(taught.is_ok(), "an operator may teach: {taught:?}");
    assert_eq!(taught.unwrap().0.skill.name, "main106-probe");

    // Reading the specific skill is still open (any user).
    assert!(
        skills::get_one(State(state.clone()), ctx, Path("main106-probe".into()))
            .await
            .is_ok(),
        "reading a skill must be open to any user"
    );

    // Unteach is gated the same way. A second, ungranted user in this tenant is
    // refused; the granted one succeeds.
    let (_t2, _u2, ctx2) = {
        // A distinct user in the SAME tenant, without a grant.
        let u = UserId::new();
        sqlx::query(
            "INSERT INTO users (id, tenant_id, person_id, display_name, email)
             VALUES ($1, $2, gen_random_uuid(), 'M', $3)",
        )
        .bind(u)
        .bind(tenant)
        .bind(format!("m-{}@example.test", u.0.simple()))
        .execute(&pool)
        .await
        .unwrap();
        (
            tenant,
            u,
            AuthCtx {
                session_id: AuthSessionId::new(),
                user_id: u,
                tenant_id: tenant,
                principal: Principal::User,
                cookie_session: false,
            },
        )
    };
    assert!(
        skills::unteach(State(state.clone()), ctx2, Path("main106-probe".into()))
            .await
            .is_err(),
        "a non-manager must not be able to unteach"
    );
    assert!(
        skills::unteach(State(state.clone()), ctx, Path("main106-probe".into()))
            .await
            .is_ok(),
        "an operator may unteach"
    );

    // Cleanup: the tenant (and its rows) this test created.
    let _ = sqlx::query("DELETE FROM tenants WHERE id = $1 AND slug <> 'dev'")
        .bind(tenant)
        .execute(&pool)
        .await;
}
