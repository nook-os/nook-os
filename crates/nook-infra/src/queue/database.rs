//! The zero-infra queue backend: a Postgres table drained with `FOR UPDATE
//! SKIP LOCKED`.
//!
//! `receive` claims visible rows inside a transaction, holding a row lock on
//! each until commit. Two receivers polling at once therefore skip each other's
//! locked rows and never hand the same message to both — the load-bearing
//! guarantee (`SKIP LOCKED`). Invisibility is a `locked_until` column, not a
//! delete: a claimed row is filtered out of future receives until that instant
//! passes, so a consumer that crashes without acking has its work reappear
//! automatically (the at-least-once contract in the module docs).
//!
//! Exhaustion is enforced at receive time: a row whose `attempts` already reach
//! `max_attempts` is moved to `work_queue_dead` instead of being delivered
//! again, so a poison message loops at most `max_attempts` times whether it is
//! failing via nacks or via silent consumer crashes.

use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use super::{Nack, NewWork, Queue, QueueStats, WorkEnvelope};

pub struct DbQueue {
    db: PgPool,
}

#[derive(sqlx::FromRow)]
struct WorkRow {
    id: Uuid,
    tenant_id: Uuid,
    work_type: String,
    payload: Vec<u8>,
    attempts: i32,
    max_attempts: i32,
    not_before: DateTime<Utc>,
    enqueued_at: DateTime<Utc>,
}

impl WorkRow {
    fn into_envelope(self) -> WorkEnvelope {
        WorkEnvelope {
            id: self.id,
            tenant_id: self.tenant_id,
            work_type: self.work_type,
            payload: self.payload,
            attempts: self.attempts,
            max_attempts: self.max_attempts,
            not_before: self.not_before,
            enqueued_at: self.enqueued_at,
        }
    }
}

impl DbQueue {
    pub fn new(db: PgPool) -> Self {
        Self { db }
    }

    /// For logs and `from_config` — which backend this is.
    pub fn backend(&self) -> &'static str {
        "database"
    }
}

/// Postgres wants an interval; a `Duration` becomes fractional seconds fed to
/// `make_interval(secs => …)`.
fn secs(d: Duration) -> f64 {
    d.as_secs_f64()
}

#[async_trait::async_trait]
impl Queue for DbQueue {
    async fn enqueue(&self, work: NewWork) -> Result<Uuid> {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO work_queue \
               (id, tenant_id, work_type, payload, attempts, max_attempts, not_before, enqueued_at) \
             VALUES ($1, $2, $3, $4, 0, $5, coalesce(now() + make_interval(secs => $6), now()), now())",
        )
        .bind(id)
        .bind(work.tenant_id)
        .bind(&work.work_type)
        .bind(&work.payload)
        .bind(work.max_attempts)
        .bind(work.delay.map(secs))
        .execute(&self.db)
        .await?;
        Ok(id)
    }

    async fn receive(
        &self,
        types: &[String],
        max: usize,
        visibility: Duration,
    ) -> Result<Vec<WorkEnvelope>> {
        // An empty type slice matches everything; otherwise filter to the
        // consumer's types. `None` binds as SQL NULL, short-circuiting the AND.
        let type_filter: Option<Vec<String>> = if types.is_empty() {
            None
        } else {
            Some(types.to_vec())
        };

        let mut tx = self.db.begin().await?;

        // Claim candidates. The row locks taken here are what make two
        // concurrent receivers disjoint; SKIP LOCKED means neither waits on the
        // other, it just moves past rows already claimed.
        let candidates = sqlx::query_as::<_, WorkRow>(
            "SELECT id, tenant_id, work_type, payload, attempts, max_attempts, \
                    not_before, enqueued_at \
             FROM work_queue \
             WHERE (locked_until IS NULL OR locked_until <= now()) \
               AND not_before <= now() \
               AND ($1::text[] IS NULL OR work_type = ANY($1)) \
             ORDER BY enqueued_at \
             FOR UPDATE SKIP LOCKED \
             LIMIT $2",
        )
        .bind(&type_filter)
        .bind(max as i64)
        .fetch_all(&mut *tx)
        .await?;

        let mut delivered = Vec::with_capacity(candidates.len());
        for row in candidates {
            if row.attempts >= row.max_attempts {
                // Exhausted by earlier deliveries (nacked or crashed) — retire
                // it rather than deliver an attempt it can never be allowed to
                // finish.
                dead_letter(&mut tx, row.id, "max attempts exhausted").await?;
                continue;
            }
            sqlx::query(
                "UPDATE work_queue \
                 SET attempts = attempts + 1, locked_until = now() + make_interval(secs => $2) \
                 WHERE id = $1",
            )
            .bind(row.id)
            .bind(secs(visibility))
            .execute(&mut *tx)
            .await?;

            let mut env = row.into_envelope();
            env.attempts += 1; // reflect this delivery
            delivered.push(env);
        }

        tx.commit().await?;
        Ok(delivered)
    }

    async fn ack(&self, id: Uuid) -> Result<()> {
        sqlx::query("DELETE FROM work_queue WHERE id = $1")
            .bind(id)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    async fn nack(&self, id: Uuid, disposition: Nack) -> Result<()> {
        match disposition {
            // Make it visible again immediately. Its incremented `attempts`
            // stays, so the next receive enforces exhaustion.
            Nack::Requeue => {
                sqlx::query("UPDATE work_queue SET locked_until = NULL WHERE id = $1")
                    .bind(id)
                    .execute(&self.db)
                    .await?;
            }
            Nack::Dead(reason) => {
                let mut tx = self.db.begin().await?;
                dead_letter(&mut tx, id, &reason).await?;
                tx.commit().await?;
            }
        }
        Ok(())
    }

    async fn extend_visibility(&self, id: Uuid, visibility: Duration) -> Result<()> {
        sqlx::query(
            "UPDATE work_queue SET locked_until = now() + make_interval(secs => $2) WHERE id = $1",
        )
        .bind(id)
        .bind(secs(visibility))
        .execute(&self.db)
        .await?;
        Ok(())
    }

    async fn describe(&self) -> Result<QueueStats> {
        let (ready, in_flight): (i64, i64) = sqlx::query_as(
            "SELECT \
               count(*) FILTER (WHERE (locked_until IS NULL OR locked_until <= now()) \
                                  AND not_before <= now()), \
               count(*) FILTER (WHERE locked_until > now()) \
             FROM work_queue",
        )
        .fetch_one(&self.db)
        .await?;
        let (dead,): (i64,) = sqlx::query_as("SELECT count(*) FROM work_queue_dead")
            .fetch_one(&self.db)
            .await?;
        Ok(QueueStats {
            backend: self.backend().into(),
            ready,
            in_flight,
            dead,
        })
    }
}

