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
use nook_db::dialect::{atomic_claim, time_math, type_mapping};
use nook_db::{params, Db, DbPool};
use uuid::Uuid;

use super::{Nack, NewWork, Queue, QueueStats, WorkEnvelope};

pub struct DbQueue {
    db: DbPool,
}

#[derive(nook_db::FromDbRow)]
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
    pub fn new(db: DbPool) -> Self {
        Self { db }
    }

    /// For logs and `from_config` — which backend this is.
    pub fn backend(&self) -> &'static str {
        "database"
    }
}

/// A `Duration` as fractional seconds, the count the interval seam multiplies
/// by `interval '1 second'` (Postgres) or feeds to a `'N seconds'` modifier
/// (SQLite). Fractional on purpose — a sub-second visibility timeout must not
/// silently round to zero.
fn secs(d: Duration) -> f64 {
    d.as_secs_f64()
}

#[async_trait::async_trait]
impl Queue for DbQueue {
    async fn enqueue(&self, work: NewWork) -> Result<Uuid> {
        let id = Uuid::now_v7();
        // Both halves go through their seams now (MAIN-444). `now()` did from
        // MAIN-211; the interval stayed inline as `make_interval(secs => $6)`,
        // Postgres's named-argument call syntax, which SQLite reads as an
        // unfinished `>` — `near ">": syntax error`.
        //
        // `coalesce(…, {now})` is load-bearing and survives the swap: `$6` is
        // NULL when the work has no delay, and on BOTH engines that makes the
        // whole expression NULL (Postgres: NULL * interval; SQLite: printf
        // renders NULL as empty, and `' seconds'` is not a valid modifier, so
        // `datetime()` returns NULL). The coalesce then yields `now`, i.e.
        // "visible immediately" — the same answer as before, reached the same
        // way. `an_undelayed_enqueue_is_visible_immediately` pins it.
        let now = type_mapping(self.db.engine()).now();
        let visible_at = time_math(self.db.engine()).now_plus_scaled("$6", "1 second");
        self.db
            .exec(
                &format!(
                    "INSERT INTO work_queue \
               (id, tenant_id, work_type, payload, attempts, max_attempts, not_before, enqueued_at) \
             VALUES ($1, $2, $3, $4, 0, $5, coalesce({visible_at}, {now}), {now})",
                ),
                params![
                    id,
                    work.tenant_id,
                    &work.work_type,
                    work.payload,
                    work.max_attempts,
                    work.delay.map(secs)
                ],
            )
            .await?;
        Ok(id)
    }

    async fn receive(
        &self,
        types: &[String],
        max: usize,
        visibility: Duration,
    ) -> Result<Vec<WorkEnvelope>> {
        let mut tx = self.db.begin().await?;

        // Claim candidates. The row locks taken here are what make two
        // concurrent receivers disjoint; the atomic-claim seam supplies the
        // engine's lock-and-skip clause (Postgres: `FOR UPDATE SKIP LOCKED`),
        // so the Postgres-specific SQL lives in the trait, not inline here
        // (MAIN-199). Behavior is bit-identical.
        //
        // The type filter is BUILT rather than short-circuited (MAIN-444). It
        // used to be one statement with `({cast} IS NULL OR work_type = ANY($1))`
        // and a NULL bind for "match everything" — which reads as engine-neutral
        // and is not: SQLite still has to PARSE the `ANY`, whatever the branch
        // evaluates to, and has no such function.
        //
        // Binding the types as a LIST is the other half. nook-db rewrites
        // `= ANY($n)` into `IN ($n, $n+1, …)` on SQLite, but only for a value
        // `is_list()` accepts — and `Option<Vec<String>>` binds as
        // `OptTextArray`, an array-COLUMN bind that is deliberately never
        // expanded. So the old form reached SQLite verbatim even though the
        // rewriter existed. `DbValue::TextList` is the membership form.
        let now = type_mapping(self.db.engine()).now();
        let (type_clause, mut binds) = if types.is_empty() {
            (String::new(), Vec::new())
        } else {
            (
                " AND work_type = ANY($1)".to_string(),
                vec![nook_db::DbValue::TextList(types.to_vec())],
            )
        };
        // `max` is bound last, so its placeholder number follows the filter's
        // presence — $1 with no filter, $2 with one. On SQLite the list
        // expansion renumbers both together.
        let limit_placeholder = binds.len() + 1;
        binds.push(nook_db::DbValue::I64(Some(max as i64)));

        let claim_sql = format!(
            "SELECT id, tenant_id, work_type, payload, attempts, max_attempts, \
                    not_before, enqueued_at \
             FROM work_queue \
             WHERE (locked_until IS NULL OR locked_until <= {now}) \
               AND not_before <= {now}{type_clause} \
             ORDER BY enqueued_at \
             {lock} \
             LIMIT ${limit_placeholder}",
            lock = atomic_claim(self.db.engine()).claim_lock_clause(),
        );
        let candidates: Vec<WorkRow> = tx.query_all(&claim_sql, binds).await?;

        let mut delivered = Vec::with_capacity(candidates.len());
        for row in candidates {
            if row.attempts >= row.max_attempts {
                // Exhausted by earlier deliveries (nacked or crashed) — retire
                // it rather than deliver an attempt it can never be allowed to
                // finish.
                dead_letter(&mut tx, row.id, "max attempts exhausted").await?;
                continue;
            }
            let locked_until = time_math(self.db.engine()).now_plus_scaled("$2", "1 second");
            tx.exec(
                &format!(
                    "UPDATE work_queue \
                 SET attempts = attempts + 1, locked_until = {locked_until} \
                 WHERE id = $1",
                ),
                params![row.id, secs(visibility)],
            )
            .await?;

            let mut env = row.into_envelope();
            env.attempts += 1; // reflect this delivery
            delivered.push(env);
        }

        tx.commit().await?;
        Ok(delivered)
    }

