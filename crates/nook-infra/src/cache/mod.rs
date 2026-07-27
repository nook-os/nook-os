//! A small key/value cache behind a provider trait — the read-side twin of
//! `crate::storage`.
//!
//! The control plane recomputes some things on every request that change far
//! less often than they are read: the per-person tenants list `/auth/me`
//! carries is a four-table join run on every poll, even though a person's set
//! of tenants changes only when someone is granted or revoked. That is exactly
//! what a cache is for — but "add a cache" should not mean "reach for a
//! `HashMap` here and a `DashMap` there", each with its own ad-hoc expiry.
//!
//! So caching lives behind this trait, chosen from config the same way
//! `ArtifactStore` is (`storage/mod.rs`). One backend today:
//!
//! - **memory** — an in-process, TTL-aware map. The default, and all a
//!   single-instance deployment needs.
//!
//! - **redis** — a shared, cross-instance cache over the client from the
//!   redis-queue card (MAIN-150). The four operations are redis-native —
//!   `GET`, `SET … PX`, `DEL` — so it drops in behind this trait with nothing
//!   else changing. Selecting it needs `NOOK_REDIS_URL`; a missing or malformed
//!   URL refuses boot (in `Config::from_env`) rather than silently handing back
//!   a per-process cache someone asked to be shared.
//!
//! Values are opaque bytes: callers serialize (JSON today) and the cache never
//! looks inside, so a redis backend stores the identical bytes. Keys are
//! strings. A miss is `Ok(None)`, never an error — "not cached" is the normal
//! state, and a cache that cannot answer must degrade to a recompute, never to
//! a failed request.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

pub mod memory;
pub mod redis;

/// The provider names this build understands.
pub const PROVIDERS: &[&str] = &["memory", "redis"];

#[async_trait]
pub trait Cache: Send + Sync {
    /// The bytes stored under `key`, or `None` when absent or expired. A miss
    /// is not an error: the caller recomputes and (usually) repopulates.
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>>;

    /// Store `value` under `key` for at most `ttl`. Overwrites any existing
    /// entry. Maps to redis `SETEX`.
    async fn set(&self, key: &str, value: Vec<u8>, ttl: Duration) -> Result<()>;

    /// Drop `key` now, if present. Deleting an absent key is a no-op, not an
    /// error — invalidation should never fail a write path.
    async fn delete(&self, key: &str) -> Result<()>;

    /// For logs and the health page: which backend.
    fn describe(&self) -> String;
}

/// Is `name` a provider this build knows by name (implemented or reserved)?
pub fn is_known_provider(name: &str) -> bool {
    PROVIDERS.contains(&name)
}

/// Validate the configured provider at boot, mirroring the `mail_provider`
/// check in `Config::from_env`. Both providers are built; the redis-specific
/// requirement (a present, parseable `NOOK_REDIS_URL`) is enforced separately in
/// `Config::from_env`.
pub fn validate_provider(name: &str) -> Result<()> {
    match name {
        "memory" | "redis" => Ok(()),
        other => anyhow::bail!(
            "NOOK_CACHE_PROVIDER must be one of [{}] — got {other:?}",
            PROVIDERS.join(", ")
        ),
    }
}

/// Build the cache this instance is configured for.
///
/// `redis` builds a lazily-connecting client (`open` is sync + non-connecting).
/// `Config::from_env` has already refused boot for a missing or unparseable
/// `NOOK_REDIS_URL`, so the fall-throughs below are unreachable defense-in-depth,
/// not a live degradation path — a running redis cache that later goes down
/// degrades per-request (a miss), it does not fall back to memory mid-flight.
pub fn from_config(cfg: &crate::config::Config) -> Box<dyn Cache> {
    if cfg.cache_provider == "redis" {
        match cfg.redis_url.as_deref() {
            Some(url) => match crate::redis_client::RedisClient::open(url) {
                Ok(client) => {
                    let cache = redis::RedisCache::new(client);
                    tracing::info!(cache = %cache.describe(), "cache provider");
                    return Box::new(cache);
                }
                Err(e) => tracing::error!(
                    error = %e,
                    "NOOK_CACHE_PROVIDER=redis but NOOK_REDIS_URL is unusable — falling back to memory"
                ),
            },
            None => tracing::error!(
                "NOOK_CACHE_PROVIDER=redis but NOOK_REDIS_URL is unset — falling back to memory"
            ),
        }
    }
    let cache = memory::MemoryCache::new();
    tracing::info!(cache = %cache.describe(), "cache provider");
    Box::new(cache)
}

/// The one cache contract, run against every backend (AC-3).
///
/// The `Cache` trait is the whole surface, so these runners take `&dyn Cache`
/// and the memory and redis test modules are thin wrappers that build their
/// backend and call these — both prove the identical behaviour. Keys are unique
/// per call so the shared test Redis (and parallel runs) never collide.
#[cfg(test)]
pub(crate) mod contract {
    use super::Cache;
    use std::time::Duration;

    fn key(tag: &str) -> String {
        format!("nooktest:{tag}:{}", uuid::Uuid::now_v7())
    }

    pub async fn absent_key_is_a_miss(c: &dyn Cache) {
        assert_eq!(
            c.get(&key("absent")).await.unwrap(),
            None,
            "an absent key is a clean miss"
        );
    }

    pub async fn set_then_get_round_trips_the_bytes(c: &dyn Cache) {
        let k = key("roundtrip");
        c.set(&k, b"hello".to_vec(), Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(c.get(&k).await.unwrap(), Some(b"hello".to_vec()));
    }

    pub async fn set_overwrites_an_existing_entry(c: &dyn Cache) {
        let k = key("overwrite");
        c.set(&k, b"one".to_vec(), Duration::from_secs(60))
            .await
            .unwrap();
        c.set(&k, b"two".to_vec(), Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(c.get(&k).await.unwrap(), Some(b"two".to_vec()));
    }

    pub async fn delete_removes_the_entry_and_is_a_noop_when_absent(c: &dyn Cache) {
        let k = key("delete");
        c.set(&k, b"v".to_vec(), Duration::from_secs(60))
            .await
            .unwrap();
        c.delete(&k).await.unwrap();
        assert_eq!(c.get(&k).await.unwrap(), None);
        // Deleting again — and deleting a key that never existed — must not error.
        c.delete(&k).await.unwrap();
        c.delete(&key("never")).await.unwrap();
    }

    pub async fn an_entry_expires_after_its_ttl(c: &dyn Cache) {
        let k = key("expiry");
        c.set(&k, b"v".to_vec(), Duration::from_millis(250))
            .await
            .unwrap();
        assert_eq!(
            c.get(&k).await.unwrap(),
            Some(b"v".to_vec()),
            "present before the TTL"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(c.get(&k).await.unwrap(), None, "gone after the TTL");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_providers_are_accepted_and_unknown_is_rejected() {
        assert!(is_known_provider("redis"));
        assert!(validate_provider("redis").is_ok(), "redis is built now");
        assert!(validate_provider("memory").is_ok());
        assert!(!is_known_provider("elasticache"));
        assert!(validate_provider("elasticache").is_err());
    }
}
