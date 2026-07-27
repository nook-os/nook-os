//! Shared Redis client layer (MAIN-150).
//!
//! Deliberately queue-agnostic: this owns nothing but the connection, so the
//! redis queue provider (`queue::redis`) and the future `RedisCache` build on
//! the identical code. `redis::Client::open` is synchronous and does not
//! connect; the multiplexed [`ConnectionManager`] — which pools and
//! auto-reconnects — is created on first use and shared behind a `OnceCell`, so
//! constructing a `RedisClient` never blocks or fails on a cold Redis.

use std::sync::Arc;

use anyhow::{Context, Result};
use redis::aio::ConnectionManager;
use tokio::sync::OnceCell;

/// A lazily-connected, auto-reconnecting Redis handle. Cheap to clone.
#[derive(Clone)]
pub struct RedisClient {
    client: redis::Client,
    url: String,
    manager: Arc<OnceCell<ConnectionManager>>,
}

impl RedisClient {
    /// Validate the URL and prepare a handle. Does NOT connect (that happens on
    /// first [`conn`](RedisClient::conn)).
    pub fn open(url: &str) -> Result<Self> {
        let client =
            redis::Client::open(url).with_context(|| format!("invalid redis url {url:?}"))?;
        Ok(Self {
            client,
            url: url.to_string(),
            manager: Arc::new(OnceCell::new()),
        })
    }

    /// A shared multiplexed connection, connecting (and caching the manager) on
    /// first use. The manager reconnects under the hood, so callers never hold a
    /// dead socket.
    pub async fn conn(&self) -> Result<ConnectionManager> {
        let m = self
            .manager
            .get_or_try_init(|| async {
                ConnectionManager::new(self.client.clone())
                    .await
                    .context("redis connect failed")
            })
            .await?;
        Ok(m.clone())
    }

    /// Round-trip a `PING`, for health checks.
    pub async fn ping(&self) -> Result<()> {
        let mut c = self.conn().await?;
        redis::cmd("PING")
            .query_async::<()>(&mut c)
            .await
            .context("redis ping failed")?;
        Ok(())
    }

    /// For logs and the health page — which Redis, credentials redacted.
    pub fn describe(&self) -> String {
        format!("redis ({})", redact(&self.url))
    }
}

/// Strip any `user:password@` userinfo from a redis URL so it is safe to log.
fn redact(url: &str) -> String {
    // redis://[user:pass@]host:port/db
    match url.split_once("://") {
        Some((scheme, rest)) => match rest.rsplit_once('@') {
            Some((_creds, hostpart)) => format!("{scheme}://{hostpart}"),
            None => url.to_string(),
        },
        None => url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_strips_credentials_but_keeps_host() {
        assert_eq!(
            redact("redis://user:secret@host:6379/0"),
            "redis://host:6379/0"
        );
        assert_eq!(redact("redis://host:6379"), "redis://host:6379");
        assert_eq!(redact("rediss://:pw@h:6380"), "rediss://h:6380");
    }

    #[test]
    fn open_rejects_a_bad_url() {
        assert!(RedisClient::open("not-a-url").is_err());
        assert!(RedisClient::open("redis://localhost:6379").is_ok());
    }
}
