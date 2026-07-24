//! Tenant provisioning: a new person gets their own tenant, and cannot see
//! anyone else's.
//!
//! This is the rule that, when it breaks, breaks quietly — everything still
//! works, there is just one more machine in your list than there should be.
//! So it is tested against a real database through the real login path rather
//! than by reading the code.
//!
//! Needs a running Postgres (the dev stack's works): set `DATABASE_URL`. The
//! tests skip cleanly when it's absent so `cargo test` stays green anywhere.

use nook_control::config::Config;
use nook_control::services::identity::{email_is_verified, login_identity, IdentityClaims};
use nook_control::state::AppState;
use nook_types::TenantId;
use sqlx::PgPool;

mod common;
use common::test_pool;
use tokio::sync::Mutex;
use uuid::Uuid;

/// These tests share one database and one piece of global state — "the oldest
/// tenant on the instance" — so they run one at a time. Cargo runs tests in a
/// thread pool by default, and two of these racing produces a failure that
/// looks like a bug in provisioning rather than in the test.
static SERIAL: Mutex<()> = Mutex::const_new(());

/// A config that does not touch the environment beyond what Config requires.
///
/// The default tenant name is unique per call. Hard-coding "dev" made these
/// tests inherit whatever the shared development database happened to hold —
/// including, once local accounts landed, a default tenant already committed
/// to password sign-in, which refuses OIDC identities by design. The tests
/// were reporting that as a bug in provisioning. Provisioning their own tenant
/// makes them independent of ambient state, which is what they were always
/// assuming.
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
        default_tenant_name: format!("test-{}", uuid::Uuid::now_v7().simple()),
        auth_dev_mode: true,
        mcp_token: None,
        dev_join_token: None,
        dist_dir: "/nonexistent".into(),
        // Port 0 = let the OS pick; these tests never bind it.
        agent_bind: "127.0.0.1:0".into(),
        agent_public_url: None,
        agent_tls_cert: None,
        agent_tls_key: None,
        releases_repo: "nook-os/nook-os".into(),
        // Artifact storage is irrelevant to tenant isolation; disk with a
        // nonexistent directory keeps these tests off the network.
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

fn claims(subject: &str, name: &str) -> IdentityClaims {
    claims_verified(subject, name, false)
}

fn claims_verified(subject: &str, name: &str, email_verified: bool) -> IdentityClaims {
    IdentityClaims {
        issuer: "test-idp".into(),
        subject: subject.into(),
        email: Some(format!("{subject}@example.test")),
        email_verified,
        display_name: Some(name.into()),
        avatar_url: None,
        raw_claims: serde_json::json!({}),
    }
}

/// Remove tenants a test created — never the seeded one.
///
/// The first identity on a fresh instance *adopts* the seeded tenant rather
/// than making its own, so a test can end up holding a tenant it did not
/// create. Deleting that takes the dev instance's board, join token and node
/// with it.
async fn cleanup(pool: &PgPool, tenants: &[TenantId]) {
    for t in tenants {
        let _ = sqlx::query("DELETE FROM tenants WHERE id = $1 AND slug <> 'dev'")
            .bind(t)
            .execute(pool)
            .await;
    }
}

/// The whole point: two people signing in do not end up in one tenant.
#[tokio::test]
async fn each_new_user_gets_their_own_tenant() {
    let _serial = SERIAL.lock().await;
    let Some(pool) = test_pool().await else {
        eprintln!("skipping: DATABASE_URL not set / postgres unreachable");
        return;
    };
    let state = AppState::new(pool.clone(), test_config(), None).await;

    // Unique subjects so the test is re-runnable against a live dev database.
    let a_sub = format!("alice-{}", Uuid::now_v7().simple());
    let b_sub = format!("bob-{}", Uuid::now_v7().simple());

    let (a_user, a_tenant) = login_identity(&state, claims(&a_sub, "Alice"))
        .await
        .expect("alice signs in");
    let (b_user, b_tenant) = login_identity(&state, claims(&b_sub, "Bob"))
        .await
        .expect("bob signs in");

    assert_ne!(
        a_tenant.id, b_tenant.id,
        "two new users landed in the same tenant — they would see each other's nodes"
    );
    assert_eq!(a_user.tenant_id, a_tenant.id);
    assert_eq!(b_user.tenant_id, b_tenant.id);
    assert_eq!(a_user.role, "owner", "you own the tenant made for you");
    assert_eq!(b_user.role, "owner");

    // Neither can see the other: scoping is by tenant, and the tenants differ.
    let (shared,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM tenant_members
         WHERE principal_id IN ($1, $2) AND tenant_id IN ($3, $4)",
    )
    .bind(a_user.id.0)
    .bind(b_user.id.0)
    .bind(a_tenant.id)
    .bind(b_tenant.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        shared, 2,
        "each user belongs to exactly one of the two tenants"
    );

    cleanup(&pool, &[a_tenant.id, b_tenant.id]).await;
}

