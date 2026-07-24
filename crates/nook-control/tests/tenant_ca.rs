//! Per-tenant CA behaviour: isolation, fingerprint verification, and the full
//! rotation sequence including the retirement guard.
//!
//! Rotation is the part that silently rots if it is only ever checked by hand,
//! so the whole distribute → switch → drain → retire dance is asserted here.

use nook_control::ca;
use nook_control::crypto::Vault;
use nook_types::TenantId;
use sqlx::PgPool;
use uuid::Uuid;

mod common;
use common::test_pool;

fn vault() -> Vault {
    Vault::from_env("test-session-secret-that-is-long-enough-000000").expect("vault")
}

async fn seed_tenant(pool: &PgPool) -> TenantId {
    // Sweep anything a previous run left behind. `cleanup` below runs at the
    // end of a test and is therefore skipped by any test that panics — which is
    // how 35 `ca-*` tenants accumulated in the dev database. Cleaning on the
    // way IN is the only cleanup a panic cannot skip.
    let _ = sqlx::query(
        "DELETE FROM tenants WHERE slug LIKE 'ca-%' AND created_at < now() - interval '1 hour'",
    )
    .execute(pool)
    .await;

    let id = TenantId::new();
    sqlx::query("INSERT INTO tenants (id, slug, name) VALUES ($1, $2, $2)")
        .bind(id)
        .bind(format!("ca-{}", Uuid::now_v7().simple()))
        .execute(pool)
        .await
        .expect("seed tenant");
    id
}

async fn cleanup(pool: &PgPool, tenants: &[TenantId]) {
    for t in tenants {
        let _ = sqlx::query("DELETE FROM tenants WHERE id = $1")
            .bind(t)
            .execute(pool)
            .await;
    }
}

/// A CA is generated, sealed, and loadable — and the key never comes back in
/// the record, only from the verified load path.
#[tokio::test]
async fn generates_and_loads_a_verified_signer() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let v = vault();
    let tenant = seed_tenant(&pool).await;

    let ca = ca::generate(&pool, &v, tenant, true).await.unwrap();
    assert_eq!(ca.state, "active");
    assert!(ca.cert_pem.contains("BEGIN CERTIFICATE"));

    let (loaded, key_pem) = ca::load_signer(&pool, &v, tenant).await.unwrap();
    assert_eq!(loaded.id, ca.id);
    assert!(key_pem.contains("PRIVATE KEY"), "key must decrypt");
    // The fingerprint is computed from the certificate, not merely stored.
    assert_eq!(
        ca::fingerprint_pem(&loaded.cert_pem).unwrap(),
        ca.fingerprint
    );

    cleanup(&pool, &[tenant]).await;
}

/// Two tenants, two CAs, no overlap. The whole reason the CA is per-tenant.
#[tokio::test]
async fn tenants_do_not_share_a_ca() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let v = vault();
    let (a, b) = (seed_tenant(&pool).await, seed_tenant(&pool).await);

    let ca_a = ca::generate(&pool, &v, a, true).await.unwrap();
    let ca_b = ca::generate(&pool, &v, b, true).await.unwrap();
    assert_ne!(ca_a.fingerprint, ca_b.fingerprint);

    // Each tenant's bundle contains only its own CA.
    let bundle_a = ca::trust_bundle(&pool, a).await.unwrap();
    assert_eq!(bundle_a.len(), 1);
    assert_eq!(bundle_a[0].id, ca_a.id);
    assert!(
        !bundle_a.iter().any(|c| c.id == ca_b.id),
        "tenant A must never see tenant B's CA"
    );

    cleanup(&pool, &[a, b]).await;
}

/// A certificate that no longer matches its recorded fingerprint is tampering
/// or corruption. Signing with it anyway would be the silent failure.
#[tokio::test]
async fn refuses_a_signer_whose_fingerprint_does_not_match() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let v = vault();
    let tenant = seed_tenant(&pool).await;
    ca::generate(&pool, &v, tenant, true).await.unwrap();

    // Swap in a different (validly-formed) certificate behind the fingerprint.
    let other = ca::generate(&pool, &v, seed_tenant(&pool).await, false)
        .await
        .unwrap();
    sqlx::query("UPDATE tenant_cas SET cert_pem = $2 WHERE tenant_id = $1 AND state = 'active'")
        .bind(tenant)
        .bind(&other.cert_pem)
        .execute(&pool)
        .await
        .unwrap();

    let err = ca::load_signer(&pool, &v, tenant).await.unwrap_err();
    assert!(
        err.to_string().contains("fingerprint"),
        "must refuse on fingerprint mismatch, got: {err}"
    );

    cleanup(&pool, &[tenant]).await;
}

