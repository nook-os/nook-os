//! MAIN-133: `GET /sessions` scopes its metadata by role. A member sees only
//! sessions they created (never `created_by NULL` rows); a tenant owner/admin
//! sees every session, including NULL-creator ones, for capacity/audit. Content
//! access (`session_guard`) is untouched. Set `DATABASE_URL`.

use axum::extract::{Query, State};
use nook_control::auth::{AuthCtx, Principal};
use nook_control::config::Config;
use nook_control::routes::sessions::SessionsQuery;
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
        queue_provider: "database".into(),
        redis_url: None,
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

fn user_ctx(user: UserId, tenant: TenantId) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: user,
        tenant_id: tenant,
        principal: Principal::User,
        cookie_session: false,
    }
}

async fn new_tenant(db: &PgPool) -> TenantId {
    let id = TenantId::new();
    sqlx::query("INSERT INTO tenants (id, name, slug) VALUES ($1, $2, $2)")
        .bind(id)
        .bind(format!("sv-{}", id.0.simple()))
        .execute(db)
        .await
        .expect("tenant");
    id
}

async fn add_user(db: &PgPool, tenant: TenantId, role: &str) -> UserId {
    let user = UserId::new();
    sqlx::query(
        "INSERT INTO users (id, tenant_id, person_id, display_name, email, role)
         VALUES ($1, $2, $3, 'U', $4, $5)",
    )
    .bind(user)
    .bind(tenant)
    .bind(Uuid::now_v7())
    .bind(format!("u-{}@example.test", user.0.simple()))
    .bind(role)
    .execute(db)
    .await
    .expect("user");
    user
}

async fn add_node(db: &PgPool, tenant: TenantId) -> NodeId {
    let id = NodeId::new();
    sqlx::query(
        "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status)
         VALUES ($1, $2, $3, $4, 'offline')",
    )
    .bind(id)
    .bind(tenant)
    .bind(format!("n-{}", id.0.simple()))
    .bind(format!("h-{}", id.0.simple()))
    .execute(db)
    .await
    .expect("node");
    id
}

/// A session on `node` created by `creator` (None = a legacy/MCP row).
async fn add_session(
    db: &PgPool,
    tenant: TenantId,
    node: NodeId,
    creator: Option<UserId>,
) -> SessionId {
    let id = SessionId::new();
    sqlx::query(
        "INSERT INTO sessions (id, tenant_id, node_id, runtime, status, created_by)
         VALUES ($1, $2, $3, 'bash', 'running', $4)",
    )
    .bind(id)
    .bind(tenant)
    .bind(node)
    .bind(creator)
    .execute(db)
    .await
    .expect("session");
    id
}

async fn cleanup(db: &PgPool, tenant: TenantId) {
    let _ = sqlx::query("DELETE FROM tenants WHERE id = $1 AND slug <> 'dev'")
        .bind(tenant)
        .execute(db)
        .await;
}

fn all(active: Option<bool>) -> Query<SessionsQuery> {
    Query(SessionsQuery {
        workspace_id: None,
        active,
    })
}

#[tokio::test]
async fn members_see_only_their_own_admins_see_all_including_null_creators() {
    let Some(pool) = test_pool().await else {
        eprintln!("skipping session-visibility test — no DATABASE_URL");
        return;
    };
    let state = AppState::new(pool.clone(), test_config(), None).await;
    let tenant = new_tenant(&pool).await;
    let owner = add_user(&pool, tenant, "owner").await;
    let member = add_user(&pool, tenant, "member").await;
    let node = add_node(&pool, tenant).await;

    let my_session = add_session(&pool, tenant, node, Some(member)).await;
    let owner_session = add_session(&pool, tenant, node, Some(owner)).await;
    let legacy_session = add_session(&pool, tenant, node, None).await; // created_by NULL

    // The member sees ONLY their own — not the owner's, not the NULL-creator row.
    let mine = nook_control::routes::sessions::list(
        State(state.clone()),
        user_ctx(member, tenant),
        all(None),
    )
    .await
    .expect("list")
    .0;
    let mine_ids: Vec<SessionId> = mine.iter().map(|s| s.id).collect();
    assert!(
        mine_ids.contains(&my_session),
        "member sees their own session"
    );
    assert!(
        !mine_ids.contains(&owner_session),
        "member must NOT see a teammate's session"
    );
    assert!(
        !mine_ids.contains(&legacy_session),
        "member must NOT see a created_by NULL session"
    );

    // The owner (admin role) sees all three, including the NULL-creator row.
    let all_seen = nook_control::routes::sessions::list(
        State(state.clone()),
        user_ctx(owner, tenant),
        all(None),
    )
    .await
    .expect("list")
    .0;
    let all_ids: Vec<SessionId> = all_seen.iter().map(|s| s.id).collect();
    assert!(
        all_ids.contains(&my_session)
            && all_ids.contains(&owner_session)
            && all_ids.contains(&legacy_session),
        "an owner/admin sees every session incl. NULL-creator rows"
    );

    cleanup(&pool, tenant).await;
}

#[tokio::test]
async fn a_node_credential_sees_all_sessions_unchanged() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let state = AppState::new(pool.clone(), test_config(), None).await;
    let tenant = new_tenant(&pool).await;
    let member = add_user(&pool, tenant, "member").await;
    let node = add_node(&pool, tenant).await;
    let s1 = add_session(&pool, tenant, node, Some(member)).await;
    let s2 = add_session(&pool, tenant, node, None).await;

    // A node token's listing is unchanged (whole tenant).
    let node_ctx = AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: UserId(Uuid::nil()),
        tenant_id: tenant,
        principal: Principal::Node(node),
        cookie_session: false,
    };
    let listed = nook_control::routes::sessions::list(State(state.clone()), node_ctx, all(None))
        .await
        .expect("list")
        .0;
    let ids: Vec<SessionId> = listed.iter().map(|s| s.id).collect();
    assert!(
        ids.contains(&s1) && ids.contains(&s2),
        "a node token still sees every session in its tenant"
    );

    cleanup(&pool, tenant).await;
}
