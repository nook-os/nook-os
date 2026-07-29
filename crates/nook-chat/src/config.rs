//! Env-driven config, mirroring `nook-control/src/config.rs`.

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Config {
    /// The shared Postgres — the SAME database the control plane uses. Chat owns
    /// its tables in a dedicated `chat` schema (NG-3: shared DB, chat-owned
    /// tables), so its migration ledger cannot collide with the control plane's.
    pub database_url: String,
    /// Where the chat service listens.
    pub bind: String,
    /// Public URL a person reaches the app at (for links/CORS parity later).
    pub public_base_url: String,
    /// The web origin (dev: Vite on 5173).
    pub web_origin: String,
    /// `APP_ENV`, mirroring nook-infra's `Config` — only `"production"` is
    /// production. Gates the migrator's dev tolerance (MAIN-224): dev warns past
    /// a ledger ahead of this checkout, production stays strictly fatal.
    pub app_env: String,
}

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            database_url: env_opt("DATABASE_URL").context("DATABASE_URL is required")?,
            bind: env_opt("CHAT_BIND").unwrap_or_else(|| "0.0.0.0:8082".into()),
            public_base_url: env_opt("PUBLIC_BASE_URL")
                .unwrap_or_else(|| "http://localhost:5173".into()),
            web_origin: env_opt("WEB_ORIGIN").unwrap_or_else(|| "http://localhost:5173".into()),
            app_env: env_opt("APP_ENV").unwrap_or_else(|| "dev".into()),
        })
    }

    /// Only `APP_ENV=production` is production — same rule as nook-infra's
    /// `Config::is_production`.
    pub fn is_production(&self) -> bool {
        self.app_env == "production"
    }
}
