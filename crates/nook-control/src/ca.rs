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

/// How long a node's certificate is good for.
///
/// Short and disposable, kubelet-style: the durable identity is the node's
/// keypair, and the certificate is the expiring artifact renewed against it.
/// Short leaves also bound how long a stolen one is useful and let a CA
/// rotation drain in days rather than years.
pub const LEAF_VALIDITY_DAYS: i64 = 30;

/// A freshly issued node certificate.
#[derive(Debug, Clone)]
pub struct IssuedLeaf {
    pub cert_pem: String,
    pub not_after: DateTime<Utc>,
    /// Which CA signed it — recorded on the node so the retirement guard can
    /// answer "does this CA still have live leaves?".
    pub ca_id: Uuid,
    /// The CSR's public key, PEM. Stored as the node's durable identity so a
    /// renewal can be matched to the machine that first enrolled.
    pub public_key_pem: String,
}

/// Sign a node's CSR with the tenant's active CA.
///
/// The subject is overwritten rather than trusted: a CSR is attacker-supplied
/// input, so the identity in the issued certificate is the one the control
/// plane decided, not the one the requester asked for. Everything downstream
/// reads identity off the certificate, which makes this the single point where
/// "who is this machine" is established.
pub async fn sign_node_csr(
    db: &PgPool,
    vault: &crate::crypto::Vault,
    tenant: TenantId,
    node_id: Uuid,
    csr_pem: &str,
) -> Result<IssuedLeaf> {
    use rcgen::{
        CertificateParams, CertificateSigningRequestParams, DistinguishedName, DnType, KeyPair,
        PublicKeyData,
    };

    let (ca, ca_key_pem) = load_signer(db, vault, tenant).await?;

    // Rebuild the issuer from what we stored. `from_ca_cert_pem` keeps the
    // subject and key identifier, so issued certificates chain to the CA the
    // tenant's nodes already trust.
    let issuer_key = KeyPair::from_pem(&ca_key_pem).context("CA key is not a usable keypair")?;
    let issuer_params =
        CertificateParams::from_ca_cert_pem(&ca.cert_pem).context("CA certificate is unusable")?;
    let issuer = issuer_params.self_signed(&issuer_key)?;

    let mut csr = CertificateSigningRequestParams::from_pem(csr_pem)
        .context("that is not a valid certificate signing request")?;

    // Identity is asserted by us, not by the CSR. Both the node and its tenant
    // go in the subject so a presented certificate answers "which machine" and
    // "whose fleet" without a database round-trip.
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, node_id.to_string());
    dn.push(DnType::OrganizationName, format!("nookos:tenant:{tenant}"));
    csr.params.distinguished_name = dn;
    // A node certificate authenticates a client; it must never be able to act
    // as a CA or as a server.
    csr.params.is_ca = rcgen::IsCa::ExplicitNoCa;
    csr.params.use_authority_key_identifier_extension = true;
    csr.params.subject_alt_names = vec![rcgen::SanType::URI(
        format!("nookos://node/{node_id}").try_into()?,
    )];

    let not_after = Utc::now() + Duration::days(LEAF_VALIDITY_DAYS);
    {
        use chrono::Datelike;
        csr.params.not_after = rcgen::date_time_ymd(
            not_after.year(),
            not_after.month() as u8,
            not_after.day() as u8,
        );
    }

    let public_key_pem = pem_wrap("PUBLIC KEY", csr.public_key.der_bytes());
    let leaf = csr.signed_by(&issuer, &issuer_key)?;

    Ok(IssuedLeaf {
        cert_pem: leaf.pem(),
        not_after,
        ca_id: ca.id,
        public_key_pem,
    })
}

/// The public key a CSR carries, PEM — what renewal compares against the key
/// the node enrolled with.
///
/// Parsing the CSR also verifies its self-signature, so a match here means the
/// requester holds the corresponding private key.
pub fn csr_public_key_pem(csr_pem: &str) -> Result<String> {
    use rcgen::{CertificateSigningRequestParams, PublicKeyData};
    let csr = CertificateSigningRequestParams::from_pem(csr_pem)
        .context("that is not a valid certificate signing request")?;
    Ok(pem_wrap("PUBLIC KEY", csr.public_key.der_bytes()))
}

