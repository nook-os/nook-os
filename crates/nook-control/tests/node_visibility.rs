//! MAIN-132: `GET /nodes` and `GET /nodes/{id}` scope by role. A member sees
//! only nodes whose `owner_person_id` is theirs (a non-owned id 404s); a tenant
//! owner/admin sees the whole fleet; a node token's view is unchanged; and
//! `/auth/me` carries the caller's role + person_id. Set `DATABASE_URL`.

use axum::extract::{Path, State};
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
        .bind(format!("nv-{}", id.0.simple()))
        .execute(db)
        .await
        .expect("tenant");
    id
}

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

async fn add_node(db: &PgPool, tenant: TenantId, owner: Uuid) -> NodeId {
    let id = NodeId::new();
    sqlx::query(
        "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status, owner_person_id)
         VALUES ($1, $2, $3, $4, 'offline', $5)",
    )
    .bind(id)
    .bind(tenant)
    .bind(format!("n-{}", id.0.simple()))
    .bind(format!("h-{}", id.0.simple()))
    .bind(owner)
    .execute(db)
    .await
    .expect("node");
    id
}

async fn cleanup(db: &PgPool, tenant: TenantId) {
    let _ = sqlx::query("DELETE FROM tenants WHERE id = $1 AND slug <> 'dev'")
        .bind(tenant)
        .execute(db)
        .await;
}

#[tokio::test]
async fn members_see_only_their_own_admins_see_all() {
    let Some(pool) = test_pool().await else {
        eprintln!("skipping node-visibility test — no DATABASE_URL");
        return;
    };
    let state = AppState::new(pool.clone(), test_config(), None).await;
    let tenant = new_tenant(&pool).await;
    let (owner, owner_person) = add_user(&pool, tenant, "owner").await;
    let (member, member_person) = add_user(&pool, tenant, "member").await;

    let my_node = add_node(&pool, tenant, member_person).await;
    let owner_node = add_node(&pool, tenant, owner_person).await;

    // The member sees ONLY their own node.
    let mine = nook_control::routes::nodes::list(State(state.clone()), user_ctx(member, tenant))
        .await
        .expect("list")
        .0;
    let mine_ids: Vec<NodeId> = mine.iter().map(|n| n.id).collect();
    assert!(mine_ids.contains(&my_node), "member sees their own node");
    assert!(
        !mine_ids.contains(&owner_node),
        "member must NOT see a teammate's node"
    );

    // The owner (admin role) sees both.
    let all = nook_control::routes::nodes::list(State(state.clone()), user_ctx(owner, tenant))
        .await
        .expect("list")
        .0;
    let all_ids: Vec<NodeId> = all.iter().map(|n| n.id).collect();
    assert!(
        all_ids.contains(&my_node) && all_ids.contains(&owner_node),
        "an owner/admin sees the whole fleet"
    );

    cleanup(&pool, tenant).await;
}

#[tokio::test]
async fn get_one_404s_for_a_member_on_a_non_owned_node() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let state = AppState::new(pool.clone(), test_config(), None).await;
    let tenant = new_tenant(&pool).await;
    let (owner, owner_person) = add_user(&pool, tenant, "owner").await;
    let (member, member_person) = add_user(&pool, tenant, "member").await;

    let my_node = add_node(&pool, tenant, member_person).await;
    let owner_node = add_node(&pool, tenant, owner_person).await;

    // A member reading their own node: ok. Reading a teammate's: 404.
    assert!(
        nook_control::routes::nodes::get_one(
            State(state.clone()),
            user_ctx(member, tenant),
            Path(my_node)
        )
        .await
        .is_ok(),
        "member may read their own node"
    );
    assert!(
        nook_control::routes::nodes::get_one(
            State(state.clone()),
            user_ctx(member, tenant),
            Path(owner_node)
        )
        .await
        .is_err(),
        "a non-owned node 404s for a member"
    );
    // The owner may read the member's node.
    assert!(
        nook_control::routes::nodes::get_one(
            State(state.clone()),
            user_ctx(owner, tenant),
            Path(my_node)
        )
        .await
        .is_ok(),
        "an owner/admin may read any node in the tenant"
    );

    cleanup(&pool, tenant).await;
}

#[tokio::test]
async fn a_node_token_sees_the_whole_fleet_unchanged() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let state = AppState::new(pool.clone(), test_config(), None).await;
    let tenant = new_tenant(&pool).await;
    let (_owner, owner_person) = add_user(&pool, tenant, "owner").await;
    let (_member, member_person) = add_user(&pool, tenant, "member").await;

    let a = add_node(&pool, tenant, owner_person).await;
    let b = add_node(&pool, tenant, member_person).await;

    // A node credential's listing is unchanged (whole tenant), per AC-1.
    let node_ctx = AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: UserId(Uuid::nil()),
        tenant_id: tenant,
        principal: Principal::Node(a),
        cookie_session: false,
    };
    let listed = nook_control::routes::nodes::list(State(state.clone()), node_ctx)
        .await
        .expect("list")
        .0;
    let ids: Vec<NodeId> = listed.iter().map(|n| n.id).collect();
    assert!(
        ids.contains(&a) && ids.contains(&b),
        "a node token still sees every node in its tenant"
    );

    cleanup(&pool, tenant).await;
}

#[tokio::test]
async fn me_carries_role_and_person_id() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let state = AppState::new(pool.clone(), test_config(), None).await;
    let tenant = new_tenant(&pool).await;
    let (member, member_person) = add_user(&pool, tenant, "member").await;

    let me = nook_control::routes::auth::me(State(state.clone()), user_ctx(member, tenant))
        .await
        .expect("me")
        .0;
    assert_eq!(
        me.person_id, member_person,
        "me carries the caller's person id"
    );
    assert_eq!(
        me.user.role, "member",
        "me carries the caller's tenant role"
    );

    cleanup(&pool, tenant).await;
}
