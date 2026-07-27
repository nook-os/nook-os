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

/// Set (or clear) a node's `shared` flag, scoped to the one row (MAIN-136).
async fn set_shared_flag(db: &PgPool, node: NodeId, shared: bool) {
    sqlx::query("UPDATE nodes SET shared = $2 WHERE id = $1")
        .bind(node)
        .bind(shared)
        .execute(db)
        .await
        .expect("set shared");
}

/// A node with a chosen (or NULL) owner, so the spawn guard can be exercised
/// directly without a join.
async fn insert_node(db: &PgPool, tenant: TenantId, owner: Option<Uuid>) -> NodeId {
    let id = NodeId::new();
    sqlx::query(
        "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status, owner_person_id)
         VALUES ($1, $2, $3, $4, 'offline', $5)",
    )
    .bind(id)
    .bind(tenant)
    .bind(format!("n-{}", id.0.simple()))
    .bind(format!("h-{}", id.0.simple())) // unique — node_token_hash is UNIQUE
    .bind(owner)
    .execute(db)
    .await
    .expect("node");
    id
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
         VALUES ($1, $2, $3, $4, 'offline')",
    )
    .bind(node)
    .bind(tenant)
    .bind(format!("legacy-{}", node.0.simple()))
    .bind(format!("h-{}", node.0.simple()))
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

// ── MAIN-130: session-start authorization (owner-only, no exceptions) ─────────

/// The shared guard: a session spawns only for the person who owns the node.
/// The owner passes; a member, an admin, and an ownerless node all refuse; and
/// the MCP path (no acting identity) is refused rather than run as the tenant
/// owner.
#[tokio::test]
async fn spawn_guard_is_owner_only_with_no_role_bypass() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let state = AppState::new(pool.clone(), test_config(), None).await;
    let tenant = new_tenant(&pool).await;
    let (owner, owner_person) = add_user(&pool, tenant, "owner").await;
    let (member, _) = add_user(&pool, tenant, "member").await;
    let (admin, _) = add_user(&pool, tenant, "admin").await;

    let node = insert_node(&pool, tenant, Some(owner_person)).await;

    // The owner may spawn.
    assert!(
        nook_control::auth::require_person_owns_node(&state, tenant, Some(owner), node)
            .await
            .is_ok(),
        "the node's owner must be allowed to start a session"
    );
    // A member who does not own it is refused — this is the vuln, closed.
    assert!(
        nook_control::auth::require_person_owns_node(&state, tenant, Some(member), node)
            .await
            .is_err(),
        "a non-owner member reached a teammate's machine"
    );
    // An admin gets NO bypass (NG-3, AC-2).
    assert!(
        nook_control::auth::require_person_owns_node(&state, tenant, Some(admin), node)
            .await
            .is_err(),
        "admin/owner roles must not bypass node ownership"
    );
    // The MCP path carries no acting identity → refused, never the tenant owner.
    assert!(
        nook_control::auth::require_person_owns_node(&state, tenant, None, node)
            .await
            .is_err(),
        "a session with no acting identity (MCP) must be refused (AC-3)"
    );

    // An ownerless node refuses everyone, including the tenant owner.
    let orphan = insert_node(&pool, tenant, None).await;
    assert!(
        nook_control::auth::require_person_owns_node(&state, tenant, Some(owner), orphan)
            .await
            .is_err(),
        "an ownerless node must refuse everyone"
    );

    cleanup(&pool, tenant).await;
}

/// The two direct session-spawn routes refuse a non-owner. The guard runs
/// before any session machinery, so a member is turned away by
/// `POST /sessions` and `POST /nodes/{id}/terminal` alike.
#[tokio::test]
async fn session_routes_refuse_a_non_owner() {
    use axum::extract::Path;
    use axum::Json;

    let Some(pool) = test_pool().await else {
        return;
    };
    let state = AppState::new(pool.clone(), test_config(), None).await;
    let tenant = new_tenant(&pool).await;
    let (_owner, owner_person) = add_user(&pool, tenant, "owner").await;
    let (member, _) = add_user(&pool, tenant, "member").await;
    let node = insert_node(&pool, tenant, Some(owner_person)).await;

    // POST /sessions as the member → refused before create_session.
    let created = nook_control::routes::sessions::create(
        State(state.clone()),
        auth(member, tenant),
        Json(CreateSessionRequest {
            workspace_id: WorkspaceId::new(),
            node_id: node,
            runtime: "bash".into(),
            name: None,
            path: None,
        }),
    )
    .await;
    assert!(
        created.is_err(),
        "a non-owner must not be able to start a session via POST /sessions"
    );

    // POST /nodes/{id}/terminal as the member → refused.
    let opened = nook_control::routes::sessions::open_terminal(
        State(state.clone()),
        auth(member, tenant),
        Path(node),
        None,
    )
    .await;
    assert!(
        opened.is_err(),
        "a non-owner must not be able to open a terminal via /nodes/{{id}}/terminal"
    );

    cleanup(&pool, tenant).await;
}

