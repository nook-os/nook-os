//! Per-tenant CA behaviour: isolation, fingerprint verification, and the full
//! rotation sequence including the retirement guard.
//!
//! Rotation is the part that silently rots if it is only ever checked by hand,
//! so the whole distribute → switch → drain → retire dance is asserted here.
//!
//! Setup + teardown run through `nook_testkit::TestBed` (MAIN-156).

use nook_control::ca;
use nook_control::crypto::Vault;
use nook_db::{params, Db, DbPool};
use nook_testkit::TestBed;
use nook_types::TenantId;
use uuid::Uuid;

fn vault() -> Vault {
    Vault::from_env("test-session-secret-that-is-long-enough-000000").expect("vault")
}

async fn seed_tenant(pool: &DbPool) -> TenantId {
    let id = TenantId::new();
    pool.exec(
        "INSERT INTO tenants (id, slug, name) VALUES ($1, $2, $2)",
        params![id, format!("ca-{}", Uuid::now_v7().simple())],
    )
    .await
    .expect("seed tenant");
    id
}

/// A CA is generated, sealed, and loadable — and the key never comes back in
/// the record, only from the verified load path.
#[tokio::test]
async fn generates_and_loads_a_verified_signer() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let v = vault();
    let tenant = seed_tenant(&bed.db()).await;

    let ca = ca::generate(&cas(&bed), &v, tenant, true).await.unwrap();
    assert_eq!(ca.state, "active");
    assert!(ca.cert_pem.contains("BEGIN CERTIFICATE"));

    let (loaded, key_pem) = ca::load_signer(&cas(&bed), &v, tenant).await.unwrap();
    assert_eq!(loaded.id, ca.id);
    assert!(key_pem.contains("PRIVATE KEY"), "key must decrypt");
    // The fingerprint is computed from the certificate, not merely stored.
    assert_eq!(
        ca::fingerprint_pem(&loaded.cert_pem).unwrap(),
        ca.fingerprint
    );

    bed.teardown().await;
}

/// Two tenants, two CAs, no overlap. The whole reason the CA is per-tenant.
#[tokio::test]
async fn tenants_do_not_share_a_ca() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let v = vault();
    let (a, b) = (seed_tenant(&bed.db()).await, seed_tenant(&bed.db()).await);

    let ca_a = ca::generate(&cas(&bed), &v, a, true).await.unwrap();
    let ca_b = ca::generate(&cas(&bed), &v, b, true).await.unwrap();
    assert_ne!(ca_a.fingerprint, ca_b.fingerprint);

    // Each tenant's bundle contains only its own CA.
    let bundle_a = ca::trust_bundle(&cas(&bed), a).await.unwrap();
    assert_eq!(bundle_a.len(), 1);
    assert_eq!(bundle_a[0].id, ca_a.id);
    assert!(
        !bundle_a.iter().any(|c| c.id == ca_b.id),
        "tenant A must never see tenant B's CA"
    );

    bed.teardown().await;
}

/// A certificate that no longer matches its recorded fingerprint is tampering
/// or corruption. Signing with it anyway would be the silent failure.
#[tokio::test]
async fn refuses_a_signer_whose_fingerprint_does_not_match() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let v = vault();
    let tenant = seed_tenant(&bed.db()).await;
    ca::generate(&cas(&bed), &v, tenant, true).await.unwrap();

    // Swap in a different (validly-formed) certificate behind the fingerprint.
    let other = ca::generate(&cas(&bed), &v, seed_tenant(&bed.db()).await, false)
        .await
        .unwrap();
    bed.db()
        .exec(
            "UPDATE tenant_cas SET cert_pem = $2 WHERE tenant_id = $1 AND state = 'active'",
            params![tenant, &other.cert_pem],
        )
        .await
        .unwrap();

    let err = ca::load_signer(&cas(&bed), &v, tenant).await.unwrap_err();
    assert!(
        err.to_string().contains("fingerprint"),
        "must refuse on fingerprint mismatch, got: {err}"
    );

    bed.teardown().await;
}

