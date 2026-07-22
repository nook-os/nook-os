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
