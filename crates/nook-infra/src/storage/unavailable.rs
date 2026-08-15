//! The store a deployment configured, when it cannot be built (MAIN-598).
//!
//! `from_config` used to answer a broken S3 configuration with a `DiskStore`.
//! That is the worst of the three possible answers: the operator's stated
//! backend is silently not the one in use, uploads land on a container
//! filesystem nobody is backing up, and the deployment looks healthy. So a
//! configuration naming `s3` stays `s3` — as this, which reports what it is
//! and refuses every operation with the reason it could not be built.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use super::{ArtifactStore, ObjectMeta};

pub struct UnavailableStore {
    backend: String,
    target: String,
    reason: String,
}

impl UnavailableStore {
    pub fn new(backend: &str, target: &str, reason: &str) -> Self {
        Self {
            backend: backend.to_string(),
            target: target.to_string(),
            reason: reason.to_string(),
        }
    }

    fn refuse<T>(&self) -> Result<T> {
        Err(anyhow::anyhow!(
            "{} storage at {} is unusable: {}",
            self.backend,
            self.target,
            self.reason
        ))
    }
}

#[async_trait]
impl ArtifactStore for UnavailableStore {
    async fn list(&self, _prefix: &str) -> Result<Vec<ObjectMeta>> {
        self.refuse()
    }

    async fn get(&self, _key: &str) -> Result<Vec<u8>> {
        self.refuse()
    }

    async fn put(&self, _key: &str, _bytes: Vec<u8>) -> Result<()> {
        self.refuse()
    }

    async fn head(&self, _key: &str) -> Result<Option<ObjectMeta>> {
        self.refuse()
    }

    async fn delete(&self, _key: &str) -> Result<()> {
        self.refuse()
    }

    async fn presign(&self, _key: &str, _ttl: Duration) -> Result<Option<String>> {
        self.refuse()
    }

    fn describe(&self) -> String {
        format!("{}:{} (unusable)", self.backend, self.target)
    }
}
