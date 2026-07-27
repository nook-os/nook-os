//! A durable work queue behind a provider trait — the write-side sibling of
//! `crate::cache` and `crate::storage`.
//!
//! The control plane needs to hand work to something that survives a restart:
//! a node build, a scheduled sweep, a fan-out of notifications. An in-process
//! channel loses that work when the process dies. So queueing lives behind this
//! trait, chosen from config the same way the cache and artifact store are.
//! One backend today:
//!
//! - **database** — a Postgres table drained with `FOR UPDATE SKIP LOCKED`. The
//!   default, and all a single-Postgres deployment needs: zero extra infra.
//!
//! - **redis** and **sqs** are RESERVED names with no implementation here
//!   (NG-1). The contract below is deliberately SQS-shaped — a visibility
//!   timeout, receive/ack/nack, a dead-letter destination — so the day one of
//!   those providers lands it drops in behind this trait with nothing else
//!   changing. Selecting one today fails at boot with a pointed "not built yet"
//!   error rather than silently falling back to the database, because a
//!   deployment that asked for a shared broker and silently got a single-node
//!   table would be a correctness surprise, not a convenience.
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

/// The provider names this build understands. `redis` and `sqs` are listed —
/// they are known, reserved names — but are not implemented here (NG-1).
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
        "database" => Ok(()),
        "redis" | "sqs" => anyhow::bail!(
            "NOOK_QUEUE_PROVIDER={name} is reserved but not built yet — \
             use `database` (the default) until a {name} backend ships"
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
    let _ = cfg.queue_provider.as_str();
    let queue = database::DbQueue::new(db);
    tracing::info!(queue = %queue.backend(), "queue provider");
    Box::new(queue)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_providers_are_known_but_refused_with_a_pointed_message() {
        for name in ["redis", "sqs"] {
            assert!(is_known_provider(name));
            let err = validate_provider(name).unwrap_err().to_string();
            assert!(err.contains("not built yet"), "{err}");
            assert!(
                err.contains("database"),
                "points at the working default: {err}"
            );
        }
    }

    #[test]
    fn database_is_accepted_and_unknown_is_rejected() {
        assert!(validate_provider("database").is_ok());
        assert!(!is_known_provider("kafka"));
        assert!(validate_provider("kafka").is_err());
    }
}
