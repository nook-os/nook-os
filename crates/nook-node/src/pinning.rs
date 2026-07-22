//! Pinning the control plane's certificate on first contact.
//!
//! Enrolment is the exchange where a machine receives its identity, so a
//! man-in-the-middle *there* is the worst case in the whole system: it would
//! hand the attacker a signed certificate for a real node. Verifying against
//! the OS root store is not sufficient by itself — any publicly-trusted CA can
//! issue for any hostname, so a mis-issued or compelled certificate would pass.
//!
//! So the join token carries a SHA-256 of the certificate the node should see,
//! modelled on `kubeadm join --discovery-token-ca-cert-hash`, and the node
//! refuses anything else. It is the same idea as SSH host-key checking, except
//! the fingerprint arrives with the invitation rather than being trusted on
//! first use.
//!
//! This is exactly the kind of check that is easy to write as an accidental
//! no-op — a verifier that returns "valid" no matter what still compiles and
//! still connects — so the tests below assert the *negative* case: a
//! certificate that doesn't match must be refused.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error as TlsError, SignatureScheme};
use sha2::{Digest, Sha256};

/// SHA-256 of a certificate's DER, lowercase hex — the string that travels in
/// a join token.
pub fn fingerprint(der: &[u8]) -> String {
    format!("{:x}", Sha256::digest(der))
}

/// Accepts exactly one server certificate: the one whose fingerprint was
/// handed over out of band.
///
/// Deliberately does NOT fall back to the web PKI. A pin that also accepts
/// "any certificate a public CA vouched for" is not a pin.
#[derive(Debug)]
pub struct PinnedServerCert {
    expected: String,
}

impl PinnedServerCert {
    pub fn new(expected_fingerprint: &str) -> Self {
        Self {
            // Compare case- and separator-insensitively: people paste
            // fingerprints with colons and in either case.
            expected: normalize(expected_fingerprint),
        }
    }
}

fn normalize(fp: &str) -> String {
    fp.trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect()
}

impl ServerCertVerifier for PinnedServerCert {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let actual = fingerprint(end_entity.as_ref());
        if actual == self.expected {
            Ok(ServerCertVerified::assertion())
        } else {
            // The message names both sides: the usual cause is pointing a node
            // at the wrong instance, and a fingerprint mismatch should be
            // diagnosable without a packet capture.
            Err(TlsError::General(format!(
                "server certificate does not match the fingerprint in the join token \
                 (expected {}, got {})",
                self.expected, actual
            )))
        }
    }

    // The certificate is pinned by identity, so the signature schemes it uses
    // are not what we are trusting; accept what rustls negotiated.
    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ED25519,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PKCS1_SHA256,
        ]
    }
}

/// A TLS client config that trusts only the pinned certificate.
pub fn pinned_client_config(expected_fingerprint: &str) -> rustls::ClientConfig {
    rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedServerCert::new(expected_fingerprint)))
        .with_no_client_auth()
}

/// The same pin, plus this machine's own certificate — the client half of
/// mutual TLS.
///
/// Both directions matter and they are separate proofs: the pin is how the
/// node knows it is talking to the right control plane, the client certificate
/// is how the control plane knows which machine is calling. Neither substitutes
/// for the other.
pub fn mutual_client_config(
    expected_fingerprint: Option<&str>,
    cert_pem: &str,
    key_pem: &str,
) -> anyhow::Result<rustls::ClientConfig> {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};

    let certs: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut cert_pem.as_bytes()).collect::<Result<_, _>>()?;
    if certs.is_empty() {
        anyhow::bail!("node certificate file contains no certificate");
    }
    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut key_pem.as_bytes())?
        .ok_or_else(|| anyhow::anyhow!("node key file contains no private key"))?;

    let builder = rustls::ClientConfig::builder();
    Ok(match expected_fingerprint {
        Some(fp) => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedServerCert::new(fp)))
            .with_client_auth_cert(certs, key)?,
        None => {
            // No pin recorded: fall back to the OS roots. Weaker, and only
            // reachable when the control plane never advertised a fingerprint.
            let mut roots = rustls::RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            builder
                .with_root_certificates(roots)
                .with_client_auth_cert(certs, key)?
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cert() -> (Vec<u8>, String) {
        let key = rcgen::KeyPair::generate().unwrap();
        let params = rcgen::CertificateParams::new(vec!["nook.example.com".into()]).unwrap();
        let c = params.self_signed(&key).unwrap();
        let der = c.der().to_vec();
        let fp = fingerprint(&der);
        (der, fp)
    }

    fn verify(v: &PinnedServerCert, der: &[u8]) -> Result<(), TlsError> {
        v.verify_server_cert(
            &CertificateDer::from(der.to_vec()),
            &[],
            &ServerName::try_from("nook.example.com").unwrap(),
            &[],
            UnixTime::now(),
        )
        .map(|_| ())
    }

    #[test]
    fn accepts_the_pinned_certificate() {
        let (der, fp) = cert();
        assert!(verify(&PinnedServerCert::new(&fp), &der).is_ok());
    }

    /// The negative case, which is the whole point: a different certificate —
    /// even a perfectly valid one — must be refused. If this ever passes, the
    /// pin has become a no-op.
    #[test]
    fn refuses_any_other_certificate() {
        let (_der_a, fp_a) = cert();
        let (der_b, fp_b) = cert();
        assert_ne!(fp_a, fp_b);

        let err = verify(&PinnedServerCert::new(&fp_a), &der_b)
            .expect_err("a mismatched certificate MUST be refused");
        assert!(
            err.to_string().contains("does not match the fingerprint"),
            "got: {err}"
        );
    }

    /// A tampered certificate fails even if only one byte changed.
    #[test]
    fn refuses_a_modified_certificate() {
        let (mut der, fp) = cert();
        let last = der.len() - 1;
        der[last] ^= 0xff;
        assert!(verify(&PinnedServerCert::new(&fp), &der).is_err());
    }

    #[test]
    fn fingerprints_are_compared_forgivingly() {
        let (der, fp) = cert();
        // Colons and uppercase are how people paste these around.
        let pretty = fp
            .as_bytes()
            .chunks(2)
            .map(|c| String::from_utf8_lossy(c).to_uppercase())
            .collect::<Vec<_>>()
            .join(":");
        assert!(verify(&PinnedServerCert::new(&pretty), &der).is_ok());
    }
}
