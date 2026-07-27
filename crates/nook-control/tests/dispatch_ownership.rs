//! MAIN-131: auto-dispatch places work only on the requester's OWN nodes.
//! `schedule::pick` filters candidates to nodes whose `owner_person_id` equals
//! the acting person; an unowned node is never chosen, however well-resourced,
//! and no acting identity (the MCP path) yields the no-eligible-node error
//! rather than a tenant-wide pick. Set `DATABASE_URL`.

use nook_control::config::Config;
use nook_control::state::AppState;
use nook_control::ws::registry::NodeHandle;
use nook_proto::ControlToNode;
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

async fn new_tenant(db: &PgPool) -> TenantId {
    let id = TenantId::new();
    sqlx::query("INSERT INTO tenants (id, name, slug) VALUES ($1, $2, $2)")
        .bind(id)
        .bind(format!("di-{}", id.0.simple()))
        .execute(db)
        .await
        .expect("tenant");
    id
}

/// A user with a known person id. Returns (user, person).
async fn add_user(db: &PgPool, tenant: TenantId) -> (UserId, Uuid) {
    let user = UserId::new();
    let person = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO users (id, tenant_id, person_id, display_name, email, role)
         VALUES ($1, $2, $3, 'U', $4, 'member')",
    )
    .bind(user)
    .bind(tenant)
    .bind(person)
    .bind(format!("u-{}@example.test", user.0.simple()))
    .execute(db)
    .await
    .expect("user");
    (user, person)
}

/// A node owned by `owner` with a chosen resource sample, then registered ONLINE
/// in the registry so `schedule::pick` treats it as a live candidate.
async fn online_node(state: &AppState, tenant: TenantId, owner: Uuid, free_mem_gb: u64) -> NodeId {
    let id = NodeId::new();
    let resources = serde_json::json!({
        "cpu_percent": 0.0,
        "mem_used": 0,
        "mem_total": free_mem_gb,
        "load_avg1": 0.0,
        "active_sessions": 0,
    });
    sqlx::query(
        "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status, owner_person_id, resources)
         VALUES ($1, $2, $3, $4, 'online', $5, $6)",
    )
    .bind(id)
    .bind(tenant)
    .bind(format!("n-{}", id.0.simple()))
    .bind(format!("h-{}", id.0.simple()))
    .bind(owner)
    .bind(resources)
    .execute(&state.db)
    .await
    .expect("node");
    let (tx, _rx) = tokio::sync::mpsc::channel::<ControlToNode>(4);
    state.registry.register_node(
        id,
        NodeHandle {
            tenant_id: tenant,
            tx,
        },
    );
    id
}

async fn cleanup(db: &PgPool, tenant: TenantId) {
    let _ = sqlx::query("DELETE FROM tenants WHERE id = $1 AND slug <> 'dev'")
        .bind(tenant)
        .execute(db)
        .await;
}

#[tokio::test]
async fn pick_chooses_an_owned_node_and_never_a_teammates() {
    let Some(pool) = test_pool().await else {
        eprintln!("skipping dispatch-ownership test — no DATABASE_URL");
        return;
    };
    let state = AppState::new(pool.clone(), test_config(), None).await;
    let tenant = new_tenant(&pool).await;
    let (me, my_person) = add_user(&pool, tenant).await;
    let (_teammate, their_person) = add_user(&pool, tenant).await;

    // My modest node vs the teammate's much bigger, idle one — both online.
    let mine = online_node(&state, tenant, my_person, 8).await;
    let _theirs = online_node(&state, tenant, their_person, 64).await;

    let picked = nook_control::services::schedule::pick(&state, tenant, Some(me), None)
        .await
        .expect("an owned node is online");
    assert_eq!(
        picked, mine,
        "placement must choose my node, never the teammate's better-resourced one"
    );

    cleanup(&pool, tenant).await;
}

#[tokio::test]
async fn no_owned_node_online_is_the_explicit_error() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let state = AppState::new(pool.clone(), test_config(), None).await;
    let tenant = new_tenant(&pool).await;
    let (me, _my_person) = add_user(&pool, tenant).await;
    let (_teammate, their_person) = add_user(&pool, tenant).await;

    // Only the teammate has an online node; I own none.
    let _theirs = online_node(&state, tenant, their_person, 64).await;

    let err = nook_control::services::schedule::pick(&state, tenant, Some(me), None)
        .await
        .expect_err("I own no online node");
    assert!(
        err.to_string().contains("no eligible node of yours"),
        "the caller gets the explicit no-owned-node message, got: {err}"
    );

    cleanup(&pool, tenant).await;
}

#[tokio::test]
async fn no_acting_identity_is_refused_not_widened() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let state = AppState::new(pool.clone(), test_config(), None).await;
    let tenant = new_tenant(&pool).await;
    let (_teammate, their_person) = add_user(&pool, tenant).await;

    // A node exists and is online, but the caller (MCP) has no acting person.
    let _theirs = online_node(&state, tenant, their_person, 64).await;

    let err = nook_control::services::schedule::pick(&state, tenant, None, None)
        .await
        .expect_err("no acting person → no eligible node");
    assert!(
        err.to_string().contains("no eligible node of yours"),
        "a None actor must be refused, never widened to a tenant-wide pick, got: {err}"
    );

    cleanup(&pool, tenant).await;
}
