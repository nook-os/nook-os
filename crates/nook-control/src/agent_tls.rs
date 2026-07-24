//! TLS for the agent listener, terminating **here** rather than at the edge.
//!
//! This is the whole reason the agent has its own port. A reverse proxy that
//! terminated TLS would hold the client certificate and hand us plaintext, and
//! only the control plane can decide which tenant's CA a given certificate
//! should be judged against. So the handshake ends in this process, and the
//! peer certificate travels into the request for `AuthCtx` to verify.
//!
//! **Why the TLS layer accepts any client certificate.** rustls wants its
//! trust roots fixed when the server config is built, but trust here is
//! per-tenant and changes underneath us — a rotation stages a new CA at any
//! moment. A root set captured at startup would be stale exactly when it
//! mattered, and staging a CA would break enrolment until someone restarted
//! the process. So the handshake only proves possession of a private key, and
//! the authoritative check — chain, tenant, revocation — happens in
//! `ca::verify_node_cert`, which reads the bundle live from the database.
//! Completing a handshake authenticates nobody by itself, exactly as
//! presenting a bearer token does not.

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DistinguishedName, ServerConfig};
use tokio_rustls::TlsAcceptor;

/// The peer certificate, put into request extensions after the handshake.
///
/// A newtype rather than a bare Vec so nothing can pull "some bytes" out of
/// the extensions and mistake them for a verified identity.
#[derive(Debug, Clone)]
pub struct PeerCertificate(pub Vec<u8>);

/// Requests a client certificate, accepts whatever arrives, and defers the
/// real decision. See the module note — this is deliberate, not lax.
#[derive(Debug)]
struct DeferredClientAuth;

impl ClientCertVerifier for DeferredClientAuth {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }

    /// A node that has not enrolled yet has no certificate but still needs to
    /// reach `/nodes/enroll`. Refusing anonymous clients here would make
    /// bootstrapping impossible.
    fn client_auth_mandatory(&self) -> bool {
        false
    }

    fn offer_client_auth(&self) -> bool {
        true
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Build the acceptor from an operator-supplied certificate and key.
pub fn acceptor(cert_path: &str, key_path: &str) -> Result<TlsAcceptor> {
    let certs: Vec<CertificateDer<'static>> = {
        let pem = std::fs::read(cert_path)
            .with_context(|| format!("cannot read the agent TLS certificate at {cert_path}"))?;
        rustls_pemfile::certs(&mut pem.as_slice()).collect::<Result<_, _>>()?
    };
    if certs.is_empty() {
        anyhow::bail!("{cert_path} contains no certificate");
    }
    let key: PrivateKeyDer<'static> = {
        let pem = std::fs::read(key_path)
            .with_context(|| format!("cannot read the agent TLS key at {key_path}"))?;
        rustls_pemfile::private_key(&mut pem.as_slice())?
            .with_context(|| format!("{key_path} contains no private key"))?
    };

    let config = ServerConfig::builder()
        .with_client_cert_verifier(Arc::new(DeferredClientAuth))
        .with_single_cert(certs, key)
        .context("agent TLS certificate and key do not match")?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Serve the agent router over TLS, carrying each peer's certificate into the
/// request so `AuthCtx` can establish identity from it.
///
/// `shutdown` resolves on a termination signal: the accept loop then stops
/// taking new connections and returns, so a rolling update drains this door
/// alongside the browser one. Connections already accepted keep running as
/// detached tasks until the process exits within its grace period.
pub async fn serve(
    listener: tokio::net::TcpListener,
    router: Router,
    tls: TlsAcceptor,
    shutdown: impl std::future::Future<Output = ()> + Send,
) {
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use hyper_util::server::conn::auto::Builder;
    use tower::ServiceExt;

    tokio::pin!(shutdown);
    loop {
        let (stream, peer) = tokio::select! {
            _ = &mut shutdown => {
                tracing::info!("agent listener draining — no longer accepting connections");
                return;
            }
            accepted = listener.accept() => match accepted {
                Ok(conn) => conn,
                Err(_) => continue,
            },
        };
        let tls = tls.clone();
        let router = router.clone();

        tokio::spawn(async move {
            let stream = match tls.accept(stream).await {
                Ok(s) => s,
                Err(e) => {
                    // A failed handshake is routine (probes, wrong pin, a
                    // client with no TLS) — never fatal to the listener.
                    tracing::debug!(%peer, error = %e, "agent TLS handshake failed");
                    return;
                }
            };

            // Lift the peer certificate out of the completed handshake. This
            // is the only place it exists; everything downstream reads it from
            // the request.
            let peer_cert = stream
                .get_ref()
                .1
                .peer_certificates()
                .and_then(|c| c.first())
                .map(|c| PeerCertificate(c.as_ref().to_vec()));

            let svc = hyper::service::service_fn(
                move |mut req: hyper::Request<hyper::body::Incoming>| {
                    let router = router.clone();
                    let peer_cert = peer_cert.clone();
                    async move {
                        if let Some(cert) = peer_cert {
                            req.extensions_mut().insert(cert);
                        }
                        router.oneshot(req).await
                    }
                },
            );

            // `with_upgrades`: the agent connection is a WebSocket, and an
            // upgrade cannot complete without it.
            if let Err(e) = Builder::new(TokioExecutor::new())
                .serve_connection_with_upgrades(TokioIo::new(stream), svc)
                .await
            {
                tracing::debug!(%peer, error = %e, "agent connection ended");
            }
        });
    }
}
