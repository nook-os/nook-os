//! All runtime configuration comes from the environment (.env in dev).
//! No configuration value may name a specific auth provider — the OIDC issuer
//! is whatever the deployment points at.

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Config {
    pub app_env: String,
    pub bind: String,
    pub public_base_url: String,
    pub web_origin: String,
    pub database_url: String,

    pub oidc_issuer_url: Option<String>,
    pub oidc_client_id: Option<String>,
    pub oidc_client_secret: Option<String>,
    pub oidc_redirect_url: Option<String>,
    pub oidc_scopes: String,

    pub session_secret: String,
    pub session_ttl_hours: i64,
    pub default_tenant_name: String,
    pub auth_dev_mode: bool,

    pub mcp_token: Option<String>,
    pub dev_join_token: Option<String>,

    /// Where node binaries live, served at `/dist/<artifact>`. The control
    /// image drops the build it was compiled with here.
    pub dist_dir: String,

    /// Where the node agent connects, separate from the browser/API port.
    ///
    /// Two reasons they are split. The agent connection is the one that will
    /// carry mutual TLS: its handshake has to terminate at the control plane
    /// (only it can pick the right tenant CA), which means the edge proxy must
    /// pass it through rather than terminate it — a different rule than the
    /// browser API wants. And it keeps a machine credential and a browser
    /// session on separate doors, so a proxy misconfiguration on one cannot
    /// quietly widen the other.
    pub agent_bind: String,

    // ── Artifact storage ────────────────────────────────────────────────
    /// `disk` (default) or `s3`. See `crate::storage`.
    pub artifact_store: String,
    /// Key prefix inside the bucket, so NookOS can share a bucket with
    /// whatever else the operator keeps there.
    pub artifact_prefix: String,
    /// Hand out a time-limited URL to the object store instead of streaming
    /// the bytes through this server. Faster, but exposes the store's hostname
    /// to whoever is installing — off by default so one hostname serves
    /// everything.
    pub artifact_redirect: bool,

    pub s3_bucket: Option<String>,
    /// Unset for AWS; set for MinIO or GCS.
    pub s3_endpoint: Option<String>,
    pub s3_region: Option<String>,
    pub s3_access_key_id: Option<String>,
    pub s3_secret_access_key: Option<String>,
    /// Path-style addressing (`host/bucket/key`). Default true, because
    /// self-hosted gateways rarely have per-bucket DNS.
    pub s3_path_style: bool,
}

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let cfg = Self {
            app_env: env_opt("APP_ENV").unwrap_or_else(|| "dev".into()),
            bind: env_opt("CONTROL_PLANE_BIND").unwrap_or_else(|| "0.0.0.0:8080".into()),
            public_base_url: env_opt("PUBLIC_BASE_URL")
                .unwrap_or_else(|| "http://localhost:8080".into()),
            web_origin: env_opt("WEB_ORIGIN").unwrap_or_else(|| "http://localhost:5173".into()),
            database_url: env_opt("DATABASE_URL").context("DATABASE_URL is required")?,

            oidc_issuer_url: env_opt("OIDC_ISSUER_URL"),
            oidc_client_id: env_opt("OIDC_CLIENT_ID"),
            oidc_client_secret: env_opt("OIDC_CLIENT_SECRET"),
            oidc_redirect_url: env_opt("OIDC_REDIRECT_URL"),
            oidc_scopes: env_opt("OIDC_SCOPES").unwrap_or_else(|| "openid profile email".into()),

            session_secret: env_opt("SESSION_SECRET").context("SESSION_SECRET is required")?,
            session_ttl_hours: env_opt("SESSION_TTL_HOURS")
                .and_then(|v| v.parse().ok())
                .unwrap_or(168),
            default_tenant_name: env_opt("DEFAULT_TENANT_NAME").unwrap_or_else(|| "dev".into()),
            auth_dev_mode: env_opt("AUTH_DEV_MODE")
                .map(|v| v == "true" || v == "1")
                .unwrap_or(false),

            agent_bind: env_opt("NOOK_AGENT_BIND").unwrap_or_else(|| "0.0.0.0:8081".into()),
            dist_dir: env_opt("NOOK_DIST_DIR")
                .unwrap_or_else(|| "/usr/local/share/nook/dist".into()),

            artifact_store: env_opt("NOOK_ARTIFACT_STORE").unwrap_or_else(|| "disk".into()),
            artifact_prefix: env_opt("NOOK_ARTIFACT_PREFIX").unwrap_or_else(|| "nook".into()),
            artifact_redirect: env_opt("NOOK_ARTIFACT_REDIRECT")
                .map(|v| v == "true" || v == "1")
                .unwrap_or(false),
            s3_bucket: env_opt("NOOK_S3_BUCKET"),
            s3_endpoint: env_opt("NOOK_S3_ENDPOINT"),
            s3_region: env_opt("NOOK_S3_REGION"),
            s3_access_key_id: env_opt("NOOK_S3_ACCESS_KEY_ID"),
            s3_secret_access_key: env_opt("NOOK_S3_SECRET_ACCESS_KEY"),
            s3_path_style: env_opt("NOOK_S3_PATH_STYLE")
                .map(|v| v == "true" || v == "1")
                .unwrap_or(true),
            mcp_token: env_opt("MCP_TOKEN"),
            dev_join_token: env_opt("NOOK_DEV_JOIN_TOKEN"),
        };

        if cfg.is_production() && cfg.auth_dev_mode {
            anyhow::bail!("AUTH_DEV_MODE must not be enabled when APP_ENV=production");
        }
        if cfg.session_secret.len() < 32 {
            anyhow::bail!("SESSION_SECRET must be at least 32 characters");
        }
        Ok(cfg)
    }

    pub fn is_production(&self) -> bool {
        self.app_env == "production"
    }

    pub fn oidc_configured(&self) -> bool {
        self.oidc_issuer_url.is_some()
            && self.oidc_client_id.is_some()
            && self.oidc_redirect_url.is_some()
    }
}