/// Move a row from `work_queue` to `work_queue_dead` with `reason`, within the
/// caller's transaction. A no-op if the id is already gone.
async fn dead_letter(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: Uuid,
    reason: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO work_queue_dead \
           (id, tenant_id, work_type, payload, attempts, max_attempts, enqueued_at, reason) \
         SELECT id, tenant_id, work_type, payload, attempts, max_attempts, enqueued_at, $2 \
         FROM work_queue WHERE id = $1",
    )
    .bind(id)
    .bind(reason)
    .execute(&mut **tx)
    .await?;
    sqlx::query("DELETE FROM work_queue WHERE id = $1")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::contract::{self, DeadInspect};

    // The queue tests share the long-lived dev DB; each contract runner scopes
    // to a unique `work_type` so parallel/shared rows never collide.
    async fn queue() -> Option<DbQueue> {
        if std::env::var("NOOK_REQUIRE_DB").ok().as_deref() != Some("1") {
            return None;
        }
        let url = std::env::var("DATABASE_URL").ok()?;
        let db = sqlx::postgres::PgPoolOptions::new()
            .max_connections(5)
            .connect(&url)
            .await
            .ok()?;
        // MAIN-146 NG-2: the migration set stays owned by nook-control; this
        // crate pulls it in as a dev-dependency only to build the test schema.
        nook_control::MIGRATOR.run(&db).await.ok()?;
        Some(DbQueue::new(db))
    }

    /// Reads the dead-letter table for the parameterized contract runners.
    struct DbDead(sqlx::PgPool);
    #[async_trait::async_trait]
    impl DeadInspect for DbDead {
        async fn dead_count(&self, work_type: &str) -> i64 {
            sqlx::query_as::<_, (i64,)>("SELECT count(*) FROM work_queue_dead WHERE work_type = $1")
                .bind(work_type)
                .fetch_one(&self.0)
                .await
                .unwrap()
                .0
        }
        async fn dead_reason(&self, id: Uuid) -> Option<String> {
            sqlx::query_as::<_, (String,)>("SELECT reason FROM work_queue_dead WHERE id = $1")
                .bind(id)
                .fetch_optional(&self.0)
                .await
                .unwrap()
                .map(|r| r.0)
        }
    }

    macro_rules! skip_or {
        () => {{
            let Some(q) = queue().await else {
                eprintln!("skipping — no DATABASE_URL");
                return;
            };
            let dead = DbDead(q.db.clone());
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
