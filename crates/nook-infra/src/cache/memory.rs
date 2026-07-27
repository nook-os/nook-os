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
    use crate::cache::contract;

    #[tokio::test]
    async fn absent_key_is_a_miss() {
        contract::absent_key_is_a_miss(&MemoryCache::new()).await;
    }
    #[tokio::test]
    async fn set_then_get_round_trips_the_bytes() {
        contract::set_then_get_round_trips_the_bytes(&MemoryCache::new()).await;
    }
    #[tokio::test]
    async fn set_overwrites_an_existing_entry() {
        contract::set_overwrites_an_existing_entry(&MemoryCache::new()).await;
    }
    #[tokio::test]
    async fn delete_removes_the_entry_and_is_a_noop_when_absent() {
        contract::delete_removes_the_entry_and_is_a_noop_when_absent(&MemoryCache::new()).await;
    }
    #[tokio::test]
    async fn an_entry_expires_after_its_ttl() {
        contract::an_entry_expires_after_its_ttl(&MemoryCache::new()).await;
    }
}