/// Signing in again is not a new tenant — the identity is already known.
#[tokio::test]
async fn returning_user_keeps_their_tenant() {
    let _serial = SERIAL.lock().await;
    let Some(pool) = test_pool().await else {
        eprintln!("skipping: DATABASE_URL not set / postgres unreachable");
        return;
    };
    let state = AppState::new(pool.clone(), test_config(), None).await;
    let sub = format!("carol-{}", Uuid::now_v7().simple());

    let (first_user, first_tenant) = login_identity(&state, claims(&sub, "Carol"))
        .await
        .expect("first sign-in");
    let (again_user, again_tenant) = login_identity(&state, claims(&sub, "Carol"))
        .await
        .expect("second sign-in");

    assert_eq!(first_tenant.id, again_tenant.id);
    assert_eq!(first_user.id, again_user.id);

    let (tenant_count,): (i64,) = sqlx::query_as("SELECT count(*) FROM tenants WHERE id = $1")
        .bind(first_tenant.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(tenant_count, 1);

    cleanup(&pool, &[first_tenant.id]).await;
}

/// Membership is written alongside the user, because that table is what teams
/// will read — if provisioning skips it, a user belongs to a tenant by one
/// rule and not by the other.
#[tokio::test]
async fn membership_row_mirrors_the_personal_tenant() {
    let _serial = SERIAL.lock().await;
    let Some(pool) = test_pool().await else {
        eprintln!("skipping: DATABASE_URL not set / postgres unreachable");
        return;
    };
    let state = AppState::new(pool.clone(), test_config(), None).await;
    let sub = format!("dave-{}", Uuid::now_v7().simple());

    let (user, tenant) = login_identity(&state, claims(&sub, "Dave"))
        .await
        .expect("dave signs in");

    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT principal_type, role FROM tenant_members
         WHERE tenant_id = $1 AND principal_id = $2",
    )
    .bind(tenant.id)
    .bind(user.id.0)
    .fetch_optional(&pool)
    .await
    .unwrap();

    let (principal_type, role) = row.expect("membership row was not written");
    assert_eq!(principal_type, "user");
    assert_eq!(role, "owner");

    cleanup(&pool, &[tenant.id]).await;
}

/// There is no way to make two people share a tenant by signing in. The flag
/// that used to do it is gone, so this asserts the property directly: two new
/// identities, two tenants, no configuration involved.
#[tokio::test]
async fn every_new_identity_gets_its_own_tenant() {
    let _serial = SERIAL.lock().await;
    let Some(pool) = test_pool().await else {
        eprintln!("skipping: DATABASE_URL not set / postgres unreachable");
        return;
    };
    let state = AppState::new(pool.clone(), test_config(), None).await;

    let a = format!("erin-{}", Uuid::now_v7().simple());
    let b = format!("frank-{}", Uuid::now_v7().simple());
    let (a_user, a_tenant) = login_identity(&state, claims(&a, "Erin")).await.unwrap();
    let (b_user, b_tenant) = login_identity(&state, claims(&b, "Frank")).await.unwrap();

    assert_ne!(
        a_tenant.id, b_tenant.id,
        "signing in must never drop a new person into someone else's tenant"
    );
    assert_eq!(
        a_user.role, "owner",
        "a personal tenant is owned by its person"
    );
    assert_eq!(b_user.role, "owner");

    cleanup(&pool, &[a_tenant.id, b_tenant.id]).await;
}

/// A node token is a service credential, not the owner's password.
///
/// It authenticates every machine that joined, and it sits in a plain file on
/// a box whose job is running other people's code. So it may do a node's work
/// — read the tenant, drive sessions — but not the things that hand over
/// lasting control: the vault, enrolling machines, evicting other nodes.
#[tokio::test]
async fn node_tokens_cannot_escalate() {
    use nook_control::auth::{AuthCtx, Principal};
    use nook_types::{AuthSessionId, NodeId, UserId};

    let node = AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: UserId(Uuid::nil()),
        tenant_id: TenantId(Uuid::nil()),
        principal: Principal::Node(NodeId(Uuid::now_v7())),
        cookie_session: false,
    };
    let human = AuthCtx {
        principal: Principal::User,
        ..node
    };

    assert!(
        node.require_user().is_err(),
        "a node token must be refused for owner-only operations"
    );
    assert!(human.require_user().is_ok(), "a signed-in user must not be");
}

