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
//!
//! They differ in one more way, and MAIN-598 is what it cost to learn it: on
//! **disk** they are two directories, not one. `dist_dir` is inside the image —
//! baked read-only at build time, mounted by nothing — so an upload written
//! there failed on every disk-backed deployment. Artifacts are built with the
//! image and read; user content arrives at runtime and must outlive it. On S3
//! the distinction does not arise: one bucket, two prefixes.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;

pub mod disk;
pub mod s3;
pub mod unavailable;

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

/// The two stores this instance is configured for, and what the boot probe
/// learned about the one that gets written (MAIN-598).
pub struct Storage {
    /// Where the node binaries are read from. Read-only in the deployments
    /// that matter: the container image populates it at build time.
    pub artifacts: Arc<dyn ArtifactStore>,
    /// Where an upload's bytes go. Its own directory on disk; the same bucket,
    /// under its own prefix, on S3.
    pub user_content: Arc<dyn ArtifactStore>,
    /// `None` when the boot probe wrote, read back and deleted an object.
    /// Otherwise the reason it could not, which the upload route answers with a
    /// 503 rather than an opaque 500.
    pub user_content_error: Option<String>,
}

/// Build the stores this instance is configured for, and prove the writable
/// one is writable.
///
/// **The backend is exactly what `NOOK_ARTIFACT_STORE` names.** A configured
/// S3 that cannot be reached used to become a `DiskStore` silently, which meant
/// a deployment that had said "put this in the bucket" wrote to a container
/// filesystem instead and looked fine doing it. A store that cannot be built
/// now stays the backend it was configured as and refuses (`UnavailableStore`).
///
/// **A failed probe does not stop the boot.** Everything else the control plane
/// does — sign-in, the board, the fleet — is unaffected by a store nobody can
/// write, and a process that refuses to start cannot be fixed from its own UI.
/// The failure is a loud WARN here and a 503 at the upload route; it is never
/// silence.
pub async fn from_config(cfg: &crate::config::Config) -> Storage {
    match cfg.artifact_store.as_str() {
        "s3" => match s3::S3Store::from_config(cfg).await {
            Ok(store) => {
                let store: Arc<dyn ArtifactStore> = Arc::new(store);
                tracing::info!(store = %store.describe(), "artifact storage");
                let user_content_error = probe(store.as_ref(), &cfg.user_content_prefix)
                    .await
                    .err()
                    .map(|e| report(&store.describe(), e));
                Storage {
                    artifacts: store.clone(),
                    user_content: store,
                    user_content_error,
                }
            }
            Err(e) => {
                let bucket = cfg.s3_bucket.clone().unwrap_or_default();
                let reason = format!("{e:#}");
                let store: Arc<dyn ArtifactStore> =
                    Arc::new(unavailable::UnavailableStore::new("s3", &bucket, &reason));
                Storage {
                    artifacts: store.clone(),
                    user_content: store.clone(),
                    user_content_error: Some(report(&store.describe(), e)),
                }
            }
        },
        _ => {
            let artifacts: Arc<dyn ArtifactStore> = Arc::new(disk::DiskStore::new(&cfg.dist_dir));
            let user_content: Arc<dyn ArtifactStore> =
                Arc::new(disk::DiskStore::new(&cfg.user_content_dir));
            tracing::info!(
                store = %artifacts.describe(),
                user_content = %user_content.describe(),
                "artifact storage"
            );
            let user_content_error = probe(user_content.as_ref(), &cfg.user_content_prefix)
                .await
                .err()
                .map(|e| report(&user_content.describe(), e));
            Storage {
                artifacts,
                user_content,
                user_content_error,
            }
        }
    }
}

/// One WARN naming the backend, the target, and the underlying OS/SDK error —
/// and the string the 503 is decided by.
///
/// The detail belongs here and only here: the log is where an operator can act
/// on a path or a bucket name, and the response body is where MAIN-273's rule
/// says it must not appear.
fn report(store: &str, e: anyhow::Error) -> String {
    let cause = format!("{e:#}");
    tracing::warn!(
        store,
        error = %cause,
        "user-content storage is unusable — uploads will be refused with a 503 until this is fixed"
    );
    // The returned copy carries the target too. The log has it as its own
    // field; this string has to stand alone, because it is what a reader —
    // a test, or a future health endpoint — gets without the log line.
    format!("{store}: {cause}")
}

