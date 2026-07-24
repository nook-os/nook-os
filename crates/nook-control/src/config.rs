//! All runtime configuration comes from the environment (.env in dev).
//! No configuration value may name a specific auth provider — the OIDC issuer
//! is whatever the deployment points at.

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Config {
    pub app_env: String,
    pub bind: String,
    /// How long, in seconds, in-flight requests get to finish after a shutdown
    /// signal before the process exits anyway. Bounds the graceful drain so one
    /// stuck connection cannot hold the pod past Kubernetes' own grace period
    /// (`terminationGracePeriodSeconds`) and earn a SIGKILL mid-request. Set a
    /// touch below that value; defaults to 25s.
    pub shutdown_grace_secs: u64,
    pub public_base_url: String,
    pub web_origin: String,
    pub database_url: String,

    pub oidc_issuer_url: Option<String>,
    pub oidc_client_id: Option<String>,
    /// A second, PUBLIC client registered at the IdP for native apps.
    ///
    /// The control plane's own client is confidential — it holds a secret, and
    /// a secret shipped inside a desktop binary is not a secret. Native
    /// clients need their own public client, which is what the device
    /// authorization grant expects anyway.
    pub oidc_device_client_id: Option<String>,
    /// Where native clients start a device authorization.
    ///
    /// This belongs in the IdP's discovery document, and a compliant client
    /// would find it there. Two things make it configuration instead: the
    /// `openidconnect` crate's `CoreProviderMetadata` drops unrecognised
    /// fields, and providers that support the grant do not always advertise
    /// the endpoint — auth.hein.network lists `device_code` under
    /// `grant_types_supported` while omitting `device_authorization_endpoint`,
    /// so nothing can discover it. Setting it here unblocks that; fixing the
    /// discovery document is the better repair.
    pub oidc_device_authorization_endpoint: Option<String>,
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

    /// `owner/repo` whose tagged releases carry the node binaries.
    ///
    /// The control plane used to host them itself. That was the wrong call: it
    /// could only ever serve what its own build host could compile (no macOS),
    /// and it made every deployment a binary mirror. Releases are a release
    /// problem, so they live with the tags.
    pub releases_repo: String,

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
    /// Public URL of the agent listener, handed to joining machines.
    ///
    /// Defaults to the API's public URL, which is right for a single-host
    /// deployment where both are the same name. Set it when the agent port has
    /// its own hostname — a proxy that terminates TLS breaks mutual auth, so
    /// that name usually resolves somewhere different on purpose.
    pub agent_public_url: Option<String>,

    /// PEM certificate the agent listener serves, when the control plane
    /// terminates TLS itself. Its SHA-256 goes into join tokens so a machine
    /// can pin the server on first contact instead of trusting whatever the
    /// web PKI vouches for — any public CA can issue for any hostname, so root
    /// -store validation alone would not stop a mis-issued certificate.
    pub agent_tls_cert: Option<String>,
    /// Private key for `agent_tls_cert`. Both must be set for the agent
    /// listener to terminate TLS; either alone is a misconfiguration.
    pub agent_tls_key: Option<String>,

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

    // ── Email (mail provider) ───────────────────────────────────────────
    /// Which mail transport to use, chosen by name — `smtp` or `capture`.
    /// Explicit, like `NOOK_ARTIFACT_STORE`, rather than inferred from whether
    /// an SMTP host happens to be set: a provider is a deployment decision, and
    /// adding a hosted transport later (SES, Sendgrid, …) means a new name here,
    /// not new inference. Defaults to `capture` (logs, does not send).
    pub mail_provider: String,
    /// SMTP host, used when `mail_provider = smtp`. Dev points at Mailpit, prod
    /// at the mail relay.
    pub smtp_host: Option<String>,
    /// SMTP port. 587 (submission/STARTTLS) by default; 1025 for Mailpit, 465
    /// for implicit TLS.
    pub smtp_port: u16,
    /// Transport security: `starttls` (default), `implicit` (TLS from the
    /// first byte, port 465), or `none` (plaintext, for Mailpit).
    pub smtp_tls: String,
    /// The envelope/from address, e.g. `NookOS <no-reply@example.com>`.
    pub smtp_from: String,
    /// Optional SMTP auth. Both set → credentials are sent; Mailpit needs none.
    pub smtp_username: Option<String>,
    pub smtp_password: Option<String>,

    // ── Postmark (HTTP mail provider) ───────────────────────────────────
    /// Server token for `mail_provider = postmark`, sent as the
    /// `X-Postmark-Server-Token` header. Missing → the provider fails to build
    /// and falls back to capture (nothing is delivered), never a panic.
    pub postmark_token: Option<String>,
    /// Postmark's send endpoint. Overridable so tests can point it at a mock.
    pub postmark_api_url: String,
    /// The verified sender used by the Postmark provider, e.g.
    /// `NookOS <no-reply@hein.network>`. Separate from `smtp_from` because the
    /// verified sender at Postmark need not match the SMTP relay's envelope.
    pub mail_from: String,

    // ── Send guards (provider-agnostic) ─────────────────────────────────
    /// GLOBAL send switch. Default OFF: unless this is true, EVERY provider —
    /// including a fully-configured postmark/smtp — captures (logs "would
    /// send"), delivering nothing. Turning sending on is a deliberate ops step.
    pub mail_send_enabled: bool,
    /// Whether `notification`-category mail (the email notification channel) may
    /// send. Default OFF: even with `mail_send_enabled`, notifications stay
    /// captured until this is separately turned on. Transactional mail
    /// (verification, invites) is unaffected by this flag.
    pub mail_notifications_enabled: bool,
    /// Hard monthly cap on REAL sends — a backstop against exhausting the
    /// Postmark allowance. At the cap, further sends are captured (WARN), not
    /// sent. Counted from recorded sends, so it survives restarts. Defaults to
    /// 100 (the Postmark free allowance); `0` blocks all real sends.
    pub mail_max_per_month: Option<i64>,
    /// Optional daily cap, same semantics as the monthly one. Unset by default.
    pub mail_max_per_day: Option<i64>,
}

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let cfg = Self {
            app_env: env_opt("APP_ENV").unwrap_or_else(|| "dev".into()),
            bind: env_opt("CONTROL_PLANE_BIND").unwrap_or_else(|| "0.0.0.0:8080".into()),
            shutdown_grace_secs: env_opt("NOOK_SHUTDOWN_GRACE_SECS")
                .and_then(|v| v.parse().ok())
                .unwrap_or(25),
            public_base_url: env_opt("PUBLIC_BASE_URL")
                .unwrap_or_else(|| "http://localhost:8080".into()),
            web_origin: env_opt("WEB_ORIGIN").unwrap_or_else(|| "http://localhost:5173".into()),
            database_url: env_opt("DATABASE_URL").context("DATABASE_URL is required")?,

            oidc_issuer_url: env_opt("OIDC_ISSUER_URL"),
            oidc_client_id: env_opt("OIDC_CLIENT_ID"),
            oidc_device_client_id: env_opt("OIDC_DEVICE_CLIENT_ID"),
            oidc_device_authorization_endpoint: env_opt("OIDC_DEVICE_AUTHORIZATION_ENDPOINT"),
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

            releases_repo: env_opt("NOOK_RELEASES_REPO")
                .unwrap_or_else(|| "nook-os/nook-os".into()),
            agent_tls_cert: env_opt("NOOK_AGENT_TLS_CERT"),
            agent_tls_key: env_opt("NOOK_AGENT_TLS_KEY"),
            agent_bind: env_opt("NOOK_AGENT_BIND").unwrap_or_else(|| "0.0.0.0:8081".into()),
            agent_public_url: env_opt("NOOK_AGENT_PUBLIC_URL"),
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

            mail_provider: env_opt("MAIL_PROVIDER").unwrap_or_else(|| "capture".into()),
            smtp_host: env_opt("SMTP_HOST"),
            smtp_port: env_opt("SMTP_PORT")
                .and_then(|v| v.parse().ok())
                .unwrap_or(587),
            smtp_tls: env_opt("SMTP_TLS").unwrap_or_else(|| "starttls".into()),
            smtp_from: env_opt("SMTP_FROM").unwrap_or_else(|| "NookOS <no-reply@localhost>".into()),
            smtp_username: env_opt("SMTP_USERNAME"),
            smtp_password: env_opt("SMTP_PASSWORD"),

            postmark_token: env_opt("POSTMARK_TOKEN"),
            postmark_api_url: env_opt("POSTMARK_API_URL")
                .unwrap_or_else(|| "https://api.postmarkapp.com/email".into()),
            mail_from: env_opt("MAIL_FROM").unwrap_or_else(|| "NookOS <no-reply@localhost>".into()),

            mail_send_enabled: env_opt("MAIL_SEND_ENABLED")
                .map(|v| v == "true" || v == "1")
                .unwrap_or(false),
            mail_notifications_enabled: env_opt("MAIL_NOTIFICATIONS_ENABLED")
                .map(|v| v == "true" || v == "1")
                .unwrap_or(false),
            // Always a monthly cap; unset or unparseable falls back to 100 (the
            // Postmark free allowance) so a typo can never silently uncap sends.
            mail_max_per_month: Some(
                env_opt("MAIL_MAX_PER_MONTH")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(100),
            ),
            mail_max_per_day: env_opt("MAIL_MAX_PER_DAY").and_then(|v| v.parse().ok()),
        };

        if cfg.is_production() && cfg.auth_dev_mode {
            anyhow::bail!("AUTH_DEV_MODE must not be enabled when APP_ENV=production");
        }
        // An unknown mail provider is a misconfiguration worth stopping for,
        // rather than silently falling through to some default and dropping mail.
        if !crate::mailer::is_known_provider(&cfg.mail_provider) {
            anyhow::bail!(
                "MAIL_PROVIDER must be one of [{}] — got {:?}",
                crate::mailer::PROVIDERS.join(", "),
                cfg.mail_provider
            );
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

    /// A Config with everything defaulted, for unit tests that need one without
    /// touching the process environment. Mirrors the `from_env` defaults.
    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self {
            app_env: "test".into(),
            bind: "127.0.0.1:0".into(),
            shutdown_grace_secs: 25,
            public_base_url: "http://localhost:8080".into(),
            web_origin: "http://localhost:5173".into(),
            database_url: String::new(),
            oidc_issuer_url: None,
            oidc_client_id: None,
            oidc_device_client_id: None,
            oidc_device_authorization_endpoint: None,
            oidc_client_secret: None,
            oidc_redirect_url: None,
            oidc_scopes: "openid profile email".into(),
            session_secret: "0".repeat(64),
            session_ttl_hours: 168,
            default_tenant_name: "test".into(),
            auth_dev_mode: true,
            mcp_token: None,
            dev_join_token: None,
            dist_dir: "/tmp".into(),
            releases_repo: "nook-os/nook-os".into(),
            agent_bind: "127.0.0.1:0".into(),
            agent_public_url: None,
            agent_tls_cert: None,
            agent_tls_key: None,
            artifact_store: "disk".into(),
            artifact_prefix: "nook".into(),
            artifact_redirect: false,
            s3_bucket: None,
            s3_endpoint: None,
            s3_region: None,
            s3_access_key_id: None,
            s3_secret_access_key: None,
            s3_path_style: true,
            mail_provider: "capture".into(),
            smtp_host: None,
            smtp_port: 587,
            smtp_tls: "starttls".into(),
            smtp_from: "NookOS <no-reply@localhost>".into(),
            smtp_username: None,
            smtp_password: None,
            postmark_token: None,
            postmark_api_url: "https://api.postmarkapp.com/email".into(),
            mail_from: "NookOS <no-reply@localhost>".into(),
            // Tests get the shipped-safe defaults: sending off, notifications
            // off, a monthly cap. Guard tests set these explicitly per case.
            mail_send_enabled: false,
            mail_notifications_enabled: false,
            mail_max_per_month: Some(100),
            mail_max_per_day: None,
        }
    }
}
