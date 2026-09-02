//! Talking to a Kubernetes API server, for an executor (MAIN-339).
//!
//! Nothing in this tree could do that before: `kube` and `k8s-openapi` appeared
//! in no manifest, no code created or watched a Pod. MAIN-623 — running a loop
//! job as a Pod from an in-cluster agent — needs exactly one client, so this is
//! it, ahead of its consumer and deliberately narrow: credential resolution
//! ([`config`]), the five Pod verbs a job's lifecycle uses ([`pods`]), and
//! errors a caller can act on ([`error`]).
//!
//! **It is not in the control plane, and must not become so.** The control
//! plane places work; it does not reach into clusters, and a Kubernetes
//! permission held by the process that owns every tenant's database is a blast
//! radius nobody asked for. [`guards`] reads the manifests to keep that true —
//! see AC-4.
//!
//! Its own crate rather than a module in `nook-node` for the confinement AC-5
//! asks for: the desktop bundle builds the `nook` binary, so a dependency added
//! there ships `k8s-openapi` to every laptop install of an app with no cluster.
//! Here it costs one `[dependencies]` line, added by whoever needs it.
//!
//! Nothing here needs a cluster to be tested. Config resolution runs against a
//! fixture directory, every request shape against `tower_test::mock`, and every
//! failure against the `Status` body an apiserver would have sent. A live
//! cluster test belongs to MAIN-623, which has something to run.

pub mod config;
pub mod error;
pub mod pods;

/// The API types a caller needs to BUILD what [`pods`] sends.
///
/// A client that can create a Pod while exposing no way to construct one is
/// unusable, and the alternative -- its caller naming `k8s-openapi` itself --
/// is the second manifest AC-5 exists to prevent. So the types come through
/// here, and the kube dependency stays named in exactly one place.
///
/// Deliberately a re-export and not a wrapper: a hand-rolled Pod builder would
/// be a second, lossier spelling of a schema that already exists, and every
/// field it forgot would be one a caller could not set.
pub mod types {
    pub use k8s_openapi::api::core::v1::{
        Container, EnvFromSource, EnvVar, Pod, PodSecurityContext, PodSpec, SecretEnvSource,
        SecurityContext, Toleration,
    };
    pub use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
}

pub use config::{connect, resolve, Connection, Environment, Source, SERVICE_ACCOUNT_DIR};
pub use error::{Error, Operation, Result};
pub use pods::Pods;

#[cfg(test)]
mod guards;