/// Write one small object, read it back, delete it (MAIN-598 AC-4).
///
/// Only the store that gets WRITTEN is probed. The artifact store's whole
/// contract on disk is the read-only dist the image bakes in, so probing it
/// would warn, every boot, about a directory nothing intends to write. On S3
/// the two are one store and this covers both.
///
/// The key is unique per boot rather than a fixed name: replicas start
/// together, and two probes sharing a key would read each other's bytes and
/// delete each other's object.
async fn probe(store: &dyn ArtifactStore, prefix: &str) -> Result<()> {
    let key = user_content_key(prefix, ".probe", &uuid::Uuid::now_v7().to_string());
    let payload = b"nook storage probe".to_vec();

    store
        .put(&key, payload.clone())
        .await
        .with_context(|| format!("writing {key}"))?;
    let read_back = store
        .get(&key)
        .await
        .with_context(|| format!("reading back {key}"))?;
    // Delete before the comparison fails: a store that round-trips the wrong
    // bytes is broken, and leaving the probe object behind would not help.
    let removed = store.delete(&key).await;
    anyhow::ensure!(
        read_back == payload,
        "{key} read back {} bytes, not the {} written",
        read_back.len(),
        payload.len()
    );
    removed.with_context(|| format!("deleting {key}"))
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

    fn scratch(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "nook-storage-{tag}-{}",
            uuid::Uuid::now_v7().simple()
        ))
    }

    /// MAIN-598 AC-1: on disk the two stores are two directories. An upload
    /// must never reach `dist_dir`, which the image bakes read-only.
    #[tokio::test]
    async fn disk_puts_user_content_somewhere_other_than_the_dist_dir() {
        let dist = scratch("dist");
        let uploads = scratch("uploads");
        let mut cfg = crate::config::Config::for_test();
        cfg.dist_dir = dist.to_string_lossy().into_owned();
        cfg.user_content_dir = uploads.to_string_lossy().into_owned();

        let storage = from_config(&cfg).await;
        assert!(
            storage.user_content_error.is_none(),
            "a writable temp directory probes clean: {:?}",
            storage.user_content_error
        );

        let key = user_content_key(&cfg.user_content_prefix, "tenant", "object");
        storage
            .user_content
            .put(&key, b"bytes".to_vec())
            .await
            .unwrap();
        assert!(
            uploads.join(&key).is_file(),
            "written under user_content_dir"
        );
        assert!(
            !dist.exists(),
            "and nothing at all under dist_dir — the probe must not touch it either"
        );

        let _ = std::fs::remove_dir_all(&uploads);
    }

    /// AC-4/AC-5: the boot probe's whole job. A directory that cannot be
    /// created — its parent is a regular file, which is ENOTDIR for every uid —
    /// is reported, and the report carries the path and the OS error an
    /// operator needs.
    #[tokio::test]
    async fn the_probe_reports_a_store_it_cannot_write() {
        let blocker = scratch("blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();
        let mut cfg = crate::config::Config::for_test();
        cfg.user_content_dir = blocker.join("uploads").to_string_lossy().into_owned();

        let storage = from_config(&cfg).await;
        let reason = storage
            .user_content_error
            .expect("an unwritable store is reported, not discovered by the first uploader");
        assert!(
            reason.contains(&blocker.to_string_lossy().to_string()),
            "the path is in the reason: {reason}"
        );
        assert!(
            reason.to_ascii_lowercase().contains("writing"),
            "and which operation failed: {reason}"
        );

        let _ = std::fs::remove_file(&blocker);
    }

    /// AC-4: a store that swallows a write and answers a read with nothing is
    /// as broken as one that refuses outright, and the probe has to say so —
    /// which is why it reads the object back rather than trusting `put`.
    #[tokio::test]
    async fn the_probe_refuses_a_store_that_does_not_return_what_it_stored() {
        struct Amnesiac;

        #[async_trait]
        impl ArtifactStore for Amnesiac {
            async fn list(&self, _prefix: &str) -> Result<Vec<ObjectMeta>> {
                Ok(Vec::new())
            }
            async fn get(&self, _key: &str) -> Result<Vec<u8>> {
                Ok(Vec::new())
            }
            async fn put(&self, _key: &str, _bytes: Vec<u8>) -> Result<()> {
                Ok(())
            }
            async fn head(&self, _key: &str) -> Result<Option<ObjectMeta>> {
                Ok(None)
            }
            async fn delete(&self, _key: &str) -> Result<()> {
                Ok(())
            }
            fn describe(&self) -> String {
                "fake:amnesiac".into()
            }
        }

        let err = probe(&Amnesiac, "nook/user-content")
            .await
            .expect_err("bytes that do not come back are a failed probe");
        assert!(format!("{err}").contains("read back"), "{err}");
    }

    /// AC-3: a configured-but-unusable S3 STAYS S3. The silent fall back to a
    /// `DiskStore` meant a deployment that said "bucket" wrote to a container
    /// filesystem and looked healthy doing it.
    #[tokio::test]
    async fn a_broken_s3_configuration_never_becomes_a_disk_store() {
        let dist = scratch("s3-dist");
        let uploads = scratch("s3-uploads");
        let mut cfg = crate::config::Config::for_test();
        cfg.artifact_store = "s3".into();
        // No bucket: S3Store::from_config refuses before it can reach a network.
        cfg.s3_bucket = None;
        cfg.dist_dir = dist.to_string_lossy().into_owned();
        cfg.user_content_dir = uploads.to_string_lossy().into_owned();

        let storage = from_config(&cfg).await;
        assert!(
            storage.user_content.describe().starts_with("s3:"),
            "still s3, reported broken: {}",
            storage.user_content.describe()
        );
        assert!(storage.user_content_error.is_some());

        let key = user_content_key(&cfg.user_content_prefix, "tenant", "object");
        assert!(
            storage
                .user_content
                .put(&key, b"bytes".to_vec())
                .await
                .is_err(),
            "and it refuses writes rather than accepting them somewhere else"
        );
        assert!(
            !dist.exists() && !uploads.exists(),
            "no local disk was written"
        );
    }

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
