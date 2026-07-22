//! Per-tenant certificate authorities.
//!
//! Each tenant signs its own machines. That is the whole point: one control
//! plane serves many tenants, so a compromised signing key must cost one
//! customer's fleet rather than everyone's.
//!
//! Two rules run through everything here:
//!
//! **Trust is a bundle, signing is one key.** Verification accepts any CA the
//! tenant currently trusts; exactly one of them signs. Rotation is moving CAs
//! through `staged → active → retiring`, and building it in from the start is
//! deliberate — retrofitting "trust more than one CA" onto a system that
//! assumed one is far harder than starting with a set.
//!
//! **Never regenerate implicitly.** If a tenant has a CA on record and it
//! cannot be loaded or verified, that is an incident, not first boot. Silently
//! minting a replacement would orphan every node in the tenant — they would
//! keep presenting certificates signed by a key the server no longer knows —
//! so the load path refuses and says so.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, Utc};
use nook_types::TenantId;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

/// CAs outlive the leaves they sign by a wide margin — rotating a CA is a
/// fleet-wide operation, rotating a leaf is routine.
const CA_VALIDITY_DAYS: i64 = 3650;

/// A tenant CA as stored. The private key is never in this struct; it is
/// decrypted only inside `load_signer`, which verifies the fingerprint first.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TenantCa {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub state: String,
    pub cert_pem: String,
    pub fingerprint: String,
    pub not_after: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

/// SHA-256 over the certificate DER — the identity we pin and compare.
pub fn fingerprint_der(der: &[u8]) -> String {
    format!("{:x}", Sha256::digest(der))
}

/// Fingerprint of a PEM certificate, for comparing what we loaded against what
/// we recorded.
pub fn fingerprint_pem(pem: &str) -> Result<String> {
    let der = pem_to_der(pem)?;
    Ok(fingerprint_der(&der))
}

fn pem_to_der(pem: &str) -> Result<Vec<u8>> {
    use base64::Engine;
    let body: String = pem
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .collect::<Vec<_>>()
        .join("");
    base64::engine::general_purpose::STANDARD
        .decode(body.trim())
        .context("certificate is not valid PEM")
}

/// Mint a CA for a tenant.
///
/// `make_active` is for the first one: a tenant with no CA needs a signer
/// immediately, whereas a rotation stages the new CA and promotes it later,
/// once nodes have had a chance to pick it up.
pub async fn generate(
    db: &PgPool,
    vault: &crate::crypto::Vault,
    tenant: TenantId,
    make_active: bool,
) -> Result<TenantCa> {
    use rcgen::{BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair};

    let mut params = CertificateParams::new(Vec::<String>::new())?;
    let mut dn = DistinguishedName::new();
    // The tenant is in the subject so a certificate chain says which fleet it
    // belongs to without a database lookup.
    dn.push(DnType::CommonName, format!("NookOS tenant {tenant} CA"));
    dn.push(DnType::OrganizationName, "NookOS");
    params.distinguished_name = dn;
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    let not_after = Utc::now() + Duration::days(CA_VALIDITY_DAYS);
    {
        use chrono::Datelike;
        params.not_after = rcgen::date_time_ymd(
            not_after.year(),
            not_after.month() as u8,
            not_after.day() as u8,
        );
    }

    let key = KeyPair::generate()?;
    let cert = params.self_signed(&key)?;

    let cert_pem = cert.pem();
    let fingerprint = fingerprint_der(cert.der());
    // The private key is sealed before it ever touches a row, with the same
    // vault key that protects git credentials and workspace secrets.
    let key_enc = vault
        .encrypt(key.serialize_pem().as_bytes())
        .map_err(|e| anyhow::anyhow!("sealing the CA key failed: {e}"))?;

    let state = if make_active { "active" } else { "staged" };
    let row: TenantCa = sqlx::query_as(
        "INSERT INTO tenant_cas (id, tenant_id, state, cert_pem, key_enc, fingerprint, not_after)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         RETURNING id, tenant_id, state, cert_pem, fingerprint, not_after, created_at",
    )
    .bind(Uuid::now_v7())
    .bind(tenant)
    .bind(state)
    .bind(&cert_pem)
    .bind(&key_enc)
    .bind(&fingerprint)
    .bind(not_after)
    .fetch_one(db)
    .await?;
    Ok(row)
}

/// Every CA this tenant trusts, in any state — what a node must accept.
///
/// A node that refreshed only its own certificate would stay pinned to a CA
/// you are trying to retire, so enrolment and renewal both return this whole
/// set. That is what makes rotation a background process rather than a
/// fleet-wide outage.
pub async fn trust_bundle(db: &PgPool, tenant: TenantId) -> Result<Vec<TenantCa>> {
    let rows: Vec<TenantCa> = sqlx::query_as(
        "SELECT id, tenant_id, state, cert_pem, fingerprint, not_after, created_at
           FROM tenant_cas WHERE tenant_id = $1 ORDER BY created_at",
    )
    .bind(tenant)
    .fetch_all(db)
    .await?;
    Ok(rows)
}

