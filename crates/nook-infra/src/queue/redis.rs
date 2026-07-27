//! The Redis queue backend (MAIN-150).
//!
//! ## Data model — per-type lists + a claimed set
//!
//! Redis has no native visibility timeout, so this builds a reliable queue out
//! of sorted sets and a job hash, and does every state transition inside a Lua
//! script — Redis runs a script atomically (single-threaded), which is what
//! makes the claim contention-safe (two `receive`s can never hand out the same
//! job). Keys, all under the `nq:` prefix (single-node Redis; keys are built
//! inside the scripts):
//!
//! - `nq:ready:<type>` — ZSET, score = `not_before` (ms). The work waiting to be
//!   delivered; the score gives scheduled/delayed delivery for free.
//! - `nq:claimed:<type>` — ZSET, score = visibility deadline (ms). In-flight
//!   work; when a member's deadline passes it is **reclaimed** back to `ready`
//!   on the next `receive` — that is the redelivery-after-visibility guarantee.
//! - `nq:job:<id>` — HASH: `tenant`, `type`, `payload` (opaque bytes),
//!   `attempts`, `max`, `not_before`, `enqueued_at`.
//! - `nq:dead:<type>` — LIST of dead job ids; the job hash is renamed to
//!   `nq:deadjob:<id>` with a `dead_reason` field, so nothing is lost.
//! - `nq:types` — SET of every type seen, for `describe`.
//!
//! Time comes from Redis (`TIME`) inside the scripts, so there is no app↔Redis
//! clock skew — the exact analogue of the database backend using Postgres
//! `now()`. Delivery is at-least-once; the module-level contract in `queue`
//! applies (handlers must be idempotent).

use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use uuid::Uuid;

use super::{Nack, NewWork, Queue, QueueStats, WorkEnvelope};
use crate::redis_client::RedisClient;

/// Lua helpers prepended to scripts that need the clock or dead-lettering.
const PRELUDE: &str = r#"
local function now_ms()
  local t = redis.call('TIME')
  return tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)
end
local function dead_letter(ty, id, reason)
  local job = 'nq:job:' .. id
  if redis.call('EXISTS', job) == 1 then
    redis.call('HSET', job, 'dead_reason', reason)
    redis.call('RENAME', job, 'nq:deadjob:' .. id)
    redis.call('RPUSH', 'nq:dead:' .. ty, id)
  end
end
"#;

const ENQUEUE: &str = r#"
local now = now_ms()
local id, tenant, ty, payload, max, delay = ARGV[1], ARGV[2], ARGV[3], ARGV[4], tonumber(ARGV[5]), tonumber(ARGV[6])
local nb = now + delay
redis.call('HSET', 'nq:job:' .. id,
  'tenant', tenant, 'type', ty, 'payload', payload,
  'attempts', 0, 'max', max, 'not_before', nb, 'enqueued_at', now)
redis.call('ZADD', 'nq:ready:' .. ty, nb, id)
redis.call('SADD', 'nq:types', ty)
return id
"#;

const RECEIVE: &str = r#"
local now = now_ms()
local ty, limit, vis = ARGV[1], tonumber(ARGV[2]), tonumber(ARGV[3])
local ready = 'nq:ready:' .. ty
local claimed = 'nq:claimed:' .. ty
-- Reclaim any claimed work whose visibility deadline has passed (redelivery).
local expired = redis.call('ZRANGEBYSCORE', claimed, '-inf', now)
for _, id in ipairs(expired) do
  redis.call('ZREM', claimed, id)
  redis.call('ZADD', ready, now, id)
