//! In-process, TTL-aware cache — the default provider.
//!
//! A `DashMap` of key → (bytes, expiry). Expiry is checked on read and the
//! stale entry evicted lazily; there is no background sweeper, because the one
//! consumer (the tenants list) has a bounded key space (one per active session)
//! and a short TTL, so dead entries cost a little memory until their next read,
//! not an ever-growing map. If a future consumer needs eviction under memory
//! pressure that is a reason to reach for redis, which is where this trait is
//! headed anyway.

use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;
use dashmap::DashMap;

use super::Cache;

struct Entry {
    bytes: Vec<u8>,
    expires: Instant,
}

#[derive(Default)]
pub struct MemoryCache {
    map: DashMap<String, Entry>,
}

impl MemoryCache {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl Cache for MemoryCache {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        // Resolve the guard fully inside the match so it is dropped before any
        // `remove` — holding a `DashMap` read guard across a write to the same
        // shard deadlocks.
        let expired = match self.map.get(key) {
            Some(e) if e.expires > Instant::now() => return Ok(Some(e.bytes.clone())),
            Some(_) => true,
            None => false,
        };
        if expired {
            self.map.remove(key);
        }
        Ok(None)
    }

    async fn set(&self, key: &str, value: Vec<u8>, ttl: Duration) -> Result<()> {
        self.map.insert(
            key.to_string(),
            Entry {
                bytes: value,
                expires: Instant::now() + ttl,
            },
        );
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<()> {
        self.map.remove(key);
        Ok(())
    }

    fn describe(&self) -> String {
        "in-memory (per-process, TTL)".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn set_then_get_round_trips_the_bytes() {
        let c = MemoryCache::new();
        assert_eq!(
            c.get("k").await.unwrap(),
            None,
            "absent key is a clean miss"
        );
        c.set("k", b"hello".to_vec(), Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(c.get("k").await.unwrap(), Some(b"hello".to_vec()));
    }

    #[tokio::test]
    async fn an_entry_expires_after_its_ttl() {
        let c = MemoryCache::new();
        c.set("k", b"v".to_vec(), Duration::from_millis(10))
            .await
            .unwrap();
        assert_eq!(c.get("k").await.unwrap(), Some(b"v".to_vec()));
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(c.get("k").await.unwrap(), None, "the TTL backstop fired");
    }

    #[tokio::test]
    async fn delete_removes_the_entry_and_is_a_noop_when_absent() {
        let c = MemoryCache::new();
        c.set("k", b"v".to_vec(), Duration::from_secs(60))
            .await
            .unwrap();
        c.delete("k").await.unwrap();
        assert_eq!(c.get("k").await.unwrap(), None);
        // Deleting again — and deleting a key that never existed — must not error.
        c.delete("k").await.unwrap();
        c.delete("never").await.unwrap();
    }

    #[tokio::test]
    async fn set_overwrites_an_existing_entry() {
        let c = MemoryCache::new();
        c.set("k", b"one".to_vec(), Duration::from_secs(60))
            .await
            .unwrap();
        c.set("k", b"two".to_vec(), Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(c.get("k").await.unwrap(), Some(b"two".to_vec()));
    }
}
