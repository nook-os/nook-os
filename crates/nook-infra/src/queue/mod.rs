//! A durable work queue behind a provider trait — the write-side sibling of
//! `crate::cache` and `crate::storage`.
//!
//! The control plane needs to hand work to something that survives a restart:
//! a node build, a scheduled sweep, a fan-out of notifications. An in-process
//! channel loses that work when the process dies. So queueing lives behind this
//! trait, chosen from config the same way the cache and artifact store are.
//! Two backends today:
//!
//! - **database** — a Postgres table drained with `FOR UPDATE SKIP LOCKED`. The
//!   default, and all a single-Postgres deployment needs: zero extra infra.
//!
//! - **redis** — sorted sets + a job hash, every transition in an atomic Lua
//!   script (`queue::redis`, MAIN-150). A shared broker for a fleet of workers.
//!
//! - **sqs** is a RESERVED name with no implementation yet. The contract is
//!   deliberately SQS-shaped — a visibility timeout, receive/ack/nack, a
//!   dead-letter destination — so the day it lands it drops in behind this trait
//!   with nothing else changing. Selecting it today fails at boot with a pointed
//!   "not built yet" error rather than silently falling back to the database.
//!
//! ## Delivery semantics — at-least-once
//!
//! This queue is **at-least-once**, never exactly-once. A message handed to a
//! consumer is made invisible for a *visibility timeout*, not deleted; only an
//! explicit [`Queue::ack`] deletes it. If the consumer crashes, is killed, or
//! simply takes longer than the timeout, the message becomes visible again and
//! is **redelivered** — possibly while the original consumer is still working
//! on it. A consumer can buy more time with [`Queue::extend_visibility`], but
//! it can never fully close the window.
//!
//! The consequence is a hard contract: **handlers must be idempotent.**
//! Processing the same [`WorkEnvelope`] twice must be safe — key side effects
//! on the envelope `id` (or on a natural key inside the payload), and treat a
//! second delivery as a no-op. A handler that is not idempotent will
//! double-charge, double-send, or double-provision the first time a consumer is
//! slow, and that is a property of the queue, not a bug you can remove.
//!
//! Payloads are **opaque bytes**: callers serialize (JSON, by convention) and
//! the queue never looks inside, so a future redis/sqs backend stores the
//! identical bytes.

use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use uuid::Uuid;

pub mod database;
pub mod redis;

/// The provider names this build understands. `database` and `redis` are
/// implemented; `sqs` is a known, reserved name not built yet.
pub const PROVIDERS: &[&str] = &["database", "redis", "sqs"];

/// A unit of durable work as the queue sees it. The `payload` is opaque bytes
/// the queue never interprets; everything else is queue-level metadata.
#[derive(Debug, Clone)]
pub struct WorkEnvelope {
    /// Stable identity across redeliveries — the idempotency key a handler
    /// should dedupe on.
    pub id: Uuid,
    /// The tenant this work belongs to, carried so a consumer can scope its
    /// effects without re-deriving it from the payload.
    pub tenant_id: Uuid,
    /// A free-form routing string (e.g. `node.build`). Consumers select the
    /// types they handle via [`Queue::receive`].
    pub work_type: String,
    /// The caller's serialized job, opaque to the queue.
    pub payload: Vec<u8>,
    /// How many times this message has been delivered, *including the current
    /// delivery*. Starts at 1 on first receive.
    pub attempts: i32,
    /// The delivery count at which the message is dead-lettered instead of
    /// redelivered.
    pub max_attempts: i32,
    /// The message is invisible until this instant (a scheduling delay). Set at
    /// enqueue; `now` for immediate work.
    pub not_before: DateTime<Utc>,
    /// When the message was first enqueued — the drain order (oldest first).
    pub enqueued_at: DateTime<Utc>,
}

/// A new job to enqueue. `max_attempts` and `not_before` have sensible
/// defaults via [`NewWork::new`]; set them with the builder-style setters.
#[derive(Debug, Clone)]
pub struct NewWork {
    pub tenant_id: Uuid,
    pub work_type: String,
    pub payload: Vec<u8>,
    pub max_attempts: i32,
    /// A delay before the message first becomes visible. `None` means visible
    /// immediately.
    pub delay: Option<Duration>,
}

impl NewWork {
    /// A job that is visible immediately and retried up to the default number
    /// of times.
    pub fn new(tenant_id: Uuid, work_type: impl Into<String>, payload: Vec<u8>) -> Self {
        Self {
            tenant_id,
            work_type: work_type.into(),
            payload,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            delay: None,
        }
    }