/// The tenant's signing key, verified before use.
///
/// Returns the CA record plus the decrypted key PEM. Refuses — loudly — if the
/// stored certificate does not match its recorded fingerprint, rather than
/// signing with something that isn't what the tenant enrolled against.
pub async fn load_signer(
    db: &PgPool,
    vault: &crate::crypto::Vault,
    tenant: TenantId,
) -> Result<(TenantCa, String)> {
    let row: Option<(TenantCa, Vec<u8>)> = sqlx::query_as(
        "SELECT id, tenant_id, state, cert_pem, fingerprint, not_after, created_at, key_enc
           FROM tenant_cas WHERE tenant_id = $1 AND state = 'active'",
    )
    .bind(tenant)
    .fetch_optional(db)
    .await
    .map(|o| {
        o.map(
            |r: (
                Uuid,
                Uuid,
                String,
                String,
                String,
                DateTime<Utc>,
                DateTime<Utc>,
                Vec<u8>,
            )| {
                (
                    TenantCa {
                        id: r.0,
                        tenant_id: r.1,
                        state: r.2,
                        cert_pem: r.3,
                        fingerprint: r.4,
                        not_after: r.5,
                        created_at: r.6,
                    },
                    r.7,
                )
            },
        )
    })?;

    let Some((ca, key_enc)) = row else {
        bail!("tenant {tenant} has no active CA");
    };

    // Fingerprint first: a mismatch means the row was altered underneath us.
    let actual = fingerprint_pem(&ca.cert_pem)?;
    if actual != ca.fingerprint {
        bail!(
            "CA {} for tenant {tenant} does not match its recorded fingerprint \
             (recorded {}, computed {}). Refusing to sign — regenerate explicitly \
             if this is intentional.",
            ca.id,
            ca.fingerprint,
            actual
        );
    }

    let key_pem = vault
        .decrypt_string(&key_enc)
        .map_err(|e| anyhow::anyhow!("cannot decrypt the CA key for tenant {tenant}: {e}"))?;
    Ok((ca, key_pem))
}

/// Promote a staged CA to be the tenant's signer, demoting the current one to
/// `retiring` — it stays trusted, it just stops issuing.
pub async fn promote(db: &PgPool, tenant: TenantId, ca_id: Uuid) -> Result<()> {
    let mut tx = db.begin().await?;
    // Demote first: the partial unique index allows only one active row, so
    // the order matters.
    sqlx::query(
        "UPDATE tenant_cas SET state = 'retiring'
          WHERE tenant_id = $1 AND state = 'active'",
    )
    .bind(tenant)
    .execute(&mut *tx)
    .await?;
    let done = sqlx::query(
        "UPDATE tenant_cas SET state = 'active'
          WHERE id = $1 AND tenant_id = $2 AND state = 'staged'",
    )
    .bind(ca_id)
    .bind(tenant)
    .execute(&mut *tx)
    .await?;
    if done.rows_affected() == 0 {
        tx.rollback().await?;
        bail!("no staged CA {ca_id} for this tenant to promote");
    }
    tx.commit().await?;
    Ok(())
}

/// How many nodes still hold an unexpired leaf signed by this CA.
///
/// The retirement guard, and the number an admin watches during a rotation.
pub async fn live_leaves(db: &PgPool, tenant: TenantId, ca_id: Uuid) -> Result<i64> {
    let (n,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM nodes
          WHERE tenant_id = $1 AND ca_id = $2
            AND revoked_at IS NULL
            AND cert_not_after IS NOT NULL AND cert_not_after > now()",
    )
    .bind(tenant)
    .bind(ca_id)
    .fetch_one(db)
    .await?;
    Ok(n)
}

/// Drop a CA from the tenant's trust bundle.
///
/// Refuses while it has signed a still-valid leaf: removing it then would
/// lock those machines out mid-rotation, which is exactly the outage the
/// staged/active/retiring dance exists to avoid. A check here rather than a
/// step in a runbook, because runbooks are not executed at 2am.
pub async fn retire(db: &PgPool, tenant: TenantId, ca_id: Uuid) -> Result<()> {
    let live = live_leaves(db, tenant, ca_id).await?;
    if live > 0 {
        bail!(
            "refusing to retire CA {ca_id}: {live} node(s) still hold unexpired \
             certificates signed by it. They pick up the new CA as they renew — \
             retire this one once that count reaches zero."
        );
    }
    let done = sqlx::query(
        "DELETE FROM tenant_cas WHERE id = $1 AND tenant_id = $2 AND state <> 'active'",
    )
    .bind(ca_id)
    .bind(tenant)
    .execute(db)
    .await?;
    if done.rows_affected() == 0 {
        bail!("no retirable CA {ca_id} for this tenant (the active signer cannot be retired)");
    }
    Ok(())
}
