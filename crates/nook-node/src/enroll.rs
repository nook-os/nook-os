//! Getting this machine a certificate.
//!
//! The keypair is generated here and the private half never leaves — the
//! control plane only ever sees a CSR, so it cannot leak what it was never
//! given. That is also what makes renewal self-service: the key is the durable
//! identity, so a machine can prove itself months later without a fresh join
//! token.

use anyhow::{bail, Context, Result};
use nook_types::EnrollResponse;

use crate::config::{cert_paths, NodeConfig};

/// Generate a keypair and a CSR for this machine.
///
/// The subject is a hint only — the control plane overwrites it with the
/// identity it decides, since a CSR is something a server must never trust.
fn make_csr(hint: &str) -> Result<(String, String)> {
    use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};
    let key = KeyPair::generate()?;
    let mut params = CertificateParams::new(Vec::<String>::new())?;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, hint.to_string());
    params.distinguished_name = dn;
    let csr = params.serialize_request(&key)?.pem()?;
    Ok((csr, key.serialize_pem()))
}

/// Write the certificate and key beside `node.toml`, key mode 0600.
fn save_identity(cert_pem: &str, key_pem: &str) -> Result<()> {
    let (cert_path, key_path) = cert_paths()?;
    if let Some(dir) = cert_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&cert_path, cert_pem)?;
    std::fs::write(&key_path, key_pem)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // The key is this machine's identity. Nothing else on the box needs it.
        let _ = std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600));
        let _ = std::fs::set_permissions(&cert_path, std::fs::Permissions::from_mode(0o644));
    }
    println!("✓ certificate {}", cert_path.display());
    println!(
        "✓ private key {} (0600, never leaves this machine)",
        key_path.display()
    );
    Ok(())
}

/// `nook enroll --token nook_join_…` — trade a join token for a certificate.
pub async fn enroll(
    token: &str,
    server: Option<&str>,
    name: Option<&str>,
    fingerprint: Option<&str>,
) -> Result<()> {
    let existing = NodeConfig::load().ok();
    let server = server
        .map(str::to_string)
        .or_else(|| existing.as_ref().map(|c| c.server.clone()))
        .context("no --server given and this machine has not joined")?;
    let server = server.trim_end_matches('/').to_string();

    // Same rule as every other path: refuse plaintext unless explicitly allowed.
    let insecure = crate::config::check_server_security(&server, false)?;
    crate::config::warn_if_insecure(insecure, &server);

    let hostname = existing
        .as_ref()
        .map(|c| c.node_name.clone())
        .or_else(|| name.map(str::to_string))
        .unwrap_or_else(hostname_guess);

    println!("▸ generating a keypair for {hostname}");
    let (csr_pem, key_pem) = make_csr(&hostname)?;

    println!("▸ requesting a certificate from {server}");
    let resp = http_client(fingerprint)?
        .post(format!("{server}/api/v1/nodes/enroll"))
        .json(&serde_json::json!({
            "token": token,
            "csr_pem": csr_pem,
            "name": hostname,
        }))
        .send()
        .await
        .with_context(|| format!("cannot reach {server}"))?;

    if !resp.status().is_success() {
        let code = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("enrolment refused ({code}): {}", body.trim());
    }
    let issued: EnrollResponse = resp.json().await.context("bad enrolment response")?;

    save_identity(&issued.cert_pem, &key_pem)?;
    save_bundle(&issued.ca_bundle)?;
    // Recorded here as well as on renewal: a freshly enrolled node that did
    // not know its own expiry would renew on its very first check, treating
    // "I cannot tell" as "renew".
    save_expiry(issued.not_after)?;

    // Record the identity so `nook run` finds it. Keep whatever else was
    // already configured — enrolling must not undo `nook setup`.
    let mut cfg = existing.unwrap_or(NodeConfig {
        server: server.clone(),
        node_id: issued.node_id.to_string(),
        node_name: hostname.clone(),
        node_token: String::new(),
        workspace_roots: vec!["~/.nook/workspace".into()],
        ssh_key_path: None,
        server_fingerprint: None,
        agent_server: None,
        service: None,
    });
    // The agent endpoint is where the certificate is actually used; the API
    // may well live elsewhere, so record it separately rather than moving
    // `server` out from under `nook get`.
    cfg.agent_server = Some(server);
    cfg.node_id = issued.node_id.to_string();
    if let Some(fp) = fingerprint {
        cfg.server_fingerprint = Some(fp.to_string());
    }
    cfg.save()?;

    println!(
        "✓ enrolled as {} — certificate valid until {}",
        issued.node_id, issued.not_after
    );
    println!("  Restart the agent to connect with it: systemctl restart nook-node");
    Ok(())
}

/// `nook renew` — a fresh certificate on the key this machine already holds.
///
/// No join token: that is the point. A machine offline for months comes back
/// and renews itself.
pub async fn renew() -> Result<()> {
    let issued = renew_now().await?;
    println!("✓ renewed — valid until {}", issued);
    Ok(())
}

