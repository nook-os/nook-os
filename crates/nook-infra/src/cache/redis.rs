//! The Redis cache backend (MAIN-151).
//!
//! `GET` / `SET … PX` / `DEL` over the shared [`RedisClient`], storing the
//! caller's opaque bytes unchanged — so a value written by one control-plane
//! instance is read by every other, which is the whole reason to reach past the
//! per-process [`memory`](super::memory) backend.
//!
//! ## Degrade to a miss, never a failed request
//!
//! The [`Cache`] contract is explicit: a cache that cannot answer must degrade
//! to a recompute, not fail the caller. So every Redis error here is swallowed
//! and logged, not propagated — `get` returns `Ok(None)` (a miss; the caller
//! recomputes), and `set`/`delete` return `Ok(())` (the write path proceeds,
//! merely uncached). A Redis outage turns the cache off; it does not turn the
//! service off.

use std::time::Duration;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use tokio::time::timeout;

use super::Cache;
use crate::redis_client::RedisClient;

/// Hard ceiling on any single cache operation. A cache exists to make requests
/// faster, so it must never make one meaningfully slower: if Redis cannot answer
/// within this — a cold connect to a downed server, a hung socket — the
/// operation is abandoned and degrades (a miss / a no-op). Comfortably above a
/// healthy same-network round-trip (single-digit ms), tight enough that an
/// outage adds at most this much latency before the caller recomputes.
const OP_TIMEOUT: Duration = Duration::from_secs(2);

/// A shared, cross-instance cache over Redis. Cheap to clone the client behind.
pub struct RedisCache {
    client: RedisClient,
}

impl RedisCache {
    pub fn new(client: RedisClient) -> Self {
        Self { client }
    }

    async fn try_get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let mut c = self.client.conn().await?;
        let v: Option<Vec<u8>> = redis::cmd("GET").arg(key).query_async(&mut c).await?;
        Ok(v)
    }

    async fn try_set(&self, key: &str, value: Vec<u8>, ttl: Duration) -> Result<()> {
        let mut c = self.client.conn().await?;
        // `SET … PX` is `SETEX` with millisecond resolution, so sub-second TTLs —
        // which the in-memory backend honours — survive on Redis too. Floor at 1ms
        // (PX 0 is an error).
        let px = ttl.as_millis().max(1) as u64;
        redis::cmd("SET")
            .arg(key)
            .arg(value)
            .arg("PX")
            .arg(px)
            .query_async::<()>(&mut c)
            .await?;
        Ok(())
    }

    async fn try_delete(&self, key: &str) -> Result<()> {
        let mut c = self.client.conn().await?;
        redis::cmd("DEL").arg(key).query_async::<()>(&mut c).await?;
        Ok(())
    }
}

/// Run `fut` under [`OP_TIMEOUT`], flattening a timeout into an `Err` so callers
/// treat "Redis didn't answer" and "Redis errored" identically — both degrade.
async fn bounded<T>(fut: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    match timeout(OP_TIMEOUT, fut).await {
        Ok(r) => r,
        Err(_) => Err(anyhow!("redis operation exceeded {OP_TIMEOUT:?}")),
    }
}

#[async_trait]
impl Cache for RedisCache {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        match bounded(self.try_get(key)).await {
            Ok(v) => Ok(v),
            Err(e) => {
                tracing::warn!(error = %e, key, "redis cache GET failed — degrading to a miss");
                Ok(None)
            }
        }
    }

    async fn set(&self, key: &str, value: Vec<u8>, ttl: Duration) -> Result<()> {
        if let Err(e) = bounded(self.try_set(key, value, ttl)).await {
            tracing::warn!(error = %e, key, "redis cache SET failed — value left uncached");
        }
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<()> {
        if let Err(e) = bounded(self.try_delete(key)).await {
            tracing::warn!(error = %e, key, "redis cache DEL failed — key left in place");
        }
        Ok(())
    }

    fn describe(&self) -> String {
        self.client.describe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::contract;

    /// Connect to the test Redis (the compose `redis` service by default; set
    /// `NOOK_TEST_REDIS_URL` to point elsewhere, e.g. `redis://localhost:6379`
    /// on the host or in CI). Skips gracefully if Redis is unreachable, so the
    /// suite is green without it — but green *with* it is what proves the cache
    /// contract holds on Redis (AC-3).
    async fn cache() -> Option<RedisCache> {
        let url = std::env::var("NOOK_TEST_REDIS_URL")
            .unwrap_or_else(|_| "redis://redis:6379".to_string());
        let client = RedisClient::open(&url).ok()?;
        client.ping().await.ok()?;
        Some(RedisCache::new(client))
    }

    macro_rules! skip_or {
        () => {{
            let Some(c) = cache().await else {
                eprintln!("skipping — no reachable test Redis (NOOK_TEST_REDIS_URL)");
                return;
            };
            c
        }};
    }

    #[tokio::test]
    async fn absent_key_is_a_miss() {
        let c = skip_or!();
        contract::absent_key_is_a_miss(&c).await;
    }
    #[tokio::test]
    async fn set_then_get_round_trips_the_bytes() {
        let c = skip_or!();
        contract::set_then_get_round_trips_the_bytes(&c).await;
    }
    #[tokio::test]
    async fn set_overwrites_an_existing_entry() {
        let c = skip_or!();
        contract::set_overwrites_an_existing_entry(&c).await;
    }
    #[tokio::test]
    async fn delete_removes_the_entry_and_is_a_noop_when_absent() {
        let c = skip_or!();
        contract::delete_removes_the_entry_and_is_a_noop_when_absent(&c).await;
    }
    #[tokio::test]
    async fn an_entry_expires_after_its_ttl() {
        let c = skip_or!();
        contract::an_entry_expires_after_its_ttl(&c).await;
    }

    /// AC-2: pointed at an unreachable Redis, the cache degrades — a miss on
    /// `get`, a silent no-op on `set`/`delete` — and never surfaces an error.
    /// Runs without any Redis, because it exercises the *failure* path.
    #[tokio::test]
    async fn an_outage_degrades_to_a_miss_never_an_error() {
        // A syntactically valid URL nothing is listening on → connect refused.
        let client = RedisClient::open("redis://127.0.0.1:1/0").unwrap();
        let c = RedisCache::new(client);
        assert_eq!(
            c.get("k").await.unwrap(),
            None,
            "get degrades to a miss, not an error"
        );
        // set + delete must not surface the outage as a failed write path.
        c.set("k", b"v".to_vec(), Duration::from_secs(60))
            .await
            .unwrap();
        c.delete("k").await.unwrap();
        assert_eq!(
            c.get("k").await.unwrap(),
            None,
            "still a miss after a failed set"
        );
    }
}