    async fn ack(&self, id: Uuid) -> Result<()> {
        self.db
            .exec("DELETE FROM work_queue WHERE id = $1", params![id])
            .await?;
        Ok(())
    }

    async fn nack(&self, id: Uuid, disposition: Nack) -> Result<()> {
        match disposition {
            // Make it visible again immediately. Its incremented `attempts`
            // stays, so the next receive enforces exhaustion.
            Nack::Requeue => {
                self.db
                    .exec(
                        "UPDATE work_queue SET locked_until = NULL WHERE id = $1",
                        params![id],
                    )
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
        let locked_until = time_math(self.db.engine()).now_plus_scaled("$2", "1 second");
        self.db
            .exec(
                &format!("UPDATE work_queue SET locked_until = {locked_until} WHERE id = $1",),
                params![id, secs(visibility)],
            )
            .await?;
        Ok(())
    }

    async fn describe(&self) -> Result<QueueStats> {
        let now = type_mapping(self.db.engine()).now();
        let (ready, in_flight): (i64, i64) = self
            .db
            .query_one(
                &format!(
                    "SELECT \
               count(*) FILTER (WHERE (locked_until IS NULL OR locked_until <= {now}) \
                                  AND not_before <= {now}), \
               count(*) FILTER (WHERE locked_until > {now}) \
             FROM work_queue",
                ),
                params![],
            )
            .await?;
        let dead: i64 = self
            .db
            .query_scalar("SELECT count(*) FROM work_queue_dead", params![])
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
async fn dead_letter(tx: &mut nook_db::DbTx<'_>, id: Uuid, reason: &str) -> Result<()> {
    tx.exec(
        "INSERT INTO work_queue_dead \
           (id, tenant_id, work_type, payload, attempts, max_attempts, enqueued_at, reason) \
         SELECT id, tenant_id, work_type, payload, attempts, max_attempts, enqueued_at, $2 \
         FROM work_queue WHERE id = $1",
        params![id, reason],
    )
    .await?;
    tx.exec("DELETE FROM work_queue WHERE id = $1", params![id])
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
        Some(DbQueue::new(nook_db::EnginePool::from_pg(db)))
    }

    /// Reads the dead-letter table for the parameterized contract runners.
    struct DbDead(nook_db::DbPool);
    #[async_trait::async_trait]
    impl DeadInspect for DbDead {
        async fn dead_count(&self, work_type: &str) -> i64 {
            self.0
                .query_scalar::<i64>(
                    "SELECT count(*) FROM work_queue_dead WHERE work_type = $1",
                    params![work_type],
                )
                .await
                .unwrap()
        }
        async fn dead_reason(&self, id: Uuid) -> Option<String> {
            self.0
                .query_opt::<(String,)>(
                    "SELECT reason FROM work_queue_dead WHERE id = $1",
                    params![id],
                )
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
