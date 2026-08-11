//! Where distributed binaries live.
//!
//! The node agent has to reach every machine in a fleet, which makes "how do
//! we hand out the binary" an operations question rather than a code one: a
//! home lab wants a directory, a hosted install wants object storage, and a
//! Mac build gets uploaded from a laptop after the Linux CI has already run.
//! Baking artifacts into the container image answers none of those — the image
//! can only ever contain what its build host could compile, and shipping a new
//! macOS binary would mean rebuilding and redeploying the server.
//!
//! So artifacts live behind this trait. Two backends today:
//!
//! - **disk** — a directory. The default, and what the container image still
//!   populates for the platform it was built on.
//! - **s3** — anything speaking the S3 API: AWS, MinIO, or Google Cloud
//!   Storage through its S3-compatible endpoint.
//!
//! Keys are `<prefix>/<version>/<artifact>`, so several versions coexist and a
//! node can pin one. `latest` is not a key — it's whichever version the caller
//! asks for, defaulting to the control plane's own, because a server handing
//! out an agent it wasn't built alongside is how fleets drift.
//!
//! Downloads stream through the control plane by default rather than
//! redirecting to the store. That is slower, and it is the right default: the
//! object store is commonly on a private network where the machine running the
//! install script cannot reach it, and a presigned URL to an unreachable host
//! fails in a way that looks like the installer is broken. `artifact_redirect`
//! turns redirection on where the store is genuinely public.
//!
//! Since MAIN-532 the same trait also backs **user content** — what a person
//! uploads — under a prefix of its own (`user_content_key`). One trait, because
//! the question is identical: where do bytes live, and does the caller reach
//! the store directly. What differs is policy, and policy lives with each
//! caller: its own prefix, its own redirect switch, its own size cap.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

pub mod disk;
pub mod s3;

/// One stored artifact. `sha256` is optional because computing it means
/// reading the whole object, which a listing shouldn't do for a remote store.
#[derive(Debug, Clone)]
pub struct ObjectMeta {
    pub key: String,
    pub size: u64,
    pub sha256: Option<String>,
}

#[async_trait]
pub trait ArtifactStore: Send + Sync {
    /// Objects under a key prefix. Returns an empty list rather than an error
    /// when nothing matches — "no artifacts yet" is a normal state for a fresh
    /// instance, not a failure.
    async fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>>;

    async fn get(&self, key: &str) -> Result<Vec<u8>>;

    async fn put(&self, key: &str, bytes: Vec<u8>) -> Result<()>;

    async fn head(&self, key: &str) -> Result<Option<ObjectMeta>>;

    /// Remove an object. Deleting what is not there succeeds: a caller whose
    /// row and object have already diverged wants the row gone either way, and
    /// a delete that fails on the second attempt would strand it.
    async fn delete(&self, key: &str) -> Result<()>;

    /// A time-limited URL the caller can fetch directly, when the backend can
    /// mint one. `None` means "stream it through the control plane instead" —
    /// which is always correct, just less efficient, and is what a disk store
    /// always answers.
    async fn presign(&self, _key: &str, _ttl: Duration) -> Result<Option<String>> {
        Ok(None)
    }

    /// For logs and the health page: which backend, pointed where.
    fn describe(&self) -> String;
}

/// Build the store this instance is configured for.
///
/// Falls back to disk rather than failing to boot: a control plane that won't
/// start because object storage is misconfigured is worse than one that starts
/// and can't hand out binaries, since the second one can still be fixed from
/// its own UI.
pub async fn from_config(cfg: &crate::config::Config) -> Box<dyn ArtifactStore> {
    match cfg.artifact_store.as_str() {
        "s3" => match s3::S3Store::from_config(cfg).await {
            Ok(store) => {
                tracing::info!(store = %store.describe(), "artifact storage");
                Box::new(store)
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "S3 artifact storage is configured but unusable — falling back to disk; \
                     node downloads will only offer what is on this machine"
                );
                Box::new(disk::DiskStore::new(&cfg.dist_dir))
            }
        },
        _ => {
            let store = disk::DiskStore::new(&cfg.dist_dir);
            tracing::info!(store = %store.describe(), "artifact storage");
            Box::new(store)
        }
    }
}

/// `<prefix>/<version>/<name>` — the layout every backend shares.
pub fn artifact_key(prefix: &str, version: &str, name: &str) -> String {
    let prefix = prefix.trim_matches('/');
    if prefix.is_empty() {
        format!("{version}/{name}")
    } else {
        format!("{prefix}/{version}/{name}")
    }
}

/// `<prefix>/<tenant>/<id>` — where a person's upload lives (MAIN-532).
///
/// Tenant-scoped so one tenant's objects are a subtree rather than mixed
/// through the bucket, and under a prefix of its own so user content can never
/// be mistaken for — or collide with — a distributed binary, which is
/// `<artifact_prefix>/<version>/<name>`. The id, not the filename, is the last
/// segment: two people uploading `report.pdf` must not overwrite each other,
/// and a filename off the wire is not a path component we want to trust.
pub fn user_content_key(prefix: &str, tenant: &str, id: &str) -> String {
    let prefix = prefix.trim_matches('/');
    if prefix.is_empty() {
        format!("{tenant}/{id}")
    } else {
        format!("{prefix}/{tenant}/{id}")
    }
}

/// The version segment of a key, for turning a listing back into versions.
pub fn version_from_key(prefix: &str, key: &str) -> Option<String> {
    let rest = match prefix.trim_matches('/') {
        "" => key,
        p => key.strip_prefix(p)?.trim_start_matches('/'),
    };
    let (version, remainder) = rest.split_once('/')?;
    // Exactly one segment below the version — anything deeper isn't ours.
    (!version.is_empty() && !remainder.is_empty() && !remainder.contains('/'))
        .then(|| version.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip() {
        let k = artifact_key("nook", "0.1.0", "nook-linux-x86_64");
        assert_eq!(k, "nook/0.1.0/nook-linux-x86_64");
        assert_eq!(version_from_key("nook", &k).as_deref(), Some("0.1.0"));
    }

    #[test]
    fn empty_prefix_is_allowed() {
        let k = artifact_key("", "0.2.0", "nook-darwin-aarch64");
        assert_eq!(k, "0.2.0/nook-darwin-aarch64");
        assert_eq!(version_from_key("", &k).as_deref(), Some("0.2.0"));
    }

    /// MAIN-532 AC-10: user content and node binaries share a store and must
    /// never share a key. They are distinguished by the top-level prefix, not
    /// by luck about what a version or a filename happens to be.
    #[test]
    fn user_content_never_collides_with_a_binary() {
        let content = user_content_key(
            "nook/user-content",
            "0198ffff-0000-7000-8000-000000000001",
            "0198ffff-0000-7000-8000-000000000002",
        );
        assert_eq!(
            content,
            "nook/user-content/0198ffff-0000-7000-8000-000000000001/0198ffff-0000-7000-8000-000000000002"
        );
        // Not readable as an artifact: the "version" segment would have to be
        // the literal prefix segment, and the remainder is two levels deep.
        assert_eq!(version_from_key("nook", &content), None);
    }

    #[test]
    fn foreign_keys_are_not_mistaken_for_artifacts() {
        // Someone else's objects in a shared bucket must not show up as
        // downloadable node builds.
        assert_eq!(version_from_key("nook", "other/0.1.0/thing"), None);
        assert_eq!(version_from_key("nook", "nook/0.1.0/deep/thing"), None);
        assert_eq!(version_from_key("nook", "nook/justafile"), None);
    }
}
