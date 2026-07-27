//! Artifacts in a directory.
//!
//! The default, and the one that needs no configuration: the container image
//! drops the binary it built into `dist_dir`, and a single-machine install
//! never has to think about object storage at all.
//!
//! Layout matches the S3 backend exactly (`<dir>/<version>/<name>`), with one
//! concession to history: a file sitting directly in `dist_dir` is treated as
//! belonging to this server's own version. That is what the image has always
//! produced, and an upgrade shouldn't strand the binary already on disk.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use sha2::{Digest, Sha256};

use super::{ArtifactStore, ObjectMeta};

pub struct DiskStore {
    root: PathBuf,
}

impl DiskStore {
    pub fn new(root: &str) -> Self {
        Self {
            root: PathBuf::from(root),
        }
    }

    /// Resolve a key to a path, refusing anything that could climb out of the
    /// root. Keys arrive from HTTP, so this is a boundary, not a formality.
    fn path_for(&self, key: &str) -> Result<PathBuf> {
        let mut out = self.root.clone();
        for segment in key.split('/').filter(|s| !s.is_empty()) {
            if segment == "." || segment == ".." || segment.contains('\\') {
                anyhow::bail!("unsafe artifact key");
            }
            out.push(segment);
        }
        Ok(out)
    }
}

#[async_trait]
impl ArtifactStore for DiskStore {
    async fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>> {
        let base = self.path_for(prefix)?;
        let mut out = Vec::new();
        let mut stack = vec![base.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
                continue; // missing directory is an empty listing, not an error
            };
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                let Ok(meta) = entry.metadata().await else {
                    continue;
                };
                if meta.is_dir() {
                    stack.push(path);
                } else if meta.is_file() {
                    // Key it relative to the root so callers see the same
                    // strings they would from S3.
                    if let Ok(rel) = path.strip_prefix(&self.root) {
                        out.push(ObjectMeta {
                            key: rel.to_string_lossy().replace('\\', "/"),
                            size: meta.len(),
                            sha256: None,
                        });
                    }
                }
            }
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    async fn get(&self, key: &str) -> Result<Vec<u8>> {
        let path = self.path_for(key)?;
        tokio::fs::read(&path)
            .await
            .with_context(|| format!("no artifact at {}", path.display()))
    }

    async fn put(&self, key: &str, bytes: Vec<u8>) -> Result<()> {
        let path = self.path_for(key)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        // Write beside, then rename: a half-written binary that a node
        // downloads is worse than one that isn't there yet.
        let tmp = path.with_extension("partial");
        tokio::fs::write(&tmp, &bytes).await?;
        tokio::fs::rename(&tmp, &path).await?;
        Ok(())
    }

    async fn head(&self, key: &str) -> Result<Option<ObjectMeta>> {
        let path = self.path_for(key)?;
        let Ok(meta) = tokio::fs::metadata(&path).await else {
            return Ok(None);
        };
        if !meta.is_file() {
            return Ok(None);
        }
        // Local reads are cheap and the checksum is the only way a download
        // over a flaky link can be verified, so pay for it here.
        let sha256 = tokio::fs::read(&path)
            .await
            .map(|b| format!("{:x}", Sha256::digest(&b)))
            .ok();
        Ok(Some(ObjectMeta {
            key: key.to_string(),
            size: meta.len(),
            sha256,
        }))
    }

    async fn presign(&self, _key: &str, _ttl: Duration) -> Result<Option<String>> {
        // Nothing to sign — a directory has no URL of its own. The control
        // plane streams these.
        Ok(None)
    }

    fn describe(&self) -> String {
        format!("disk:{}", self.root.display())
    }
}

/// Does a bare `<dist_dir>/<name>` exist? The pre-versioning layout the
/// container image still produces.
pub fn legacy_path(root: &str, name: &str) -> Option<PathBuf> {
    let p = Path::new(root).join(name);
    p.is_file().then_some(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn refuses_to_escape_the_root() {
        let store = DiskStore::new("/tmp/nook-test-dist");
        assert!(store.path_for("../../etc/passwd").is_err());
        assert!(store.path_for("0.1.0/../../etc/passwd").is_err());
        assert!(store.path_for("0.1.0/nook-linux-x86_64").is_ok());
    }

    #[tokio::test]
    async fn put_then_get_round_trips() {
        let dir = std::env::temp_dir().join(format!("nook-store-{}", uuid::Uuid::now_v7()));
        let store = DiskStore::new(&dir.to_string_lossy());
        store
            .put("0.1.0/nook-linux-x86_64", b"binary".to_vec())
            .await
            .unwrap();

        assert_eq!(
            store.get("0.1.0/nook-linux-x86_64").await.unwrap(),
            b"binary"
        );
        let head = store
            .head("0.1.0/nook-linux-x86_64")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(head.size, 6);
        assert!(head.sha256.is_some());

        let listed = store.list("").await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].key, "0.1.0/nook-linux-x86_64");

        // A missing key is None, not an error.
        assert!(store.head("0.1.0/nope").await.unwrap().is_none());
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