    /// Cap redelivery at `n` attempts before dead-lettering.
    pub fn max_attempts(mut self, n: i32) -> Self {
        self.max_attempts = n.max(1);
        self
    }

    /// Hold the message invisible for `delay` before its first delivery.
    pub fn delay(mut self, delay: Duration) -> Self {
        self.delay = Some(delay);
        self
    }
}

/// The default redelivery cap: generous enough to ride out transient failures,
/// finite so a poison message eventually dead-letters instead of looping.
pub const DEFAULT_MAX_ATTEMPTS: i32 = 20;

/// What to do with a message a consumer could not complete.
#[derive(Debug, Clone)]
pub enum Nack {
    /// Return it to the queue for another attempt. It becomes visible
    /// immediately; on its next receive, exhaustion of `max_attempts`
    /// dead-letters it automatically.
    Requeue,
    /// Give up now, regardless of remaining attempts, and move it straight to
    /// the dead-letter table with this reason.
    Dead(String),
}

/// A point-in-time picture of the queue, for logs, the health page, and later
/// autoscaling. `ready` is the `queue.depth` gauge — work waiting to be picked
/// up right now.
#[derive(Debug, Clone)]
pub struct QueueStats {
    /// Which backend answered.
    pub backend: String,
    /// Visible messages whose `not_before` has passed and that are not locked —
    /// the drain-able depth an autoscaler cares about.
    pub ready: i64,
    /// Messages currently locked by a consumer (invisible within their
    /// visibility window).
    pub in_flight: i64,
    /// Messages that exhausted their attempts and were dead-lettered.
    pub dead: i64,
}

/// A durable, at-least-once work queue. See the module docs for the delivery
/// contract; **handlers must be idempotent**.
#[async_trait::async_trait]
pub trait Queue: Send + Sync {
    /// Durably enqueue a job. Returns its assigned id (also the idempotency
    /// key handlers dedupe on).
    async fn enqueue(&self, work: NewWork) -> Result<Uuid>;

    /// Claim up to `max` visible messages whose `work_type` is in `types` (an
    /// empty slice matches every type), making each invisible for `visibility`.
    /// Returns them with `attempts` already incremented for this delivery. A
    /// claimed message must be [`ack`](Queue::ack)ed or it will be redelivered
    /// after `visibility` elapses.
    async fn receive(
        &self,
        types: &[String],
        max: usize,
        visibility: Duration,
    ) -> Result<Vec<WorkEnvelope>>;

    /// Delete a message — the consumer finished it successfully. Acking an
    /// already-gone id is a no-op, not an error.
    async fn ack(&self, id: Uuid) -> Result<()>;

    /// Report a message the consumer could not finish. See [`Nack`].
    async fn nack(&self, id: Uuid, disposition: Nack) -> Result<()>;

    /// Push a claimed message's visibility deadline out by `visibility` from
    /// now — a long handler renewing its lease so the message is not redelivered
    /// underneath it.
    async fn extend_visibility(&self, id: Uuid, visibility: Duration) -> Result<()>;

    /// Current depth and dead-letter counts. See [`QueueStats`].
    async fn describe(&self) -> Result<QueueStats>;
}

/// Is `name` a provider this build knows by name (implemented or reserved)?
pub fn is_known_provider(name: &str) -> bool {
    PROVIDERS.contains(&name)
}

/// Validate the configured provider at boot, mirroring the cache/mail checks in
/// `Config::from_env`. `redis` and `sqs` are known but unbuilt, so each earns a
/// message pointing at the working default rather than a generic "unknown".
pub fn validate_provider(name: &str) -> Result<()> {
    match name {
        "database" | "redis" => Ok(()),
        "sqs" => anyhow::bail!(
            "NOOK_QUEUE_PROVIDER=sqs is reserved but not built yet — \
             use `database` (the default) or `redis`"
        ),
        other => anyhow::bail!(
            "NOOK_QUEUE_PROVIDER must be one of [{}] — got {other:?}",
            PROVIDERS.join(", ")
        ),
    }
}