/// The full rotation: stage (trusted, not signing) → promote → drain → retire,
/// with the guard refusing while a live leaf still chains to the old CA.
#[tokio::test]
async fn rotation_distributes_then_switches_then_retires() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let v = vault();
    let tenant = seed_tenant(&bed.db()).await;

    let old = ca::generate(&cas(&bed), &v, tenant, true).await.unwrap();

    // A node holding a live leaf signed by the old CA.
    let node = Uuid::now_v7();
    // The expiry is computed in Rust and bound, not written as
    // `now() + interval '30 days'` — a Postgres-only spelling, and this binary
    // also runs on SQLite (MAIN-438).
    bed.db().exec("INSERT INTO nodes (id, tenant_id, name, node_token_hash, status, ca_id, cert_not_after)
         VALUES ($1, $2, $3, $3, 'online', $4, $5)", params![node, tenant, format!("n-{}", Uuid::now_v7().simple()), old.id, chrono::Utc::now() + chrono::Duration::days(30)])
    .await
    .unwrap();

    // ── distribute ──────────────────────────────────────────────────────
    let new = ca::generate(&cas(&bed), &v, tenant, false).await.unwrap();
    assert_eq!(new.state, "staged");
    let bundle = ca::trust_bundle(&cas(&bed), tenant).await.unwrap();
    assert_eq!(bundle.len(), 2, "both CAs are trusted during a rotation");
    // ...but the signer is still the old one.
    let (signer, _) = ca::load_signer(&cas(&bed), &v, tenant).await.unwrap();
    assert_eq!(signer.id, old.id, "staging must not change who signs");

    // ── switch ──────────────────────────────────────────────────────────
    ca::promote(&cas(&bed), tenant, new.id).await.unwrap();
    let (signer, _) = ca::load_signer(&cas(&bed), &v, tenant).await.unwrap();
    assert_eq!(signer.id, new.id);
    // The old CA is still trusted — nodes have not renewed yet.
    assert_eq!(ca::trust_bundle(&cas(&bed), tenant).await.unwrap().len(), 2);

    // ── the guard ───────────────────────────────────────────────────────
    assert_eq!(
        ca::live_leaves(&nodes(&bed), tenant, old.id).await.unwrap(),
        1
    );
    let err = ca::retire(&cas(&bed), &nodes(&bed), tenant, old.id)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("still hold unexpired"),
        "must refuse to retire a CA with live leaves, got: {err}"
    );

    // ── drain, then retire ──────────────────────────────────────────────
    // The node renews onto the new CA (what enrolment will do for real).
    bed.db()
        .exec(
            "UPDATE nodes SET ca_id = $2 WHERE id = $1",
            params![node, new.id],
        )
        .await
        .unwrap();
    assert_eq!(
        ca::live_leaves(&nodes(&bed), tenant, old.id).await.unwrap(),
        0
    );

    ca::retire(&cas(&bed), &nodes(&bed), tenant, old.id)
        .await
        .unwrap();
    let bundle = ca::trust_bundle(&cas(&bed), tenant).await.unwrap();
    assert_eq!(bundle.len(), 1);
    assert_eq!(bundle[0].id, new.id);

    bed.teardown().await;
}

/// The active signer is never retirable — that would leave the tenant unable
/// to issue anything.
#[tokio::test]
async fn the_active_signer_cannot_be_retired() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let v = vault();
    let tenant = seed_tenant(&bed.db()).await;
    let ca_row = ca::generate(&cas(&bed), &v, tenant, true).await.unwrap();

    let err = ca::retire(&cas(&bed), &nodes(&bed), tenant, ca_row.id)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no retirable CA"), "got: {err}");

    bed.teardown().await;
}

// ── Authorization ───────────────────────────────────────────────────────────

use nook_control::auth::{AuthCtx, Principal};
use nook_types::{AuthSessionId, UserId};

/// The CA and node repositories over the bed's pool. Tests keep raw DB access
/// (the chain's NG-4); these just spell the repositories the migrated `ca::`
/// functions now take.
fn cas(bed: &TestBed) -> nook_control::repo::nodes::DbTenantCaRepository {
    nook_control::repo::nodes::DbTenantCaRepository::new(bed.db())
}

fn nodes(bed: &TestBed) -> nook_control::repo::nodes::DbNodeRepository {
    nook_control::repo::nodes::DbNodeRepository::new(bed.db())
}

async fn seed_user(pool: &DbPool, tenant: TenantId, role: &str) -> UserId {
    let id = UserId::new();
    pool.exec(
        "INSERT INTO users (id, tenant_id, display_name, email, role)
         VALUES ($1, $2, $3, $4, $5)",
        params![
            id,
            tenant,
            role,
            format!("{}@example.test", Uuid::now_v7().simple()),
            role
        ],
    )
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
        cookie_session: true,
    }
}

/// Owners and admins may run CA operations; a plain member may not.
#[tokio::test]
async fn ca_operations_need_owner_or_admin() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = seed_tenant(&bed.db()).await;
    let state = bed.app_state().await;

    for role in ["owner", "admin"] {
        let u = seed_user(&bed.db(), tenant, role).await;
        assert!(
            ctx(u, tenant).require_tenant_admin(&state).await.is_ok(),
            "{role} must be allowed"
        );
    }
    let member = seed_user(&bed.db(), tenant, "member").await;
    assert!(
        ctx(member, tenant)
            .require_tenant_admin(&state)
            .await
            .is_err(),
        "a member must not run CA operations"
    );

    bed.teardown().await;
}

/// An admin of one tenant is not an admin of another. The role lookup is
/// scoped by the authenticated tenant, so a forged context gets nothing.
#[tokio::test]
async fn a_tenant_admin_cannot_reach_another_tenant() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (a, b) = (seed_tenant(&bed.db()).await, seed_tenant(&bed.db()).await);
    let state = bed.app_state().await;

    let admin_a = seed_user(&bed.db(), a, "owner").await;
    // Their own tenant: fine.
    assert!(ctx(admin_a, a).require_tenant_admin(&state).await.is_ok());
    // Someone else's: they hold no role there, so the lookup finds nothing.
    assert!(
        ctx(admin_a, b).require_tenant_admin(&state).await.is_err(),
        "tenant A's owner must not be an admin of tenant B"
    );

    bed.teardown().await;
}

/// A machine credential can never run CA operations, whatever the tenant's
/// roles say — that is how a stolen node token stays confined.
#[tokio::test]
async fn a_node_credential_cannot_run_ca_operations() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = seed_tenant(&bed.db()).await;
    let state = bed.app_state().await;
    let owner = seed_user(&bed.db(), tenant, "owner").await;

    let as_node = AuthCtx {
        principal: Principal::Node(nook_types::NodeId::new()),
        ..ctx(owner, tenant)
    };
    assert!(
        as_node.require_tenant_admin(&state).await.is_err(),
        "a node credential must never reach CA lifecycle actions"
    );

    bed.teardown().await;
}
