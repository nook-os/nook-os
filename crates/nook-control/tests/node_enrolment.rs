//! Enrolment and renewal: the CA actually issuing certificates.
//!
//! The case worth guarding hardest is renewal after a long outage. Machines
//! are shut for weeks; if expiry cost a manual re-join, every laptop that came
//! back from a holiday would need a human and a fresh token.

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

/// A node generating its keypair and CSR, exactly as the agent will.
fn node_csr(node_hint: &str) -> (rcgen::KeyPair, String) {
    use rcgen::{CertificateParams, DistinguishedName, DnType};
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, node_hint.to_string());
    params.distinguished_name = dn;
    let csr = params.serialize_request(&key).unwrap();
    (key, csr.pem().unwrap())
}

async fn seed_tenant(pool: &PgPool) -> TenantId {
    let id = TenantId::new();
    sqlx::query("INSERT INTO tenants (id, slug, name) VALUES ($1, $2, $2)")
        .bind(id)
        .bind(format!("en-{}", Uuid::now_v7().simple()))
        .execute(pool)
        .await
        .expect("tenant");
    id
}

async fn seed_node(pool: &PgPool, tenant: TenantId) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status)
         VALUES ($1, $2, $3, $3, 'offline')",
    )
    .bind(id)
    .bind(tenant)
    .bind(format!("n-{}", Uuid::now_v7().simple()))
    .execute(pool)
    .await
    .expect("node");
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

/// A CSR is signed, and the issued certificate carries the identity the
/// control plane decided — not whatever the CSR asked for.
#[tokio::test]
async fn signs_a_csr_with_server_chosen_identity() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let v = vault();
    let tenant = seed_tenant(&pool).await;
    ca::generate(&pool, &v, tenant, true).await.unwrap();
    let node = seed_node(&pool, tenant).await;

    // The CSR lies about who it is; the issued cert must not repeat the lie.
    let (_key, csr) = node_csr("i-am-somebody-else");
    let leaf = ca::sign_node_csr(&pool, &v, tenant, node, &csr)
        .await
        .unwrap();

    assert!(leaf.cert_pem.contains("BEGIN CERTIFICATE"));
    assert!(leaf.not_after > chrono::Utc::now());
    assert!(!leaf.public_key_pem.is_empty());

    // Parse it back and confirm the subject is the node id we chose.
    let der = {
        use base64::Engine;
        let body: String = leaf
            .cert_pem
            .lines()
            .filter(|l| !l.starts_with("-----"))
            .collect::<Vec<_>>()
            .join("");
        base64::engine::general_purpose::STANDARD
            .decode(body.trim())
            .unwrap()
    };
    let (_, parsed) = x509_parser::parse_x509_certificate(&der).unwrap();
    let subject = parsed.subject().to_string();
    assert!(
        subject.contains(&node.to_string()),
        "subject must be the node id the server chose, got: {subject}"
    );
    assert!(
        !subject.contains("i-am-somebody-else"),
        "the CSR's claimed identity must be discarded, got: {subject}"
    );
    assert!(
        subject.contains(&tenant.to_string()),
        "the tenant must be asserted in the certificate, got: {subject}"
    );

    cleanup(&pool, &[tenant]).await;
}

/// The public key in a CSR round-trips, which is what renewal matches on.
#[tokio::test]
async fn csr_public_key_identifies_the_machine() {
    let (_k1, csr1) = node_csr("a");
    let (_k2, csr2) = node_csr("b");
    let p1 = ca::csr_public_key_pem(&csr1).unwrap();
    let p2 = ca::csr_public_key_pem(&csr2).unwrap();
    assert!(p1.contains("BEGIN PUBLIC KEY"));
    assert_ne!(p1, p2, "different keypairs must be distinguishable");
    // Same CSR, same answer — the comparison renewal relies on is stable.
    assert_eq!(p1, ca::csr_public_key_pem(&csr1).unwrap());
}