/// Build the queue this instance is configured for.
///
/// Only `database` is constructible today; `redis`/`sqs` are rejected earlier
/// by `validate_provider` (called from `Config::from_env`), so by the time we
/// get here the provider is valid and anything but a recognised name resolves
/// to the database backend rather than panicking a boot that already validated.
pub fn from_config(cfg: &crate::config::Config, db: sqlx::PgPool) -> Box<dyn Queue> {
    // `redis` builds a lazily-connecting client (open is sync + non-connecting).
    // `Config::from_env` has already refused boot for a missing or unparseable
    // `NOOK_REDIS_URL` (a silent swap to database would split-brain work
    // routing), so the fall-throughs below are unreachable defense-in-depth, not
    // a live degradation path.
    if cfg.queue_provider == "redis" {
        match cfg.redis_url.as_deref() {
            Some(url) => match crate::redis_client::RedisClient::open(url) {
                Ok(client) => {
                    let queue = redis::RedisQueue::new(client);
                    tracing::info!(queue = %queue.backend(), "queue provider");
                    return Box::new(queue);
                }
                Err(e) => tracing::error!(
                    error = %e,
                    "NOOK_QUEUE_PROVIDER=redis but NOOK_REDIS_URL is unusable — falling back to database"
                ),
            },
            None => tracing::error!(
                "NOOK_QUEUE_PROVIDER=redis but NOOK_REDIS_URL is unset — falling back to database"
            ),
        }
    }
    let queue = database::DbQueue::new(db);
    tracing::info!(queue = %queue.backend(), "queue provider");
    Box::new(queue)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqs_is_known_but_refused_with_a_pointed_message() {
        assert!(is_known_provider("sqs"));
        let err = validate_provider("sqs").unwrap_err().to_string();
        assert!(err.contains("not built yet"), "{err}");
        assert!(
            err.contains("database"),
            "points at a working provider: {err}"
        );
    }

    #[test]
    fn database_and_redis_are_accepted_and_unknown_is_rejected() {
        assert!(validate_provider("database").is_ok());
        assert!(validate_provider("redis").is_ok());
        assert!(!is_known_provider("kafka"));
        assert!(validate_provider("kafka").is_err());
    }
}

/// The provider-agnostic queue contract, run against every backend (MAIN-150
/// AC-3). Each runner takes a `&dyn Queue` and — for the dead-letter checks,
/// which the trait does not expose per type — a `&dyn DeadInspect`. The database
/// and redis test modules are thin wrappers that build their backend and call
/// these, so both prove the identical contract, including the contention test.
#[cfg(test)]
pub(crate) mod contract {
    use super::*;
    use std::time::Duration;
    use uuid::Uuid;

    /// Backend-specific dead-letter inspection (the `Queue` trait deliberately
    /// has no per-type dead read).
    #[async_trait::async_trait]
    pub trait DeadInspect {
        async fn dead_count(&self, work_type: &str) -> i64;
        async fn dead_reason(&self, id: Uuid) -> Option<String>;
    }

    pub fn unique_type(tag: &str) -> String {
        format!("test.{tag}.{}", Uuid::now_v7())
    }
    fn work(ty: &str) -> NewWork {
        NewWork::new(Uuid::now_v7(), ty, b"{\"hello\":1}".to_vec())
    }
    const VIS: Duration = Duration::from_secs(30);

    pub async fn enqueue_receive_ack_round_trip(q: &dyn Queue) {
        let ty = unique_type("rt");
        let id = q.enqueue(work(&ty)).await.unwrap();

        let got = q.receive(std::slice::from_ref(&ty), 10, VIS).await.unwrap();
        assert_eq!(got.len(), 1, "the one enqueued message is delivered");
        assert_eq!(got[0].id, id);
        assert_eq!(got[0].attempts, 1, "first delivery counts as attempt 1");
        assert_eq!(got[0].payload, b"{\"hello\":1}");

        let again = q.receive(std::slice::from_ref(&ty), 10, VIS).await.unwrap();
        assert!(
            again.is_empty(),
            "a claimed message is not redelivered in-window"
        );

        q.ack(id).await.unwrap();
        let after = q.receive(std::slice::from_ref(&ty), 10, VIS).await.unwrap();
        assert!(after.is_empty(), "an acked message is gone for good");
    }