/// A node token is confined to its own machine.
///
/// This is the lateral-movement boundary: starting a session, cloning, or
/// attaching a terminal all execute code on the node they name. One
/// compromised machine must not become every machine.
#[tokio::test]
async fn node_tokens_are_confined_to_their_own_machine() {
    use nook_control::auth::{AuthCtx, Principal};
    use nook_types::{AuthSessionId, NodeId, UserId};

    let self_id = NodeId(Uuid::now_v7());
    let other_id = NodeId(Uuid::now_v7());

    let node = AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: UserId(Uuid::nil()),
        tenant_id: TenantId(Uuid::nil()),
        principal: Principal::Node(self_id),
        cookie_session: false,
    };
    let human = AuthCtx {
        principal: Principal::User,
        ..node
    };

    assert!(
        node.require_node_self(self_id).is_ok(),
        "a node must still be able to act on itself — that is the CLI"
    );
    assert!(
        node.require_node_self(other_id).is_err(),
        "a node token reached another machine: lateral movement is open"
    );
    assert!(
        human.require_node_self(other_id).is_ok(),
        "driving other nodes is what the control plane is for"
    );
}

/// MAIN-29: an OIDC login whose IdP asserts `email_verified=true` stamps the
/// identity, and the predicate reports it. An unverified claim leaves it null.
#[tokio::test]
async fn oidc_email_verified_claim_sets_the_timestamp_and_predicate() {
    let _serial = SERIAL.lock().await;
    let Some(pool) = test_pool().await else {
        eprintln!("skipping: DATABASE_URL not set / postgres unreachable");
        return;
    };
    let state = AppState::new(pool.clone(), test_config(), None).await;

    let v_sub = format!("verified-{}", Uuid::now_v7().simple());
    let (v_user, v_tenant) = login_identity(&state, claims_verified(&v_sub, "Vera", true))
        .await
        .expect("verified user signs in");
    let u_sub = format!("unverified-{}", Uuid::now_v7().simple());
    let (u_user, u_tenant) = login_identity(&state, claims_verified(&u_sub, "Uri", false))
        .await
        .expect("unverified user signs in");

    // The column reflects the claim…
    let (v_at,): (Option<chrono::DateTime<chrono::Utc>>,) =
        sqlx::query_as("SELECT email_verified_at FROM identities WHERE subject = $1")
            .bind(&v_sub)
            .fetch_one(&pool)
            .await
            .unwrap();
    let (u_at,): (Option<chrono::DateTime<chrono::Utc>>,) =
        sqlx::query_as("SELECT email_verified_at FROM identities WHERE subject = $1")
            .bind(&u_sub)
            .fetch_one(&pool)
            .await
            .unwrap();

    // …and so does the predicate.
    let v_pred = email_is_verified(&pool, v_user.id).await.unwrap();
    let u_pred = email_is_verified(&pool, u_user.id).await.unwrap();

    cleanup(&pool, &[v_tenant.id, u_tenant.id]).await;

    assert!(
        v_at.is_some(),
        "email_verified=true must stamp the timestamp"
    );
    assert!(
        u_at.is_none(),
        "an unverified claim must leave the timestamp null"
    );
    assert!(v_pred, "predicate is true for a verified identity");
    assert!(!u_pred, "predicate is false when the timestamp is null");
}

/// MAIN-29: a returning identity that was unverified becomes verified the first
/// time the IdP asserts it — verification only moves one way.
#[tokio::test]
async fn returning_login_records_a_newly_verified_email() {
    let _serial = SERIAL.lock().await;
    let Some(pool) = test_pool().await else {
        eprintln!("skipping: DATABASE_URL not set / postgres unreachable");
        return;
    };
    let state = AppState::new(pool.clone(), test_config(), None).await;
    let sub = format!("laterverify-{}", Uuid::now_v7().simple());

    let (user, tenant) = login_identity(&state, claims_verified(&sub, "Lee", false))
        .await
        .expect("first sign-in, unverified");
    assert!(
        !email_is_verified(&pool, user.id).await.unwrap(),
        "starts unverified"
    );

    // The IdP now confirms the address.
    login_identity(&state, claims_verified(&sub, "Lee", true))
        .await
        .expect("second sign-in, now verified");
    let verified = email_is_verified(&pool, user.id).await.unwrap();

    cleanup(&pool, &[tenant.id]).await;
    assert!(verified, "a later verified login records the verification");
}
