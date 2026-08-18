//! The Pod operations an executor needs, and only those (MAIN-339 AC-2).
//!
//! Five: create one, look one up, list the ones this executor owns, delete one,
//! and follow its output. That is the whole lifecycle of a loop job as a Pod —
//! and the list is deliberately closed, because every verb here has to appear
//! in the Role MAIN-623 grants. A convenience method nobody calls is a
//! permission somebody has to justify.
//!
//! Errors come back through [`Error::classify`], so a caller reads `Forbidden`
//! and `QuotaExceeded` rather than two `403`s (AC-3).

use futures_util::io::AsyncBufRead;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, DeleteParams, ListParams, LogParams, PostParams};
use kube::Client;

use crate::error::{Error, Operation, Result};

const PODS: &str = "pods";

/// Pods in ONE namespace. Namespaced, not cluster-wide, because that is the
/// scope the executor's Role is bound at (NG-4: not multi-cluster, and not
/// cluster-wide either).
pub struct Pods {
    api: Api<Pod>,
    namespace: String,
}

impl Pods {
    pub fn new(client: Client, namespace: &str) -> Self {
        Self {
            api: Api::namespaced(client, namespace),
            namespace: namespace.to_string(),
        }
    }

    pub async fn create(&self, pod: &Pod) -> Result<Pod> {
        let name = pod.metadata.name.clone().unwrap_or_default();
        self.api
            .create(&PostParams::default(), pod)
            .await
            .map_err(|e| Error::classify(self.op("create", &name), e))
    }

    pub async fn get(&self, name: &str) -> Result<Pod> {
        self.api
            .get(name)
            .await
            .map_err(|e| Error::classify(self.op("get", name), e))
    }

    /// Every Pod matching a label selector — how an executor finds the Pods it
    /// owns after a restart, since its own memory of them is gone.
    pub async fn list_labelled(&self, selector: &str) -> Result<Vec<Pod>> {
        let list = self
            .api
            .list(&ListParams::default().labels(selector))
            .await
            .map_err(|e| Error::classify(Operation::new("list", PODS, &self.namespace), e))?;
        Ok(list.items)
    }

    /// Delete a Pod, treating an already-absent one as success.
    ///
    /// The caller deletes on every exit path — success, failure, cancel — and
    /// more than one of those can run for a single Pod. A second delete that
    /// errored would turn tidy-up into a reported failure.
    pub async fn delete(&self, name: &str) -> Result<()> {
        match self.api.delete(name, &DeleteParams::default()).await {
            Ok(_) => Ok(()),
            Err(e) => match Error::classify(self.op("delete", name), e) {
                Error::NotFound { .. } => Ok(()),
                other => Err(other),
            },
        }
    }

    /// Follow a Pod's output from the beginning, as lines arrive.
    ///
    /// `follow`, because the transcript is streamed to the card while the job
    /// runs; a one-shot fetch would deliver it only once the Pod had exited.
    pub async fn follow_logs(&self, name: &str) -> Result<impl AsyncBufRead> {
        self.api
            .log_stream(
                name,
                &LogParams {
                    follow: true,
                    ..LogParams::default()
                },
            )
            .await
            .map_err(|e| Error::classify(self.op("logs", name), e))
    }

    fn op(&self, verb: &'static str, name: &str) -> Operation {
        Operation::named(verb, PODS, &self.namespace, name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use futures_util::io::AsyncBufReadExt;
    use futures_util::TryStreamExt;
    use http::{Method, Request, Response};
    use kube::client::Body;
    use serde_json::{json, Value};

    /// One request, answered by `respond`, with the request handed back for
    /// assertion. `tower_test::mock` is the harness kube's own suite uses.
    async fn one_request<T, F>(
        call: impl FnOnce(Pods) -> F,
        respond: Response<Body>,
    ) -> (Request<Body>, Result<T>)
    where
        F: std::future::Future<Output = Result<T>>,
    {
        let (service, mut handle) = tower_test::mock::pair::<Request<Body>, Response<Body>>();
        let pods = Pods::new(Client::new(service, "nook-jobs"), "nook-jobs");
        let server = tokio::spawn(async move {
            let (request, send) = handle.next_request().await.expect("the client called out");
            send.send_response(respond);
            request
        });
        let outcome = call(pods).await;
        (server.await.expect("mock apiserver"), outcome)
    }

    async fn body_json(request: Request<Body>) -> Value {
        let bytes = http_body_bytes(request.into_body()).await;
        serde_json::from_slice(&bytes).expect("a JSON request body")
    }

    async fn http_body_bytes(body: Body) -> Vec<u8> {
        use http_body_util::BodyExt;
        body.collect().await.expect("body").to_bytes().to_vec()
    }

    fn pod(name: &str) -> Pod {
        serde_json::from_value(json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": { "name": name, "labels": { "nook.job": "job-1" } },
            "spec": { "containers": [{ "name": "agent", "image": "nook-job-sandbox:latest" }] },
        }))
        .expect("a Pod")
    }