/// The full rotation: stage (trusted, not signing) → promote → drain → retire,
/// with the guard refusing while a live leaf still chains to the old CA.
#[tokio::test]
async fn rotation_distributes_then_switches_then_retires() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let v = vault();
    let tenant = seed_tenant(&pool).await;

    let old = ca::generate(&pool, &v, tenant, true).await.unwrap();

    // A node holding a live leaf signed by the old CA.
    let node = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status, ca_id, cert_not_after)
         VALUES ($1, $2, $3, $3, 'online', $4, now() + interval '30 days')",
    )
    .bind(node)
    .bind(tenant)
    .bind(format!("n-{}", Uuid::now_v7().simple()))
    .bind(old.id)
    .execute(&pool)
    .await
    .unwrap();

    // ── distribute ──────────────────────────────────────────────────────
    let new = ca::generate(&pool, &v, tenant, false).await.unwrap();
    assert_eq!(new.state, "staged");
    let bundle = ca::trust_bundle(&pool, tenant).await.unwrap();
    assert_eq!(bundle.len(), 2, "both CAs are trusted during a rotation");
    // ...but the signer is still the old one.
    let (signer, _) = ca::load_signer(&pool, &v, tenant).await.unwrap();
    assert_eq!(signer.id, old.id, "staging must not change who signs");

    // ── switch ──────────────────────────────────────────────────────────
    ca::promote(&pool, tenant, new.id).await.unwrap();
    let (signer, _) = ca::load_signer(&pool, &v, tenant).await.unwrap();
    assert_eq!(signer.id, new.id);
    // The old CA is still trusted — nodes have not renewed yet.
    assert_eq!(ca::trust_bundle(&pool, tenant).await.unwrap().len(), 2);

    // ── the guard ───────────────────────────────────────────────────────
    assert_eq!(ca::live_leaves(&pool, tenant, old.id).await.unwrap(), 1);
    let err = ca::retire(&pool, tenant, old.id).await.unwrap_err();
    assert!(
        err.to_string().contains("still hold unexpired"),
        "must refuse to retire a CA with live leaves, got: {err}"
    );

    // ── drain, then retire ──────────────────────────────────────────────
    // The node renews onto the new CA (what enrolment will do for real).
    sqlx::query("UPDATE nodes SET ca_id = $2 WHERE id = $1")
        .bind(node)
        .bind(new.id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(ca::live_leaves(&pool, tenant, old.id).await.unwrap(), 0);

    ca::retire(&pool, tenant, old.id).await.unwrap();
    let bundle = ca::trust_bundle(&pool, tenant).await.unwrap();
    assert_eq!(bundle.len(), 1);
    assert_eq!(bundle[0].id, new.id);

    cleanup(&pool, &[tenant]).await;
}

/// The active signer is never retirable — that would leave the tenant unable
/// to issue anything.
#[tokio::test]
async fn the_active_signer_cannot_be_retired() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let v = vault();
    let tenant = seed_tenant(&pool).await;
    let ca_row = ca::generate(&pool, &v, tenant, true).await.unwrap();

    let err = ca::retire(&pool, tenant, ca_row.id).await.unwrap_err();
    assert!(err.to_string().contains("no retirable CA"), "got: {err}");

    cleanup(&pool, &[tenant]).await;
}

// ── Authorization ───────────────────────────────────────────────────────────

use nook_control::auth::{AuthCtx, Principal};
use nook_control::state::AppState;
use nook_types::{AuthSessionId, UserId};

async fn seed_user(pool: &PgPool, tenant: TenantId, role: &str) -> UserId {
    let id = UserId::new();
    sqlx::query(
        "INSERT INTO users (id, tenant_id, display_name, email, role)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(tenant)
    .bind(role)
    .bind(format!("{}@example.test", Uuid::now_v7().simple()))
    .bind(role)
    .execute(pool)
    .await
    .expect("user");
    id
}

fn ctx(user: UserId, tenant: TenantId) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId::new(),
        user_id: user,
        tenant_id: tenant,
        principal: Principal::User,
    }
}

/// Owners and admins may run CA operations; a plain member may not.
#[tokio::test]
async fn ca_operations_need_owner_or_admin() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let tenant = seed_tenant(&pool).await;
    let state = AppState::new(pool.clone(), test_config(), None).await;

    for role in ["owner", "admin"] {
        let u = seed_user(&pool, tenant, role).await;
        assert!(
            ctx(u, tenant).require_tenant_admin(&state).await.is_ok(),
            "{role} must be allowed"
        );
    }
    let member = seed_user(&pool, tenant, "member").await;
    assert!(
        ctx(member, tenant)
            .require_tenant_admin(&state)
            .await
            .is_err(),
        "a member must not run CA operations"
    );

    cleanup(&pool, &[tenant]).await;
}

/// An admin of one tenant is not an admin of another. The role lookup is
/// scoped by the authenticated tenant, so a forged context gets nothing.
#[tokio::test]
async fn a_tenant_admin_cannot_reach_another_tenant() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let (a, b) = (seed_tenant(&pool).await, seed_tenant(&pool).await);
    let state = AppState::new(pool.clone(), test_config(), None).await;

    let admin_a = seed_user(&pool, a, "owner").await;
    // Their own tenant: fine.
    assert!(ctx(admin_a, a).require_tenant_admin(&state).await.is_ok());
    // Someone else's: they hold no role there, so the lookup finds nothing.
    assert!(
        ctx(admin_a, b).require_tenant_admin(&state).await.is_err(),
        "tenant A's owner must not be an admin of tenant B"
    );

    cleanup(&pool, &[a, b]).await;
}

/// A machine credential can never run CA operations, whatever the tenant's
/// roles say — that is how a stolen node token stays confined.
#[tokio::test]
async fn a_node_credential_cannot_run_ca_operations() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let tenant = seed_tenant(&pool).await;
    let state = AppState::new(pool.clone(), test_config(), None).await;
    let owner = seed_user(&pool, tenant, "owner").await;

    let as_node = AuthCtx {
        principal: Principal::Node(nook_types::NodeId::new()),
        ..ctx(owner, tenant)
    };
    assert!(
        as_node.require_tenant_admin(&state).await.is_err(),
        "a node credential must never reach CA lifecycle actions"
    );

    cleanup(&pool, &[tenant]).await;
}

/// A config that touches nothing external.
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
        mail_provider: "capture".into(),
        smtp_host: None,
        smtp_port: 587,
        smtp_tls: "starttls".into(),
        smtp_from: "NookOS <no-reply@localhost>".into(),
        smtp_username: None,
        smtp_password: None,
    }
}