/// The renewal itself. Returns when the new certificate expires.
///
/// Split from the command so the agent's automatic check can call it without
/// printing to a stdout nobody is reading — and so both paths renew in exactly
/// one way.
pub async fn renew_now() -> Result<chrono::DateTime<chrono::Utc>> {
    let cfg = NodeConfig::load().context("this machine has not joined")?;
    let (_, key_path) = cert_paths()?;
    let key_pem = std::fs::read_to_string(&key_path)
        .with_context(|| format!("no key at {} — run `nook enroll` first", key_path.display()))?;

    let insecure = crate::config::check_server_security(cfg.agent_endpoint(), false)?;
    crate::config::warn_if_insecure(insecure, cfg.agent_endpoint());

    // Re-use the SAME key: that is what identifies us.
    let key = rcgen::KeyPair::from_pem(&key_pem).context("stored key is unusable")?;
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new())?;
    let mut dn = rcgen::DistinguishedName::new();
    dn.push(rcgen::DnType::CommonName, cfg.node_name.clone());
    params.distinguished_name = dn;
    let csr_pem = params.serialize_request(&key)?.pem()?;

    let resp = http_client(cfg.server_fingerprint.as_deref())?
        .post(format!(
            "{}/api/v1/nodes/renew",
            cfg.agent_endpoint().trim_end_matches('/')
        ))
        .json(&serde_json::json!({ "node_id": cfg.node_id, "csr_pem": csr_pem }))
        .send()
        .await
        .with_context(|| format!("cannot reach {}", cfg.server))?;

    if !resp.status().is_success() {
        let code = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("renewal refused ({code}): {}", body.trim());
    }
    let issued: EnrollResponse = resp.json().await.context("bad renewal response")?;

    // Only the certificate changes; the key stays put.
    save_identity(&issued.cert_pem, &key_pem)?;
    save_bundle(&issued.ca_bundle)?;
    save_expiry(issued.not_after)?;
    Ok(issued.not_after)
}

/// Remember when our certificate expires.
///
/// Written beside the certificate rather than parsed back out of it: the node
/// already has the answer from the server, and adding an X.509 parser to read
/// a number we were just handed would be a dependency bought for nothing.
fn save_expiry(not_after: chrono::DateTime<chrono::Utc>) -> Result<()> {
    let (cert_path, _) = cert_paths()?;
    let dir = cert_path.parent().context("no config directory")?;
    std::fs::write(dir.join("cert-expiry"), not_after.to_rfc3339())?;
    Ok(())
}

/// When our certificate expires, if we know.
///
/// `None` on a node enrolled before this was recorded, which the renewal policy
/// treats as "renew" — see `certs::Reason::Unknown`.
pub fn expiry() -> Option<chrono::DateTime<chrono::Utc>> {
    let (cert_path, _) = cert_paths().ok()?;
    let raw = std::fs::read_to_string(cert_path.parent()?.join("cert-expiry")).ok()?;
    chrono::DateTime::parse_from_rfc3339(raw.trim())
        .ok()
        .map(|d| d.with_timezone(&chrono::Utc))
}

/// Fingerprints of every CA in our trust bundle, for comparing against what
/// the control plane says this tenant trusts.
pub fn held_ca_fingerprints() -> Vec<String> {
    let Ok((cert_path, _)) = cert_paths() else {
        return vec![];
    };
    let Some(dir) = cert_path.parent() else {
        return vec![];
    };
    let Ok(pem) = std::fs::read_to_string(dir.join("ca-bundle.crt")) else {
        return vec![];
    };
    let mut reader = std::io::BufReader::new(pem.as_bytes());
    rustls_pemfile::certs(&mut reader)
        .filter_map(Result::ok)
        .map(|der| crate::pinning::fingerprint(&der))
        .collect()
}

/// Keep the tenant's whole trust bundle, not just our own certificate.
///
/// A node that stored only its leaf would stay pinned to a CA being retired,
/// which is what turns a rotation into an outage.
fn save_bundle(bundle: &[String]) -> Result<()> {
    let (cert_path, _) = cert_paths()?;
    let dir = cert_path.parent().context("no config directory")?;
    let path = dir.join("ca-bundle.crt");
    std::fs::write(&path, bundle.join("\n"))?;
    println!("✓ trust bundle {} ({} CA(s))", path.display(), bundle.len());
    Ok(())
}

/// An HTTP client that honours the pin when one was supplied.
///
/// Enrolment is exactly the exchange a man-in-the-middle would want to sit in —
/// it ends with this machine holding a signed identity — so if a fingerprint
/// came with the invitation, it has to apply *here*, not only later.
fn http_client(fingerprint: Option<&str>) -> Result<reqwest::Client> {
    let Some(fp) = fingerprint else {
        return Ok(reqwest::Client::new());
    };
    let tls = crate::pinning::pinned_client_config(fp);
    Ok(reqwest::Client::builder()
        .use_preconfigured_tls(tls)
        .build()?)
}

fn hostname_guess() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "node".into())
}