/// A node credential keeps its machine-only confinement at the spawn routes —
/// it may act on itself, not on a peer (AC-4). This is the same lateral-movement
/// boundary `require_node_self` guards, preserved through the new owner guard.
#[tokio::test]
async fn a_node_credential_still_reaches_only_itself() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let state = AppState::new(pool.clone(), test_config(), None).await;
    let tenant = new_tenant(&pool).await;
    let self_id = NodeId::new();
    let other_id = NodeId::new();
    let node_ctx = AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: UserId(self_id.0),
        tenant_id: tenant,
        principal: Principal::Node(self_id),
        cookie_session: false,
    };
    assert!(
        node_ctx.require_node_owner(&state, self_id).await.is_ok(),
        "a node must still be able to act on itself"
    );
    assert!(
        node_ctx.require_node_owner(&state, other_id).await.is_err(),
        "a node token must not reach another machine"
    );

    cleanup(&pool, tenant).await;
}

// ── MAIN-136: shared nodes are usable (owner-or-shared), management owner-only ──

/// The asymmetry the epic step is about: a member may START a session on a node
/// its owner has SHARED, but may never MANAGE it. Owner-or-shared for use;
/// owner-only for the share toggle. Sharing grants no bypass of the
/// missing-identity rule, so the no-actor MCP path is refused even on a shared
/// node. Unsharing turns new starts away again (existing sessions untouched).
#[tokio::test]
async fn shared_node_relaxes_use_but_not_management() {
    use axum::extract::Path;

    let Some(pool) = test_pool().await else {
        return;
    };
    let state = AppState::new(pool.clone(), test_config(), None).await;
    let tenant = new_tenant(&pool).await;
    let (owner, owner_person) = add_user(&pool, tenant, "owner").await;
    let (member, _) = add_user(&pool, tenant, "member").await;
    let node = insert_node(&pool, tenant, Some(owner_person)).await;

    // Unshared: only the owner may use it. The member is refused, and the
    // message now names shared machines as the exception (AC-1).
    assert!(
        nook_control::auth::require_person_may_use_node(&state, tenant, Some(owner), node)
            .await
            .is_ok(),
        "the owner may always use their own node"
    );
    let refused =
        nook_control::auth::require_person_may_use_node(&state, tenant, Some(member), node)
            .await
            .expect_err("a member cannot use an UNSHARED teammate node");
    assert!(
        format!("{refused:?}").contains("shared"),
        "the refusal names shared machines as the exception, got {refused:?}"
    );

    // Share it → the member may now use it, via the free helper AND the AuthCtx
    // method the session-start routes actually call.
    set_shared_flag(&pool, node, true).await;
    assert!(
        nook_control::auth::require_person_may_use_node(&state, tenant, Some(member), node)
            .await
            .is_ok(),
        "a member may use a SHARED teammate node (AC-2)"
    );
    assert!(
        auth(member, tenant)
            .require_node_may_use(&state, node)
            .await
            .is_ok(),
        "the session-route method also allows a member on a shared node"
    );
    assert!(
        nook_control::auth::require_person_may_use_node(&state, tenant, Some(owner), node)
            .await
            .is_ok(),
        "the owner is unaffected by sharing"
    );
    // Sharing grants NO bypass of the missing-identity rule: MCP (None) refused.
    assert!(
        nook_control::auth::require_person_may_use_node(&state, tenant, None, node)
            .await
            .is_err(),
        "a shared node still refuses the no-actor MCP path (AC-1)"
    );

    // NG-1: the member can USE the shared node but cannot MANAGE it — the share
    // toggle stays owner-only even though the member now sees and uses it.
    let toggled = nook_control::routes::nodes::set_shared(
        State(state.clone()),
        auth(member, tenant),
        Path(node),
        Json(SetSharedRequest { shared: false }),
    )
    .await;
    assert!(
        toggled.is_err(),
        "a non-owner must not toggle sharing, even on a node shared with them (NG-1)"
    );
    let still: bool = sqlx::query_scalar("SELECT shared FROM nodes WHERE id = $1")
        .bind(node)
        .fetch_one(&pool)
        .await
        .expect("shared flag");
    assert!(still, "the refused toggle must not have unshared the node");

    // Unshare → the member's NEW starts are refused again (AC-2).
    set_shared_flag(&pool, node, false).await;
    assert!(
        nook_control::auth::require_person_may_use_node(&state, tenant, Some(member), node)
            .await
            .is_err(),
        "unsharing refuses the member's new session starts again (AC-2)"
    );

    cleanup(&pool, tenant).await;
}

