//! Writing ONE Secret: the fleet's runtime credential, delivered by the control
//! plane (MAIN-650).
//!
//! Deliberately not "the Secret operations a node needs" — there is one, and it
//! is an upsert. A node holds no general power over Secrets: the Role that
//! backs this names the single Secret by `resourceNames`, so `get` on anything
//! else is refused by the apiserver rather than by our restraint.
//!
//! Why the node writes it at all. The control plane runs the device
//! authorization and hands the node an opaque payload (MAIN-283); on a host node
//! that lands in a file. A Pod executor has no file a job will ever read — a job
//! is a Pod elsewhere in the cluster — so the same delivery has to land in the
//! Secret those Pods mount. The alternative was an operator running `kubectl
//! create secret` by hand from credentials they had to obtain some other way,
//! which is how a fleet ends up sharing somebody's personal login.
//!
//! It is not a privilege escalation in substance: the node is ALREADY handed
//! this credential by the delivery it is answering. Being allowed to persist it
//! where its own jobs can read it adds no secret it did not have.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::Secret;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::ByteString;
use kube::api::{Api, Patch, PatchParams, PostParams};
use kube::Client;

use crate::error::{Error, Result};

/// The one Secret this node may write, in one namespace.
pub struct Credentials {
    api: Api<Secret>,
    namespace: String,
    name: String,
}

impl Credentials {
    pub fn new(client: Client, namespace: &str, name: &str) -> Self {
        Self {
            api: Api::namespaced(client, namespace),
            namespace: namespace.to_string(),
            name: name.to_string(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Merge `entries` into the Secret, creating it if it is not there.
    ///
    /// A strategic merge patch rather than a replace, because the Secret is
    /// shared ground: an operator may have put `GH_TOKEN` beside the session by
    /// hand, and delivering a refreshed credential must not silently drop it.
    /// Keys named here win; keys absent here are left exactly as they were.
    ///
    /// Create-then-patch rather than patch-with-apply so the failure modes stay
    /// legible: a 409 on create means somebody else made it first, which is not
    /// an error — the patch that follows is the write that matters.
    pub async fn upsert(&self, entries: BTreeMap<String, Vec<u8>>) -> Result<()> {
        let data: BTreeMap<String, ByteString> = entries
            .into_iter()
            .map(|(k, v)| (k, ByteString(v)))
            .collect();

        if self
            .api
            .get_opt(&self.name)
            .await
            .map_err(|e| {
                Error::classify(
                    crate::error::Operation::new("get", "secrets", &self.namespace),
                    e,
                )
            })?
            .is_none()
        {
            let fresh = Secret {
                metadata: ObjectMeta {
                    name: Some(self.name.clone()),
                    namespace: Some(self.namespace.clone()),
                    ..Default::default()
                },
                data: Some(data.clone()),
                type_: Some("Opaque".to_string()),
                ..Default::default()
            };
            match self.api.create(&PostParams::default(), &fresh).await {
                Ok(_) => return Ok(()),
                // Lost the race. Fall through to the patch, which is what we
                // wanted anyway.
                Err(kube::Error::Api(s)) if s.code == 409 => {}
                Err(e) => {
                    return Err(Error::classify(
                        crate::error::Operation::new("create", "secrets", &self.namespace),
                        e,
                    ))
                }
            }
        }

        // A `Secret` as the patch body rather than a hand-built JSON object:
        // the type already serializes to exactly the shape the apiserver wants,
        // and it cannot drift from the schema the way a literal would.
        let patch = Secret {
            data: Some(data),
            ..Default::default()
        };
        self.api
            .patch(&self.name, &PatchParams::default(), &Patch::Merge(&patch))
            .await
            .map(|_| ())
            .map_err(|e| {
                Error::classify(
                    crate::error::Operation::new("patch", "secrets", &self.namespace),
                    e,
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use http::{Method, Request, Response};
    use kube::client::Body;
    use serde_json::Value;

    async fn body_json(request: Request<Body>) -> Value {
        use http_body_util::BodyExt;
        let bytes = request
            .into_body()
            .collect()
            .await
            .expect("a request body")
            .to_bytes();
        serde_json::from_slice(&bytes).expect("a JSON request body")
    }

    /// The write an existing Secret gets: a MERGE patch carrying only the keys
    /// we were given.
    ///
    /// Merge and not replace is the load-bearing part — an operator may have put
    /// `GH_TOKEN` beside the session by hand, and a refreshed credential must
    /// not silently drop it.
    #[tokio::test]
    async fn an_existing_secret_is_merged_not_replaced() {
        let (service, mut handle) = tower_test::mock::pair::<Request<Body>, Response<Body>>();
        let creds = Credentials::new(
            Client::new(service, "nook-jobs"),
            "nook-jobs",
            "nook-job-credentials",
        );

        let server = tokio::spawn(async move {
            // 1. the existence check
            let (get, send) = handle.next_request().await.expect("a get");
            send.send_response(
                Response::builder()
                    .status(200)
                    .body(Body::from(
                        serde_json::json!({
                            "apiVersion": "v1", "kind": "Secret",
                            "metadata": {"name": "nook-job-credentials"},
                            "data": {"GH_TOKEN": "aGk="}
                        })
                        .to_string()
                        .into_bytes(),
                    ))
                    .unwrap(),
            );
            // 2. the write
            let (patch, send) = handle.next_request().await.expect("a patch");
            send.send_response(
                Response::builder()
                    .status(200)
                    .body(Body::from(
                        serde_json::json!({
                            "apiVersion": "v1", "kind": "Secret",
                            "metadata": {"name": "nook-job-credentials"}
                        })
                        .to_string()
                        .into_bytes(),
                    ))
                    .unwrap(),
            );
            (get, patch)
        });

        creds
            .upsert(BTreeMap::from([(
                ".credentials.json".to_string(),
                b"{\"claudeAiOauth\":{}}".to_vec(),
            )]))
            .await
            .expect("the upsert");

        let (get, patch) = server.await.expect("mock apiserver");
        assert_eq!(get.method(), Method::GET);
        assert!(
            get.uri().path().ends_with("/secrets/nook-job-credentials"),
            "the existence check names ONE Secret: {}",
            get.uri()
        );
        assert_eq!(patch.method(), Method::PATCH);
        assert_eq!(
            patch
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("application/merge-patch+json"),
            "a merge patch, so keys we were not given survive"
        );

        let body = body_json(patch).await;
        let data = body.get("data").expect("a data object");
        assert_eq!(
            data.get(".credentials.json").and_then(Value::as_str),
            Some("eyJjbGF1ZGVBaU9hdXRoIjp7fX0="),
            "the payload is base64 under the file name the runtime looks for"
        );
        assert!(
            data.get("GH_TOKEN").is_none(),
            "a merge patch names only what changed: {data}"
        );
    }
}
