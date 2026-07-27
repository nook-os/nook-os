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
//! - **redis** is a RESERVED name with no implementation here (NG-1). The four
//!   operations are deliberately redis-native — `GET`, `SETEX`, `DEL` — so the
//!   day a `RedisCache` lands it drops in behind this trait with nothing else
//!   changing. Selecting it today fails at boot with a clear "not built yet"
//!   error rather than silently falling back, because a deployment that asked
//!   for a shared cache and silently got a per-process one would be a
//!   correctness surprise, not a convenience.
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

/// The provider names this build understands. `redis` is listed — it is a
/// known, reserved name — but is not implemented here (NG-1).
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
/// check in `Config::from_env`. `redis` is known but unbuilt, so it earns its
/// own message pointing at the working default rather than a generic "unknown".
pub fn validate_provider(name: &str) -> Result<()> {
    match name {
        "memory" => Ok(()),
        "redis" => anyhow::bail!(
            "NOOK_CACHE_PROVIDER=redis is reserved but not built yet — \
             use `memory` (the default) until a redis backend ships"
        ),
        other => anyhow::bail!(
            "NOOK_CACHE_PROVIDER must be one of [{}] — got {other:?}",
            PROVIDERS.join(", ")
        ),
    }
}

/// Build the cache this instance is configured for.
///
/// Only `memory` is constructible today; `redis` is rejected earlier by
/// `validate_provider` (called from `Config::from_env`), so by the time we get
/// here the provider is valid and anything but a recognised name falls back to
/// memory rather than panicking a boot that already passed validation.
pub fn from_config(cfg: &crate::config::Config) -> Box<dyn Cache> {
    // Only `memory` is constructible; `redis` was rejected by `validate_provider`
    // at boot, so any provider reaching here resolves to the in-memory backend.
    let _ = cfg.cache_provider.as_str();
    let cache = memory::MemoryCache::new();
    tracing::info!(cache = %cache.describe(), "cache provider");
    Box::new(cache)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redis_is_known_but_refused_with_a_pointed_message() {
        assert!(is_known_provider("redis"));
        let err = validate_provider("redis").unwrap_err().to_string();
        assert!(err.contains("not built yet"), "{err}");
        assert!(
            err.contains("memory"),
            "points at the working default: {err}"
        );
    }

    #[test]
    fn memory_is_accepted_and_unknown_is_rejected() {
        assert!(validate_provider("memory").is_ok());
        assert!(!is_known_provider("elasticache"));
        assert!(validate_provider("elasticache").is_err());
    }
}
