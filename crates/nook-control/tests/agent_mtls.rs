//! The handshake itself: a real TLS listener, a real client certificate, and
//! the identity that comes out the other side.
//!
//! Everything else in the mTLS work is verifiable in isolation. This is the
//! part that only means something end to end — that the certificate presented
//! during a handshake actually reaches the request, and that the control plane
//! turns it into the right node.

use std::sync::Arc;

use nook_control::agent_tls;
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
        .bind(format!("mtls-{}", Uuid::now_v7().simple()))
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

/// Issue a client certificate the way enrolment does.
async fn issue_client_cert(
    pool: &PgPool,
    tenant: TenantId,
    node: Uuid,
) -> (String, rcgen::KeyPair) {
    use rcgen::{CertificateParams, DistinguishedName, DnType};
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "whatever");
    params.distinguished_name = dn;
    let csr = params.serialize_request(&key).unwrap().pem().unwrap();

    let leaf = ca::sign_node_csr(pool, &vault(), tenant, node, &csr)
        .await
        .unwrap();
    (leaf.cert_pem, key)
}

/// A self-signed server certificate for the listener, and its fingerprint —
/// the value a join token would carry.
fn server_cert() -> (String, String, String) {
    let key = rcgen::KeyPair::generate().unwrap();
    let params = rcgen::CertificateParams::new(vec!["localhost".into()]).unwrap();
    let cert = params.self_signed(&key).unwrap();
    let fp = format!(
        "{:x}",
        <sha2::Sha256 as sha2::Digest>::digest(cert.der().as_ref())
    );
    (cert.pem(), key.serialize_pem(), fp)
}

/// The whole point: a node connects over TLS presenting its certificate, and
/// the control plane recovers exactly which machine it is.
#[tokio::test]
async fn a_client_certificate_survives_the_handshake_and_identifies_the_node() {
    let Some(pool) = test_pool().await else {
        return;
    };
    // rustls needs a process-wide crypto provider; ignore a second install.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let tenant = seed_tenant(&pool).await;
    ca::generate(&pool, &vault(), tenant, true).await.unwrap();
    let node = seed_node(&pool, tenant).await;
    let (client_cert, client_key) = issue_client_cert(&pool, tenant, node).await;

    // Stand up the real acceptor with a real certificate on disk.
    let dir = std::env::temp_dir().join(format!("nook-mtls-{}", Uuid::now_v7().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let (srv_cert, srv_key, srv_fp) = server_cert();
    let cert_path = dir.join("server.crt");
    let key_path = dir.join("server.key");
    std::fs::write(&cert_path, &srv_cert).unwrap();
    std::fs::write(&key_path, &srv_key).unwrap();

    let acceptor = agent_tls::acceptor(cert_path.to_str().unwrap(), key_path.to_str().unwrap())
        .expect("acceptor");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // Accept one connection and pull the peer certificate out of the finished
    // handshake, exactly as agent_tls::serve does.
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let tls = acceptor.accept(stream).await.expect("handshake");
        tls.get_ref()
            .1
            .peer_certificates()
            .and_then(|c| c.first())
            .map(|c| c.as_ref().to_vec())
    });

    // The node dials with the pin AND its own certificate.
    let cfg = {
        use rustls::pki_types::{CertificateDer, PrivateKeyDer};
        let certs: Vec<CertificateDer<'static>> =
            rustls_pemfile::certs(&mut client_cert.as_bytes())
                .collect::<Result<_, _>>()
                .unwrap();
        let key: PrivateKeyDer<'static> =
            rustls_pemfile::private_key(&mut client_key.serialize_pem().as_bytes())
                .unwrap()
                .unwrap();
        rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedFor(srv_fp.clone())))
            .with_client_auth_cert(certs, key)
            .unwrap()
    };

    let connector = tokio_rustls::TlsConnector::from(Arc::new(cfg));
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let _client = connector
        .connect(
            rustls::pki_types::ServerName::try_from("localhost").unwrap(),
            stream,
        )
        .await
        .expect("client handshake");

    let presented = server.await.unwrap().expect("server saw a client cert");

    // And that certificate resolves to the node we issued it to.
    let id = ca::verify_node_cert(&pool, &presented).await.unwrap();
    assert_eq!(id.node_id, node);
    assert_eq!(id.tenant_id, tenant.0);

    let _ = std::fs::remove_dir_all(&dir);
    let _ = sqlx::query("DELETE FROM tenants WHERE id = $1")
        .bind(tenant)
        .execute(&pool)
        .await;
}

/// A cert-less client still completes the handshake — a machine that has not
/// enrolled has to be able to reach /nodes/enroll. It simply has no identity.
#[tokio::test]
async fn an_anonymous_client_is_allowed_but_carries_no_identity() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let dir = std::env::temp_dir().join(format!("nook-anon-{}", Uuid::now_v7().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let (srv_cert, srv_key, srv_fp) = server_cert();
    let cert_path = dir.join("server.crt");
    let key_path = dir.join("server.key");
    std::fs::write(&cert_path, &srv_cert).unwrap();
    std::fs::write(&key_path, &srv_key).unwrap();

    let acceptor =
        agent_tls::acceptor(cert_path.to_str().unwrap(), key_path.to_str().unwrap()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let tls = acceptor.accept(stream).await.expect("handshake");
        tls.get_ref().1.peer_certificates().map(|c| c.len())
    });

    let cfg = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedFor(srv_fp)))
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(cfg));
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    connector
        .connect(
            rustls::pki_types::ServerName::try_from("localhost").unwrap(),
            stream,
        )
        .await
        .expect("anonymous client must still connect");

    assert!(
        server.await.unwrap().is_none(),
        "an anonymous client presents no certificate, so there is no identity"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Minimal pinning verifier, mirroring the node's.
#[derive(Debug)]
struct PinnedFor(String);

impl rustls::client::danger::ServerCertVerifier for PinnedFor {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _i: &[rustls::pki_types::CertificateDer<'_>],
        _n: &rustls::pki_types::ServerName<'_>,
        _o: &[u8],
        _t: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let actual = format!(
            "{:x}",
            <sha2::Sha256 as sha2::Digest>::digest(end_entity.as_ref())
        );
        if actual == self.0 {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General("pin mismatch".into()))
        }
    }
    fn verify_tls12_signature(
        &self,
        _m: &[u8],
        _c: &rustls::pki_types::CertificateDer<'_>,
        _d: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _m: &[u8],
        _c: &rustls::pki_types::CertificateDer<'_>,
        _d: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