    fn ok(value: Value) -> Response<Body> {
        Response::new(Body::from(serde_json::to_vec(&value).unwrap()))
    }

    /// AC-2: create is a POST to the namespaced collection, carrying the Pod.
    #[tokio::test]
    async fn create_posts_the_pod_to_its_namespace() {
        let (request, created) = one_request(
            |pods| async move { pods.create(&pod("job-1")).await },
            ok(serde_json::to_value(pod("job-1")).unwrap()),
        )
        .await;
        assert_eq!(request.method(), Method::POST);
        assert_eq!(
            request.uri().path(),
            "/api/v1/namespaces/nook-jobs/pods",
            "create did not address the namespaced collection"
        );
        let body = body_json(request).await;
        assert_eq!(body["metadata"]["name"], "job-1");
        assert_eq!(body["metadata"]["labels"]["nook.job"], "job-1");
        assert_eq!(
            body["spec"]["containers"][0]["image"],
            "nook-job-sandbox:latest"
        );
        assert_eq!(
            created.expect("created").metadata.name.as_deref(),
            Some("job-1")
        );
    }

    /// AC-2: get is a GET of the named object.
    #[tokio::test]
    async fn get_addresses_the_named_pod() {
        let (request, got) = one_request(
            |pods| async move { pods.get("job-1").await },
            ok(serde_json::to_value(pod("job-1")).unwrap()),
        )
        .await;
        assert_eq!(request.method(), Method::GET);
        assert_eq!(
            request.uri().path(),
            "/api/v1/namespaces/nook-jobs/pods/job-1"
        );
        assert_eq!(got.expect("got").metadata.name.as_deref(), Some("job-1"));
    }

    /// AC-2: the selector reaches the apiserver as `labelSelector`, so the
    /// filtering happens there and not over every Pod in the namespace.
    #[tokio::test]
    async fn list_sends_the_label_selector_to_the_apiserver() {
        let (request, listed) = one_request(
            |pods| async move { pods.list_labelled("nook.job").await },
            ok(json!({
                "apiVersion": "v1",
                "kind": "PodList",
                "metadata": {},
                "items": [pod("job-1"), pod("job-2")],
            })),
        )
        .await;
        assert_eq!(request.method(), Method::GET);
        assert_eq!(request.uri().path(), "/api/v1/namespaces/nook-jobs/pods");
        assert!(
            request
                .uri()
                .query()
                .unwrap_or_default()
                .contains("labelSelector=nook.job"),
            "the selector was not sent: {:?}",
            request.uri().query()
        );
        assert_eq!(listed.expect("listed").len(), 2);
    }

    /// AC-2: delete is a DELETE of the named object.
    #[tokio::test]
    async fn delete_addresses_the_named_pod() {
        let (request, deleted) = one_request(
            |pods| async move { pods.delete("job-1").await },
            ok(serde_json::to_value(pod("job-1")).unwrap()),
        )
        .await;
        assert_eq!(request.method(), Method::DELETE);
        assert_eq!(
            request.uri().path(),
            "/api/v1/namespaces/nook-jobs/pods/job-1"
        );
        deleted.expect("deleted");
    }

    /// Deleting on every exit path means deleting twice; the second must not be
    /// a failure.
    #[tokio::test]
    async fn deleting_a_pod_that_is_already_gone_succeeds() {
        let (_, deleted) = one_request(
            |pods| async move { pods.delete("job-1").await },
            status(404, "NotFound", "pods \"job-1\" not found", Some("pods")),
        )
        .await;
        deleted.expect("an absent Pod is already deleted");
    }

    /// AC-2: logs is a GET of the `log` subresource with `follow`, and the
    /// bytes arrive as a stream.
    #[tokio::test]
    async fn logs_follow_the_pods_output() {
        let (request, stream) = one_request(
            |pods| async move { pods.follow_logs("job-1").await.map(Box::pin) },
            Response::new(Body::from(b"first line\nsecond line\n".to_vec())),
        )
        .await;
        assert_eq!(request.method(), Method::GET);
        assert_eq!(
            request.uri().path(),
            "/api/v1/namespaces/nook-jobs/pods/job-1/log"
        );
        assert!(
            request
                .uri()
                .query()
                .unwrap_or_default()
                .contains("follow=true"),
            "the log request did not follow: {:?}",
            request.uri().query()
        );
        let lines: Vec<String> = stream
            .expect("a log stream")
            .lines()
            .try_collect()
            .await
            .expect("lines");
        assert_eq!(lines, vec!["first line", "second line"]);
    }

