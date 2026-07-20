//! Artifacts in object storage.
//!
//! One client covers three backends, because they all speak the S3 API:
//!
//! - **AWS S3** — leave the endpoint unset.
//! - **MinIO** — set the endpoint to your MinIO URL. Path-style addressing is
//!   the default here for exactly this reason: virtual-host style needs
//!   wildcard DNS per bucket, which a self-hosted MinIO almost never has.
//! - **Google Cloud Storage** — point the endpoint at
//!   `https://storage.googleapis.com` and use an HMAC key pair. GCS's
//!   S3-compatible XML API covers everything this module does.
//!
//! Credentials come from config, or from the ambient AWS chain (instance role,
//! `~/.aws`, environment) when none are given — so a deployment on EC2 can use
//! a role and never hold a secret.

use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use sha2::Digest;

use super::{ArtifactStore, ObjectMeta};

pub struct S3Store {
    client: Client,
    bucket: String,
    endpoint: String,
}

impl S3Store {
    pub async fn from_config(cfg: &crate::config::Config) -> Result<Self> {
        let bucket = cfg
            .s3_bucket
            .clone()
            .filter(|b| !b.is_empty())
            .context("NOOK_S3_BUCKET is required when NOOK_ARTIFACT_STORE=s3")?;

        let region = aws_config::Region::new(
            cfg.s3_region
                .clone()
                .filter(|r| !r.is_empty())
                .unwrap_or_else(|| "us-east-1".into()),
        );

        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest()).region(region);

        // Explicit keys win; without them the SDK's own chain applies, which is
        // how an instance role works.
        if let (Some(key), Some(secret)) = (&cfg.s3_access_key_id, &cfg.s3_secret_access_key) {
            if !key.is_empty() && !secret.is_empty() {
                loader = loader.credentials_provider(aws_credential_types::Credentials::new(
                    key.clone(),
                    secret.clone(),
                    None,
                    None,
                    "nook-config",
                ));
            }
        }
        if let Some(endpoint) = cfg.s3_endpoint.clone().filter(|e| !e.is_empty()) {
            loader = loader.endpoint_url(endpoint);
        }

        let shared = loader.load().await;
        let mut builder = aws_sdk_s3::config::Builder::from(&shared);
        // MinIO and most self-hosted gateways need path-style; AWS accepts it.
        builder.set_force_path_style(Some(cfg.s3_path_style));
        let client = Client::from_conf(builder.build());

        let endpoint = cfg.s3_endpoint.clone().unwrap_or_else(|| "aws".to_string());

        let store = Self {
            client,
            bucket,
            endpoint,
        };

        // Fail construction, not the first download: a misconfigured bucket
        // should be visible in the boot log rather than to whoever runs the
        // install script three days later.
        store
            .client
            .head_bucket()
            .bucket(&store.bucket)
            .send()
            .await
            .with_context(|| format!("cannot reach bucket '{}'", store.bucket))?;

        Ok(store)
    }
}

#[async_trait]
impl ArtifactStore for S3Store {
    async fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut req = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(prefix);
            if let Some(t) = &token {
                req = req.continuation_token(t);
            }
            let resp = req.send().await.context("listing artifacts failed")?;
            for obj in resp.contents() {
                if let Some(key) = obj.key() {
                    out.push(ObjectMeta {
                        key: key.to_string(),
                        size: obj.size().unwrap_or(0).max(0) as u64,
                        // The ETag is an MD5 for single-part uploads, so it is
                        // NOT a sha256 and must not be presented as one.
                        sha256: None,
                    });
                }
            }
            match resp.next_continuation_token() {
                Some(t) if resp.is_truncated().unwrap_or(false) => token = Some(t.to_string()),
                _ => break,
            }
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    async fn get(&self, key: &str) -> Result<Vec<u8>> {
        let resp = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .with_context(|| format!("no artifact at {key}"))?;
        let bytes = resp.body.collect().await?.into_bytes();
        Ok(bytes.to_vec())
    }

    async fn put(&self, key: &str, bytes: Vec<u8>) -> Result<()> {
        // Record the digest as object metadata now, while the bytes are in
        // hand. Object storage can't compute it for us later, and the ETag is
        // an MD5 (and not even that for multipart uploads) — so without this
        // there is no way to tell a node what it should have downloaded.
        let sha256 = format!("{:x}", sha2::Sha256::digest(&bytes));
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(bytes))
            .content_type("application/octet-stream")
            .metadata("sha256", sha256)
            .send()
            .await
            .with_context(|| format!("uploading {key} failed"))?;
        Ok(())
    }

    async fn head(&self, key: &str) -> Result<Option<ObjectMeta>> {
        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(resp) => Ok(Some(ObjectMeta {
                key: key.to_string(),
                size: resp.content_length().unwrap_or(0).max(0) as u64,
                // Set by the uploader, since the store can't compute it for us.
                sha256: resp
                    .metadata()
                    .and_then(|m| m.get("sha256"))
                    .map(|s| s.to_string()),
            })),
            // Any error here means "can't serve it", and the caller's next move
            // is the same either way: report it missing.
            Err(_) => Ok(None),
        }
    }

    async fn presign(&self, key: &str, ttl: Duration) -> Result<Option<String>> {
        let config = PresigningConfig::expires_in(ttl)?;
        let req = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .presigned(config)
            .await
            .context("presigning failed")?;
        Ok(Some(req.uri().to_string()))
    }

    fn describe(&self) -> String {
        format!("s3:{}/{}", self.endpoint, self.bucket)
    }
}