end
-- Claim up to `limit` visible (score <= now) ready jobs.
local cand = redis.call('ZRANGEBYSCORE', ready, '-inf', now, 'LIMIT', 0, limit)
local out = {}
for _, id in ipairs(cand) do
  local job = 'nq:job:' .. id
  local attempts = tonumber(redis.call('HGET', job, 'attempts')) or 0
  local max = tonumber(redis.call('HGET', job, 'max')) or 20
  if attempts >= max then
    redis.call('ZREM', ready, id)
    dead_letter(ty, id, 'max attempts exhausted')
  else
    redis.call('HINCRBY', job, 'attempts', 1)
    redis.call('ZREM', ready, id)
    redis.call('ZADD', claimed, now + vis, id)
    local h = redis.call('HMGET', job, 'tenant', 'payload', 'attempts', 'max', 'not_before', 'enqueued_at')
    out[#out + 1] = id
    for i = 1, 6 do out[#out + 1] = h[i] end
  end
end
return out
"#;

const ACK: &str = r#"
local id = ARGV[1]
local ty = redis.call('HGET', 'nq:job:' .. id, 'type')
if ty then
  redis.call('ZREM', 'nq:claimed:' .. ty, id)
  redis.call('ZREM', 'nq:ready:' .. ty, id)
  redis.call('DEL', 'nq:job:' .. id)
end
return 1
"#;

const NACK_REQUEUE: &str = r#"
local id = ARGV[1]
local ty = redis.call('HGET', 'nq:job:' .. id, 'type')
if ty then
  local now = now_ms()
  redis.call('ZREM', 'nq:claimed:' .. ty, id)
  redis.call('ZADD', 'nq:ready:' .. ty, now, id)
end
return 1
"#;

const NACK_DEAD: &str = r#"
local id, reason = ARGV[1], ARGV[2]
local ty = redis.call('HGET', 'nq:job:' .. id, 'type')
if ty then
  redis.call('ZREM', 'nq:claimed:' .. ty, id)
  redis.call('ZREM', 'nq:ready:' .. ty, id)
  dead_letter(ty, id, reason)
end
return 1
"#;

const EXTEND: &str = r#"
local id, vis = ARGV[1], tonumber(ARGV[2])
local ty = redis.call('HGET', 'nq:job:' .. id, 'type')
if ty then
  local now = now_ms()
  redis.call('ZADD', 'nq:claimed:' .. ty, now + vis, id)
end
return 1
"#;

const DESCRIBE: &str = r#"
local now = now_ms()
local ready, inflight, dead = 0, 0, 0
for _, ty in ipairs(redis.call('SMEMBERS', 'nq:types')) do
  ready = ready + redis.call('ZCOUNT', 'nq:ready:' .. ty, '-inf', now)
  inflight = inflight + redis.call('ZCOUNT', 'nq:claimed:' .. ty, '(' .. now, '+inf')
  dead = dead + redis.call('LLEN', 'nq:dead:' .. ty)
end
return {ready, inflight, dead}
"#;

pub struct RedisQueue {
    client: RedisClient,
}
// (client is read in tests via `q.client`)

impl RedisQueue {
    pub fn new(client: RedisClient) -> Self {
        Self { client }
    }

    pub fn backend(&self) -> &'static str {
        "redis"
    }

    fn script(body: &str) -> redis::Script {
        redis::Script::new(&format!("{PRELUDE}\n{body}"))
    }
}

fn ms_to_dt(ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(ms)
        .single()
        .unwrap_or_else(Utc::now)
}

fn parse_i64(bytes: &[u8]) -> i64 {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

#[async_trait::async_trait]
impl Queue for RedisQueue {
    async fn enqueue(&self, work: NewWork) -> Result<Uuid> {
        let id = Uuid::now_v7();
        let mut conn = self.client.conn().await?;
        let delay_ms = work.delay.map(|d| d.as_millis() as i64).unwrap_or(0);
        Self::script(ENQUEUE)
            .arg(id.to_string())
            .arg(work.tenant_id.to_string())
            .arg(&work.work_type)
            .arg(work.payload.as_slice())
            .arg(work.max_attempts as i64)
            .arg(delay_ms)
            .invoke_async::<()>(&mut conn)
            .await
            .context("redis enqueue")?;
        Ok(id)
    }

    async fn receive(
        &self,
        types: &[String],
        max: usize,
        visibility: Duration,
    ) -> Result<Vec<WorkEnvelope>> {
        let mut conn = self.client.conn().await?;
        // An empty slice means "every type" — resolve from the known-types set.
        let type_list: Vec<String> = if types.is_empty() {
            redis::cmd("SMEMBERS")
                .arg("nq:types")
                .query_async(&mut conn)
                .await
                .context("redis smembers types")?
        } else {
            types.to_vec()
        };

        let vis_ms = visibility.as_millis() as i64;
        let mut out = Vec::new();
        let mut remaining = max;
        for ty in &type_list {
            if remaining == 0 {
                break;
            }
            // Each job is 7 flat bulk-string values: id, tenant, payload,
            // attempts, max, not_before, enqueued_at.
            let flat: Vec<Vec<u8>> = Self::script(RECEIVE)
                .arg(ty.as_str())
                .arg(remaining as i64)
                .arg(vis_ms)
                .invoke_async(&mut conn)
                .await
                .context("redis receive")?;
            for chunk in flat.chunks(7) {
                if chunk.len() < 7 {
                    break;
                }
                let id = Uuid::parse_str(std::str::from_utf8(&chunk[0]).unwrap_or_default())
                    .context("redis job id")?;
                let tenant_id = Uuid::parse_str(std::str::from_utf8(&chunk[1]).unwrap_or_default())
                    .context("redis tenant id")?;
                out.push(WorkEnvelope {
                    id,
                    tenant_id,
                    work_type: ty.clone(),
                    payload: chunk[2].clone(),
                    attempts: parse_i64(&chunk[3]) as i32,
                    max_attempts: parse_i64(&chunk[4]) as i32,
                    not_before: ms_to_dt(parse_i64(&chunk[5])),
                    enqueued_at: ms_to_dt(parse_i64(&chunk[6])),
                });
                remaining = remaining.saturating_sub(1);
            }
        }
        Ok(out)
    }

    async fn ack(&self, id: Uuid) -> Result<()> {
        let mut conn = self.client.conn().await?;
        Self::script(ACK)
            .arg(id.to_string())
            .invoke_async::<()>(&mut conn)
            .await
            .context("redis ack")?;
        Ok(())
    }

    async fn nack(&self, id: Uuid, disposition: Nack) -> Result<()> {
        let mut conn = self.client.conn().await?;
        match disposition {
            Nack::Requeue => {
                Self::script(NACK_REQUEUE)
                    .arg(id.to_string())
                    .invoke_async::<()>(&mut conn)
                    .await
                    .context("redis nack requeue")?;
            }
            Nack::Dead(reason) => {
                Self::script(NACK_DEAD)
                    .arg(id.to_string())
                    .arg(reason)
                    .invoke_async::<()>(&mut conn)
                    .await
                    .context("redis nack dead")?;
            }
        }
        Ok(())
    }

    async fn extend_visibility(&self, id: Uuid, visibility: Duration) -> Result<()> {
        let mut conn = self.client.conn().await?;
        Self::script(EXTEND)
            .arg(id.to_string())
            .arg(visibility.as_millis() as i64)
            .invoke_async::<()>(&mut conn)
            .await
            .context("redis extend_visibility")?;
        Ok(())
    }

    async fn describe(&self) -> Result<QueueStats> {
        let mut conn = self.client.conn().await?;
        let (ready, in_flight, dead): (i64, i64, i64) = Self::script(DESCRIBE)
            .invoke_async(&mut conn)
            .await
            .context("redis describe")?;
        Ok(QueueStats {
            backend: self.backend().into(),
            ready,
            in_flight,
            dead,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::contract::{self, DeadInspect};

    /// Connect to the test Redis (the compose `redis` service by default; set
    /// `NOOK_TEST_REDIS_URL` to point elsewhere, e.g. `redis://localhost:6379`
    /// on the host or in CI). Skips gracefully if Redis is unreachable, so the
    /// suite is green without it — but green *with* it is what proves the
    /// contract holds on redis (AC-3).
    async fn queue() -> Option<RedisQueue> {
        let url = std::env::var("NOOK_TEST_REDIS_URL")
            .unwrap_or_else(|_| "redis://redis:6379".to_string());
        let client = RedisClient::open(&url).ok()?;
        client.ping().await.ok()?;
        Some(RedisQueue::new(client))
    }

    struct RedisDead(RedisClient);
    #[async_trait::async_trait]
    impl DeadInspect for RedisDead {
        async fn dead_count(&self, work_type: &str) -> i64 {
            let mut c = self.0.conn().await.unwrap();
            redis::cmd("LLEN")
                .arg(format!("nq:dead:{work_type}"))
                .query_async(&mut c)
                .await
                .unwrap()
        }
        async fn dead_reason(&self, id: Uuid) -> Option<String> {
            let mut c = self.0.conn().await.unwrap();
            redis::cmd("HGET")
                .arg(format!("nq:deadjob:{id}"))
                .arg("dead_reason")
                .query_async(&mut c)
                .await
                .unwrap()
        }
    }

    macro_rules! skip_or {
        () => {{
            let Some(q) = queue().await else {
                eprintln!("skipping — no reachable test Redis (NOOK_TEST_REDIS_URL)");
                return;
            };
            let dead = RedisDead(q.client.clone());
            (q, dead)
        }};
    }

    #[tokio::test]
    async fn enqueue_receive_ack_round_trip() {
        let (q, _d) = skip_or!();
        contract::enqueue_receive_ack_round_trip(&q).await;
    }
    #[tokio::test]
    async fn visibility_expiry_redelivers() {
        let (q, _d) = skip_or!();
        contract::visibility_expiry_redelivers(&q, std::time::Duration::from_millis(300)).await;
    }
    #[tokio::test]
    async fn nack_requeue_and_dead() {
        let (q, d) = skip_or!();
        contract::nack_requeue_and_dead(&q, &d).await;
    }
    #[tokio::test]
    async fn max_attempts_exhaustion_dead_letters() {
        let (q, d) = skip_or!();
        contract::max_attempts_exhaustion_dead_letters(&q, &d).await;
    }
    #[tokio::test]
    async fn receive_filters_by_type() {
        let (q, _d) = skip_or!();
        contract::receive_filters_by_type(&q).await;
    }
    #[tokio::test]
    async fn concurrent_receivers_never_double_deliver() {
        let (q, _d) = skip_or!();
        contract::concurrent_receivers_never_double_deliver(&q).await;
    }
}