    // ── AC-3: one API failure, one variant ──────────────────────────────────

    fn status(code: u16, reason: &str, message: &str, kind: Option<&str>) -> Response<Body> {
        let mut body = json!({
            "apiVersion": "v1",
            "kind": "Status",
            "status": "Failure",
            "code": code,
            "reason": reason,
            "message": message,
        });
        if let Some(kind) = kind {
            body["details"] = json!({ "kind": kind });
        }
        Response::builder()
            .status(code)
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .expect("a Status response")
    }

    #[tokio::test]
    async fn a_missing_role_is_forbidden() {
        let (_, created) = one_request(
            |pods| async move { pods.create(&pod("job-1")).await },
            status(
                403,
                "Forbidden",
                "pods is forbidden: User \"system:serviceaccount:nook-jobs:executor\" \
                 cannot create resource \"pods\" in API group \"\" in the namespace \"nook-jobs\"",
                Some("pods"),
            ),
        )
        .await;
        let err = created.expect_err("forbidden");
        assert!(matches!(err, Error::Forbidden { .. }), "{err}");
        assert!(
            err.to_string()
                .contains("create pods/job-1 in namespace nook-jobs"),
            "the error does not name the call: {err}"
        );
    }

    /// The same `403`, and NOT the same answer: a full cluster is a shortage to
    /// wait out, a missing Role is a deployment to fix.
    #[tokio::test]
    async fn a_quota_rejection_is_not_a_permission_problem() {
        let (_, created) = one_request(
            |pods| async move { pods.create(&pod("job-1")).await },
            status(
                403,
                "Forbidden",
                "pods \"job-1\" is forbidden: exceeded quota: compute-resources, \
                 requested: pods=1, used: pods=8, limited: pods=8",
                Some("pods"),
            ),
        )
        .await;
        let err = created.expect_err("quota");
        assert!(
            matches!(err, Error::QuotaExceeded { .. }),
            "a quota rejection was read as a permission problem: {err}"
        );
    }

    /// And the two `404`s, told apart by the detail the apiserver attaches.
    #[tokio::test]
    async fn a_missing_namespace_is_not_a_missing_pod() {
        let (_, created) = one_request(
            |pods| async move { pods.create(&pod("job-1")).await },
            status(
                404,
                "NotFound",
                "namespaces \"nook-jobs\" not found",
                Some("namespaces"),
            ),
        )
        .await;
        let err = created.expect_err("no namespace");
        assert!(
            matches!(err, Error::NamespaceMissing { .. }),
            "a missing namespace was read as a missing Pod: {err}"
        );
    }

    #[tokio::test]
    async fn a_missing_pod_is_not_found() {
        let (_, got) = one_request(
            |pods| async move { pods.get("job-1").await },
            status(404, "NotFound", "pods \"job-1\" not found", Some("pods")),
        )
        .await;
        let err = got.expect_err("no pod");
        assert!(matches!(err, Error::NotFound { .. }), "{err}");
    }

    /// An apiserver nothing can reach: the mock drops the request instead of
    /// answering it, which is what a refused connection looks like from here.
    #[tokio::test]
    async fn an_unreachable_apiserver_is_distinguishable() {
        let (service, handle) = tower_test::mock::pair::<Request<Body>, Response<Body>>();
        let pods = Pods::new(Client::new(service, "nook-jobs"), "nook-jobs");
        drop(handle);
        let err = pods.get("job-1").await.expect_err("unreachable");
        assert!(
            matches!(err, Error::Unreachable { .. }),
            "an apiserver that never answered was not reported as unreachable: {err}"
        );
    }

    /// Everything else keeps its code, so a caller can still tell a race from a
    /// broken apiserver.
    #[tokio::test]
    async fn any_other_refusal_keeps_its_status_code() {
        let (_, created) = one_request(
            |pods| async move { pods.create(&pod("job-1")).await },
            status(
                409,
                "AlreadyExists",
                "pods \"job-1\" already exists",
                Some("pods"),
            ),
        )
        .await;
        match created.expect_err("conflict") {
            Error::Api { code, reason, .. } => {
                assert_eq!(code, 409);
                assert_eq!(reason, "AlreadyExists");
            }
            other => panic!("a 409 lost its code: {other}"),
        }
    }
}