/// Renewal works long after expiry, on the node's own key, with no token —
/// and picks up a CA that rotated while the machine was away.
#[tokio::test]
async fn renews_after_expiry_and_across_a_rotation() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let v = vault();
    let tenant = seed_tenant(&pool).await;
    let old_ca = ca::generate(&pool, &v, tenant, true).await.unwrap();
    let node = seed_node(&pool, tenant).await;

    // Enrol.
    let (key, csr) = node_csr("first");
    let first = ca::sign_node_csr(&pool, &v, tenant, node, &csr)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE nodes SET ca_id = $2, cert_not_after = $3, public_key_pem = $4 WHERE id = $1",
    )
    .bind(node)
    .bind(first.ca_id)
    .bind(first.not_after)
    .bind(&first.public_key_pem)
    .execute(&pool)
    .await
    .unwrap();

    // The machine goes away, its certificate expires, and the tenant rotates
    // its CA while nobody is looking.
    sqlx::query("UPDATE nodes SET cert_not_after = now() - interval '90 days' WHERE id = $1")
        .bind(node)
        .execute(&pool)
        .await
        .unwrap();
    let new_ca = ca::generate(&pool, &v, tenant, false).await.unwrap();
    ca::promote(&pool, tenant, new_ca.id).await.unwrap();

    // It comes back and renews on the SAME key — no join token anywhere.
    let (_, csr2) = {
        use rcgen::{CertificateParams, DistinguishedName, DnType};
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, "renewal");
        params.distinguished_name = dn;
        let csr = params.serialize_request(&key).unwrap();
        ((), csr.pem().unwrap())
    };
    assert_eq!(
        ca::csr_public_key_pem(&csr2).unwrap().trim(),
        first.public_key_pem.trim(),
        "renewal must be recognisable as the same machine"
    );

    let renewed = ca::sign_node_csr(&pool, &v, tenant, node, &csr2)
        .await
        .unwrap();
    assert_eq!(
        renewed.ca_id, new_ca.id,
        "renewal must be signed by whichever CA is active NOW"
    );
    assert_ne!(renewed.ca_id, old_ca.id);
    assert!(renewed.not_after > chrono::Utc::now());

    // And the bundle it gets back still trusts the old CA, so it can talk to
    // instances that have not rotated yet.
    let bundle = ca::trust_bundle(&pool, tenant).await.unwrap();
    assert_eq!(bundle.len(), 2);

    cleanup(&pool, &[tenant]).await;
}

/// Signing requires an active CA. A tenant whose CA failed to load must not
/// silently get a new one.
#[tokio::test]
async fn refuses_to_sign_without_an_active_ca() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let v = vault();
    let tenant = seed_tenant(&pool).await;
    let node = seed_node(&pool, tenant).await;
    let (_k, csr) = node_csr("x");

    let err = ca::sign_node_csr(&pool, &v, tenant, node, &csr)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no active CA"), "got: {err}");

    cleanup(&pool, &[tenant]).await;
}

// ── Certificate verification ────────────────────────────────────────────────

fn der_of(pem: &str) -> Vec<u8> {
    use base64::Engine;
    let body: String = pem
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .collect::<Vec<_>>()
        .join("");
    base64::engine::general_purpose::STANDARD
        .decode(body.trim())
        .unwrap()
}

/// A certificate this tenant's CA issued identifies its node.
#[tokio::test]
async fn a_valid_certificate_identifies_its_node() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let v = vault();
    let tenant = seed_tenant(&pool).await;
    ca::generate(&pool, &v, tenant, true).await.unwrap();
    let node = seed_node(&pool, tenant).await;

    let (_k, csr) = node_csr("n");
    let leaf = ca::sign_node_csr(&pool, &v, tenant, node, &csr)
        .await
        .unwrap();

    let id = ca::verify_node_cert(&pool, &der_of(&leaf.cert_pem))
        .await
        .unwrap();
    assert_eq!(id.node_id, node);
    assert_eq!(id.tenant_id, tenant.0);

    cleanup(&pool, &[tenant]).await;
}

