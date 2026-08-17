//! What a Kubernetes API call can fail with, told apart by what the caller
//! would do about it (MAIN-339 AC-3).
//!
//! The apiserver says almost everything through one HTTP status and a `Status`
//! body, so `403` alone is not an answer: a missing RoleBinding and a
//! `ResourceQuota` rejection arrive with the same code and mean opposite things
//! — the first is a deployment the operator must fix, the second is a shortage
//! that clears itself. A caller placing loop jobs has to distinguish them (it
//! refuses the job in one case and reports a broken node in the other), and the
//! only place that reading belongs is here, once.
//!
//! Nothing in this crate panics on a failure it can be handed. Every fallible
//! step returns one of these, and `guards::no_panicking_shortcuts_outside_tests`
//! reads the source to keep it that way.

use std::path::PathBuf;

/// What the client was doing. Carried into the error so its message names the
/// call without the caller reconstructing it from context that is already gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Operation {
    pub verb: &'static str,
    pub resource: &'static str,
    pub namespace: String,
    pub name: Option<String>,
}

impl Operation {
    pub fn new(verb: &'static str, resource: &'static str, namespace: &str) -> Self {
        Self {
            verb,
            resource,
            namespace: namespace.to_string(),
            name: None,
        }
    }

    pub fn named(verb: &'static str, resource: &'static str, namespace: &str, name: &str) -> Self {
        Self {
            name: Some(name.to_string()),
            ..Self::new(verb, resource, namespace)
        }
    }
}

impl std::fmt::Display for Operation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.name {
            Some(name) => write!(
                f,
                "{} {}/{name} in namespace {}",
                self.verb, self.resource, self.namespace
            ),
            None => write!(
                f,
                "{} {} in namespace {}",
                self.verb, self.resource, self.namespace
            ),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// [`Operation`] is BOXED into every variant that carries it. This is the `Err`
/// half of every call in this crate, and an enum sized by its largest variant is
/// a cost paid on the success path too — clippy's `result_large_err` is right
/// about that.

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The ServiceAccount this Pod runs as may not do it. A deployment fault:
    /// the Role or RoleBinding is missing or too narrow, and no amount of
    /// retrying changes it.
    #[error("not permitted to {operation}: {message} — the ServiceAccount's Role is missing this permission")]
    Forbidden {
        operation: Box<Operation>,
        message: String,
    },

    /// A `ResourceQuota` refused the object. Also a `403`, and deliberately not
    /// [`Error::Forbidden`]: the cluster is full, not misconfigured, so the
    /// caller waits rather than reporting a broken node.
    #[error("quota refused {operation}: {message}")]
    QuotaExceeded {
        operation: Box<Operation>,
        message: String,
    },

    /// The namespace does not exist. A `404`, like a missing Pod, and told
    /// apart from one by the `Status` detail the apiserver attaches.
    #[error("namespace {namespace} does not exist (while trying to {operation})")]
    NamespaceMissing {
        namespace: String,
        operation: Box<Operation>,
    },

    /// The object does not exist.
    #[error("no such {resource}/{name} in namespace {namespace}")]
    NotFound {
        resource: &'static str,
        name: String,
        namespace: String,
    },

    /// The request never reached an apiserver: no route, refused connection,
    /// TLS refused, timed out.
    #[error("cannot reach the Kubernetes API server (while trying to {operation}): {message}")]
    Unreachable {
        operation: Box<Operation>,
        message: String,
    },

    /// The apiserver answered, with something none of the above covers. Kept as
    /// a variant rather than collapsed into a string so a caller can still read
    /// the code — a `409` on create is an ordinary race, a `500` is not.
    #[error("Kubernetes API refused {operation} with {code} {reason}: {message}")]
    Api {
        operation: Box<Operation>,
        code: u16,
        reason: String,
        message: String,
    },

    /// `KUBERNETES_SERVICE_HOST` and `_PORT` are set, so this process is in a
    /// Pod, but the projected token is not there. A Pod with
    /// `automountServiceAccountToken: false`, or a volume that failed to mount.
    #[error("in a cluster (KUBERNETES_SERVICE_HOST is set) but no ServiceAccount token at {0}")]
    MissingServiceAccountToken(PathBuf),

    /// Neither source produced credentials: not in a cluster, and no kubeconfig
    /// at `KUBECONFIG` or `~/.kube/config`.
    #[error("no Kubernetes credentials: not running in a cluster, and no kubeconfig at KUBECONFIG or ~/.kube/config")]
    NoCredentials,

    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{path} is not a usable PEM certificate bundle")]
    MalformedCertificate { path: PathBuf },

    #[error("{0} is not a Kubernetes API server address: {1}")]
    MalformedClusterUrl(String, String),

    /// A kubeconfig that exists but cannot be used — unparseable, no
    /// current-context, an exec plugin that is not installed.
    #[error("unusable kubeconfig: {0}")]
    Kubeconfig(String),

    /// Building a client from a resolved configuration failed: a TLS stack that
    /// will not initialise, an auth plugin that cannot run.
    #[error("cannot build a Kubernetes client: {0}")]
    Client(String),
}

impl Error {
    /// Read a `kube::Error` for what the caller has to decide.
    ///
    /// The `Status` body is the only thing that separates the two `403`s and the
    /// two `404`s, which is why this takes the whole error and not just a code.
    pub fn classify(operation: Operation, err: kube::Error) -> Self {
        let operation = Box::new(operation);
        match err {
            kube::Error::Api(status) => {
                let message = status.message.clone();
                let detail_kind = status
                    .details
                    .as_ref()
                    .map(|d| d.kind.as_str())
                    .unwrap_or_default()
                    .to_string();
                match status.code {
                    // A quota rejection is an admission webhook's `403`, and it
                    // says so only in prose. `client-go` reads it the same way;
                    // there is no machine-readable reason for it.
                    403 if message.contains("exceeded quota") => {
                        Error::QuotaExceeded { operation, message }
                    }
                    403 => Error::Forbidden { operation, message },
                    404 if detail_kind == "namespaces" => Error::NamespaceMissing {
                        namespace: operation.namespace.clone(),
                        operation,
                    },
                    404 => Error::NotFound {
                        resource: operation.resource,
                        name: operation.name.clone().unwrap_or_default(),
                        namespace: operation.namespace.clone(),
                    },
                    code => Error::Api {
                        operation,
                        code,
                        reason: status.reason.clone(),
                        message,
                    },
                }
            }
            // Nothing answered. `Service` covers the connector's own failures —
            // DNS, refused, TLS — and `HyperError` a connection that died
            // mid-exchange; to a caller they are the same fact.
            kube::Error::Service(e) => Error::Unreachable {
                operation,
                message: e.to_string(),
            },
            kube::Error::HyperError(e) => Error::Unreachable {
                operation,
                message: e.to_string(),
            },
            other => Error::Client(other.to_string()),
        }
    }
}