    pub async fn visibility_expiry_redelivers(q: &dyn Queue) {
        let ty = unique_type("vis");
        let id = q.enqueue(work(&ty)).await.unwrap();

        let first = q
            .receive(std::slice::from_ref(&ty), 10, Duration::from_millis(300))
            .await
            .unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].attempts, 1);

        let locked = q.receive(std::slice::from_ref(&ty), 10, VIS).await.unwrap();
        assert!(locked.is_empty(), "invisible until the window elapses");

        tokio::time::sleep(Duration::from_millis(600)).await;

        let second = q.receive(std::slice::from_ref(&ty), 10, VIS).await.unwrap();
        assert_eq!(second.len(), 1, "reappears after visibility expiry");
        assert_eq!(second[0].id, id);
        assert_eq!(second[0].attempts, 2, "redelivery counts as attempt 2");
        q.ack(id).await.unwrap();
    }

    pub async fn nack_requeue_and_dead(q: &dyn Queue, ins: &dyn DeadInspect) {
        // requeue → visible again
        let ty = unique_type("requeue");
        let id = q.enqueue(work(&ty)).await.unwrap();
        q.receive(std::slice::from_ref(&ty), 10, VIS).await.unwrap();
        q.nack(id, Nack::Requeue).await.unwrap();
        let back = q.receive(std::slice::from_ref(&ty), 10, VIS).await.unwrap();
        assert_eq!(back.len(), 1, "a requeued message is immediately visible");
        assert_eq!(back[0].attempts, 2);
        q.ack(id).await.unwrap();

        // dead → retired now, with the reason
        let dty = unique_type("nackdead");
        let did = q.enqueue(work(&dty)).await.unwrap();
        q.receive(std::slice::from_ref(&dty), 10, VIS)
            .await
            .unwrap();
        q.nack(did, Nack::Dead("handler said boom".into()))
            .await
            .unwrap();
        let gone = q
            .receive(std::slice::from_ref(&dty), 10, VIS)
            .await
            .unwrap();
        assert!(gone.is_empty(), "a dead-nacked message leaves the queue");
        assert_eq!(ins.dead_count(&dty).await, 1, "it lands in the dead set");
        assert_eq!(
            ins.dead_reason(did).await.as_deref(),
            Some("handler said boom")
        );
    }

    pub async fn max_attempts_exhaustion_dead_letters(q: &dyn Queue, ins: &dyn DeadInspect) {
        let ty = unique_type("exhaust");
        let id = q.enqueue(work(&ty).max_attempts(1)).await.unwrap();

        let first = q.receive(std::slice::from_ref(&ty), 10, VIS).await.unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].attempts, 1);
        q.nack(id, Nack::Requeue).await.unwrap();

        let exhausted = q.receive(std::slice::from_ref(&ty), 10, VIS).await.unwrap();
        assert!(exhausted.is_empty(), "no delivery past the attempt cap");
        assert_eq!(ins.dead_count(&ty).await, 1, "the message is dead-lettered");
        assert_eq!(
            ins.dead_reason(id).await.as_deref(),
            Some("max attempts exhausted")
        );
    }

    pub async fn receive_filters_by_type(q: &dyn Queue) {
        let a = unique_type("filterA");
        let b = unique_type("filterB");
        let ida = q.enqueue(work(&a)).await.unwrap();
        let idb = q.enqueue(work(&b)).await.unwrap();

        let only_a = q.receive(std::slice::from_ref(&a), 10, VIS).await.unwrap();
        assert_eq!(only_a.len(), 1, "only the requested type is delivered");
        assert_eq!(only_a[0].id, ida);
        assert_eq!(only_a[0].work_type, a);

        q.ack(ida).await.unwrap();
        let only_b = q.receive(std::slice::from_ref(&b), 10, VIS).await.unwrap();
        assert_eq!(only_b.len(), 1);
        assert_eq!(only_b[0].id, idb);
        q.ack(idb).await.unwrap();
    }

    pub async fn concurrent_receivers_never_double_deliver(q: &dyn Queue) {
        let ty = unique_type("contention");
        const N: usize = 12;
        for _ in 0..N {
            q.enqueue(work(&ty)).await.unwrap();
        }

        // Two receivers drain the same type at once with a long visibility. The
        // backend's atomic claim must partition the work — no id in both, N total.
        let types = vec![ty.clone()];
        let (left, right) = tokio::join!(q.receive(&types, N, VIS), q.receive(&types, N, VIS),);
        let left = left.unwrap();
        let right = right.unwrap();

        let mut ids: Vec<Uuid> = left.iter().chain(right.iter()).map(|e| e.id).collect();
        let total = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), total, "no message delivered to both receivers");
        assert_eq!(
            total, N,
            "between them the two receivers drain every message"
        );

        for id in ids {
            q.ack(id).await.unwrap();
        }
    }
}