/// THE isolation property: a certificate minted by tenant A's CA cannot be
/// used to act as a node in tenant B, even though it is a perfectly valid
/// certificate.
#[tokio::test]
async fn a_certificate_from_another_tenant_is_rejected() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let v = vault();
    let (a, b) = (seed_tenant(&pool).await, seed_tenant(&pool).await);
    ca::generate(&pool, &v, a, true).await.unwrap();
    ca::generate(&pool, &v, b, true).await.unwrap();

    // A node that really belongs to tenant B...
    let node_b = seed_node(&pool, b).await;
    // ...but tenant A's CA signs a certificate naming it.
    let (_k, csr) = node_csr("n");
    let forged = ca::sign_node_csr(&pool, &v, a, node_b, &csr).await.unwrap();

    let err = ca::verify_node_cert(&pool, &der_of(&forged.cert_pem))
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("claims tenant"),
        "a cross-tenant certificate must be refused, got: {err}"
    );

    cleanup(&pool, &[a, b]).await;
}

/// A certificate signed by nobody we trust is not an identity.
#[tokio::test]
async fn a_self_signed_certificate_is_rejected() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let v = vault();
    let tenant = seed_tenant(&pool).await;
    ca::generate(&pool, &v, tenant, true).await.unwrap();
    let node = seed_node(&pool, tenant).await;

    // Forge one with the right names but our own key.
    use rcgen::{CertificateParams, DistinguishedName, DnType};
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, node.to_string());
    dn.push(DnType::OrganizationName, format!("nookos:tenant:{tenant}"));
    params.distinguished_name = dn;
    let forged = params.self_signed(&key).unwrap();

    let err = ca::verify_node_cert(&pool, forged.der()).await.unwrap_err();
    assert!(
        err.to_string().contains("not signed by any CA"),
        "self-signed must not authenticate, got: {err}"
    );

    cleanup(&pool, &[tenant]).await;
}

/// A revoked node is refused even holding a valid, unexpired certificate.
#[tokio::test]
async fn a_revoked_node_is_refused() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let v = vault();
    let tenant = seed_tenant(&pool).await;
    ca::generate(&pool, &v, tenant, true).await.unwrap();
    let node = seed_node(&pool, tenant).await;
    let (_k, csr) = node_csr("n");
    let leaf = ca::sign_node_csr(&pool, &v, tenant, node, &csr)
        .await
        .unwrap();

    sqlx::query("UPDATE nodes SET revoked_at = now() WHERE id = $1")
        .bind(node)
        .execute(&pool)
        .await
        .unwrap();

    let err = ca::verify_node_cert(&pool, &der_of(&leaf.cert_pem))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("revoked"), "got: {err}");

    cleanup(&pool, &[tenant]).await;
}

/// A certificate issued by a now-retiring CA still authenticates while that CA
/// remains in the bundle — that is what makes rotation seamless.
#[tokio::test]
async fn a_leaf_from_a_retiring_ca_still_authenticates() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let v = vault();
    let tenant = seed_tenant(&pool).await;
    let old = ca::generate(&pool, &v, tenant, true).await.unwrap();
    let node = seed_node(&pool, tenant).await;

    let (_k, csr) = node_csr("n");
    let leaf = ca::sign_node_csr(&pool, &v, tenant, node, &csr)
        .await
        .unwrap();
    assert_eq!(leaf.ca_id, old.id);

    // Rotate underneath it.
    let new = ca::generate(&pool, &v, tenant, false).await.unwrap();
    ca::promote(&pool, tenant, new.id).await.unwrap();

    // The old leaf keeps working: the old CA is retiring, not gone.
    let id = ca::verify_node_cert(&pool, &der_of(&leaf.cert_pem))
        .await
        .unwrap();
    assert_eq!(id.node_id, node);

    cleanup(&pool, &[tenant]).await;
}
