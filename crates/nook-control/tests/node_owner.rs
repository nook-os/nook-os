//! MAIN-119: a node records its owner (person) — the join-token minter, else
//! the tenant owner — set at join/enroll and backfilled for existing rows.
//! Nothing is enforced yet; this only proves the value is stored and returned.
//! Set `DATABASE_URL`.

use axum::extract::State;
use axum::Json;
use nook_control::auth::{AuthCtx, Principal};
use nook_control::config::Config;
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

fn auth(user: UserId, tenant: TenantId) -> AuthCtx {
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
        .bind(format!("no-{}", id.0.simple()))
        .execute(db)
        .await
        .expect("tenant");
    id
}

/// A user with a KNOWN person id (so ownership can be asserted). Returns
/// (user id, person id).
async fn add_user(db: &PgPool, tenant: TenantId, role: &str) -> (UserId, Uuid) {
    let user = UserId::new();
    let person = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO users (id, tenant_id, person_id, display_name, email, role)
         VALUES ($1, $2, $3, 'U', $4, $5)",
    )
    .bind(user)
    .bind(tenant)
    .bind(person)
    .bind(format!("u-{}@example.test", user.0.simple()))
    .bind(role)
    .execute(db)
    .await
    .expect("user");
    (user, person)
}

/// Mint a join token as `minter`, returning its plaintext (created_by = minter).
async fn mint_token(state: &AppState, minter: UserId, tenant: TenantId) -> String {
    nook_control::routes::join::create_join_token(State(state.clone()), auth(minter, tenant))
        .await
        .expect("mint token")
        .0
        .token
}

async fn owner_of(db: &PgPool, node: NodeId) -> Option<Uuid> {
    sqlx::query_scalar("SELECT owner_person_id FROM nodes WHERE id = $1")
        .bind(node)
        .fetch_one(db)
        .await
        .expect("node owner")
}

async fn cleanup(db: &PgPool, tenant: TenantId) {
    let _ = sqlx::query("DELETE FROM tenants WHERE id = $1 AND slug <> 'dev'")
        .bind(tenant)
        .execute(db)
        .await;
}

#[tokio::test]
async fn join_sets_owner_to_the_minter_or_tenant_owner() {
    let Some(pool) = test_pool().await else {
        eprintln!("skipping node-owner test — no DATABASE_URL");
        return;
    };
    let state = AppState::new(pool.clone(), test_config(), None).await;
    let tenant = new_tenant(&pool).await;
    let (owner, owner_person) = add_user(&pool, tenant, "owner").await;
    let (member, member_person) = add_user(&pool, tenant, "member").await;

    // A token minted by the MEMBER → the node is owned by the member's person
    // (not the tenant owner), proving the minter is used (AC-2).
    let token = mint_token(&state, member, tenant).await;
    let joined = nook_control::routes::join::join(
        State(state.clone()),
        Json(JoinRequest {
            token,
            name: format!("m-node-{}", Uuid::now_v7().simple()),
            hostname: "host-a".into(),
            platform: "linux".into(),
        }),
    )
    .await
    .expect("join")
    .0;
    assert_eq!(
        owner_of(&pool, joined.node_id).await,
        Some(member_person),
        "the token minter's person owns the node"
    );

    // A LEGACY token (no recorded minter) falls back to the tenant owner (AC-2).
    let token = mint_token(&state, member, tenant).await;
    // Simulate a legacy token that recorded no minter. This tenant's only
    // still-unused token is the one just minted.
    sqlx::query(
        "UPDATE join_tokens SET created_by = NULL WHERE tenant_id = $1 AND used_at IS NULL",
    )
    .bind(tenant)
    .execute(&pool)
    .await
    .expect("null created_by");
    let joined = nook_control::routes::join::join(
        State(state.clone()),
        Json(JoinRequest {
            token,
            name: format!("legacy-node-{}", Uuid::now_v7().simple()),
            hostname: "host-b".into(),
            platform: "linux".into(),
        }),
    )
    .await
    .expect("legacy join")
    .0;
    assert_eq!(
        owner_of(&pool, joined.node_id).await,
        Some(owner_person),
        "a minterless token falls back to the tenant owner's person"
    );
    let _ = owner; // (the owner user exists so the fallback has a person to find)

    cleanup(&pool, tenant).await;
}

#[tokio::test]
async fn enroll_sets_owner_to_the_minter() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let state = AppState::new(pool.clone(), test_config(), None).await;
    let tenant = new_tenant(&pool).await;
    let (_owner, _) = add_user(&pool, tenant, "owner").await;
    let (member, member_person) = add_user(&pool, tenant, "member").await;

    // A CSR, exactly as the agent produces one.
    let key = rcgen::KeyPair::generate().unwrap();
    let params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    let csr_pem = params.serialize_request(&key).unwrap().pem().unwrap();

    let token = mint_token(&state, member, tenant).await;
    // enroll mints the tenant CA on first use and returns the signed cert; we
    // only care that the node it creates is owned by the minter.
    let _ = nook_control::routes::join::enroll(
        State(state.clone()),
        Json(EnrollRequest {
            token,
            csr_pem,
            name: Some(format!("enrolled-{}", Uuid::now_v7().simple())),
        }),
    )
    .await
    .expect("enroll");

    let owner: Option<Uuid> = sqlx::query_scalar(
        "SELECT owner_person_id FROM nodes WHERE tenant_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(tenant)
    .fetch_one(&pool)
    .await
    .expect("enrolled node");
    assert_eq!(
        owner,
        Some(member_person),
        "enroll records the minter as owner"
    );

    cleanup(&pool, tenant).await;
}

#[tokio::test]
async fn backfill_fills_ownerless_nodes_with_the_tenant_owner() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let tenant = new_tenant(&pool).await;
    let (_owner, owner_person) = add_user(&pool, tenant, "owner").await;

    // An owner-less node, as an existing row would be before the migration.
    let node = NodeId::new();
    sqlx::query(
        "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status)
         VALUES ($1, $2, $3, 'x', 'offline')",
    )
    .bind(node)
    .bind(tenant)
    .bind(format!("legacy-{}", node.0.simple()))
    .execute(&pool)
    .await
    .expect("ownerless node");
    assert_eq!(owner_of(&pool, node).await, None, "starts owner-less");

    // The 0016 backfill statement (scoped to this node so the shared DB's other
    // rows are untouched): resolves the tenant owner's person.
    sqlx::query(
        "UPDATE nodes n SET owner_person_id = (
             SELECT u.person_id FROM users u
             WHERE u.tenant_id = n.tenant_id AND u.role = 'owner'
             ORDER BY u.created_at LIMIT 1)
         WHERE n.owner_person_id IS NULL AND n.id = $1",
    )
    .bind(node)
    .execute(&pool)
    .await
    .expect("backfill");

    assert_eq!(
        owner_of(&pool, node).await,
        Some(owner_person),
        "backfill resolves the tenant owner's person"
    );

    cleanup(&pool, tenant).await;
}