fn pem_wrap(label: &str, der: &[u8]) -> String {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let body = b64
        .as_bytes()
        .chunks(64)
        .map(|c| String::from_utf8_lossy(c).to_string())
        .collect::<Vec<_>>()
        .join("\n");
    format!("-----BEGIN {label}-----\n{body}\n-----END {label}-----\n")
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

// ── Verifying a presented certificate ───────────────────────────────────────

/// Who a validated client certificate says it is.
#[derive(Debug, Clone, Copy)]
pub struct NodeIdentity {
    pub node_id: Uuid,
    pub tenant_id: Uuid,
}

/// Establish identity from a client certificate.
///
/// This is the whole trust decision, so it is deliberately paranoid and every
/// check earns its place:
///
/// 1. The certificate parses and is inside its validity window.
/// 2. Its subject names a node and a tenant — identity we put there, not
///    anything the requester asked for.
/// 3. Its signature verifies against one of THAT TENANT's trusted CAs. Any CA
///    in the bundle is acceptable, which is what lets a rotation proceed
///    without an outage; a CA belonging to a different tenant is not, which is
///    what keeps tenants isolated.
/// 4. The node exists, is not revoked, and its recorded tenant matches the one
///    in the certificate. A certificate that claims a node in someone else's
///    tenant is rejected even if it is otherwise perfectly valid — the
///    certificate proves *who*, the tenant check decides *what*.
pub async fn verify_node_cert(db: &PgPool, cert_der: &[u8]) -> Result<NodeIdentity> {
    use x509_parser::prelude::*;

    let (_, cert) =
        X509Certificate::from_der(cert_der).context("client certificate does not parse")?;

    if !cert.validity().is_valid() {
        bail!("client certificate is outside its validity window");
    }

    let claimed_node = subject_value(&cert, "CN").context("certificate has no node id")?;
    let claimed_org = subject_value(&cert, "O").context("certificate names no tenant")?;
    let claimed_tenant = claimed_org
        .strip_prefix("nookos:tenant:")
        .context("certificate is not a NookOS node certificate")?
        .to_string();

    let node_id: Uuid = claimed_node
        .parse()
        .context("certificate's node id is not a uuid")?;
    let tenant_id: Uuid = claimed_tenant
        .parse()
        .context("certificate's tenant is not a uuid")?;

    // The node record is the authority on which tenant a machine belongs to.
    // Comparing it against the certificate is what stops a valid certificate
    // from one tenant being used to act in another.
    let row: Option<(Uuid, Option<DateTime<Utc>>)> =
        sqlx::query_as("SELECT tenant_id, revoked_at FROM nodes WHERE id = $1")
            .bind(node_id)
            .fetch_optional(db)
            .await?;
    let Some((actual_tenant, revoked_at)) = row else {
        bail!("certificate names node {node_id}, which does not exist");
    };
    if actual_tenant != tenant_id {
        bail!(
            "certificate claims tenant {tenant_id} for node {node_id}, which belongs to {actual_tenant}"
        );
    }
    if revoked_at.is_some() {
        bail!("node {node_id} has been revoked");
    }

    // Finally the signature, against that tenant's bundle only.
    let bundle = trust_bundle(db, TenantId(tenant_id)).await?;
    if bundle.is_empty() {
        bail!("tenant {tenant_id} trusts no CA");
    }
    let mut chained = false;
    for ca in &bundle {
        // `self::` — x509_parser's prelude exports a pem_to_der of its own.
        let der = self::pem_to_der(&ca.cert_pem)?;
        let Ok((_, ca_cert)) = X509Certificate::from_der(&der) else {
            continue;
        };
        if cert.verify_signature(Some(ca_cert.public_key())).is_ok() {
            chained = true;
            break;
        }
    }
    if !chained {
        bail!("client certificate is not signed by any CA this tenant trusts");
    }

    Ok(NodeIdentity { node_id, tenant_id })
}

fn subject_value(cert: &x509_parser::certificate::X509Certificate, key: &str) -> Option<String> {
    cert.subject()
        .iter_attributes()
        .find(|a| a.attr_type().to_id_string() == oid_for(key))
        .and_then(|a| a.as_str().ok())
        .map(str::to_string)
}

fn oid_for(key: &str) -> &'static str {
    match key {
        "CN" => "2.5.4.3",
        "O" => "2.5.4.10",
        _ => "",
    }
}