// ── MAIN-126: node-scoped authorize gate ─────────────────────────────────────

/// A PERSONAL node is authorizable only by its owner; a SHARED node only by a
/// node-manager. Unauthorized callers are refused (403) BEFORE anything is
/// launched; an authorized caller passes the gate (and here fails only because
/// the test node is offline — never a 403).
#[tokio::test]
async fn authorize_is_owner_only_for_personal_and_manager_only_for_shared() {
    use axum::extract::Path;
    let Some(pool) = test_pool().await else {
        return;
    };
    let state = AppState::new(pool.clone(), test_config(), None).await;
    let tenant = new_tenant(&pool).await;
    let (owner, owner_person) = add_user(&pool, tenant, "owner").await;
    let (member, _) = add_user(&pool, tenant, "member").await;
    let node = insert_node(&pool, tenant, Some(owner_person)).await;

    let req = || {
        Json(AuthorizeRuntimeRequest {
            runtime: "claude".into(),
        })
    };
    let forbidden = |e: &nook_control::error::ApiError| {
        matches!(
            e,
            nook_control::error::ApiError::Forbidden
                | nook_control::error::ApiError::ForbiddenMsg(_)
        )
    };

    // Personal node: a non-owner member is refused.
    let denied = nook_control::routes::nodes::authorize(
        State(state.clone()),
        auth(member, tenant),
        Path(node),
        req(),
    )
    .await;
    assert!(
        denied.as_ref().err().is_some_and(forbidden),
        "a non-owner must not authorize a personal node: {denied:?}"
    );
    // The owner passes the gate — the only failure is the offline node, not a 403.
    let owner_try = nook_control::routes::nodes::authorize(
        State(state.clone()),
        auth(owner, tenant),
        Path(node),
        req(),
    )
    .await;
    assert!(
        owner_try.as_ref().err().is_some_and(|e| !forbidden(e)),
        "the owner passes the personal-node gate: {owner_try:?}"
    );

    // Shared node: it now takes a node-manager, not the owner-as-person.
    set_shared_flag(&pool, node, true).await;
    let member_shared = nook_control::routes::nodes::authorize(
        State(state.clone()),
        auth(member, tenant),
        Path(node),
        req(),
    )
    .await;
    assert!(
        member_shared.as_ref().err().is_some_and(forbidden),
        "a member without node.manage must not authorize a shared node: {member_shared:?}"
    );
    // Grant node.manage (operator role) → passes the gate.
    sqlx::query(
        "INSERT INTO role_bindings (id, subject_type, subject_id, role_key, scope_type, scope_id)
         VALUES (gen_random_uuid(), 'user', $1, 'operator', 'deployment', NULL)",
    )
    .bind(member.0)
    .execute(&pool)
    .await
    .unwrap();
    let member_admin = nook_control::routes::nodes::authorize(
        State(state.clone()),
        auth(member, tenant),
        Path(node),
        req(),
    )
    .await;
    assert!(
        member_admin.as_ref().err().is_some_and(|e| !forbidden(e)),
        "a node-manager passes the shared-node gate: {member_admin:?}"
    );

    cleanup(&pool, tenant).await;
}
