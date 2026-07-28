//! Engine seams (MAIN-189 item 3 / MAIN-198): the four Postgres-specific
//! mechanisms the dialect audit (MAIN-192) flagged as category **(c)**, put
//! behind traits so the coming query sweep can dispatch by engine instead of
//! hard-coding Postgres SQL.
//!
//! This card **scaffolds** the seams: it defines the traits and the **Postgres**
//! arm, which delegates to the exact SQL used today. Deliberately NOT here:
//! - no call site adopts them yet — that is the per-module sweep (NG-1);
//! - no SQLite arm — Postgres only (NG-2);
//! - no `DbPool` change — that is the final flip card (NG-3).
//!
//! Three of the four seams are pure SQL-fragment producers (a caller splices the
//! returned expression into its query string), so they need no pool and are
//! trivially unit-testable; the fourth, the event bus, is a live operation
//! (`pg_notify` / `LISTEN`) and takes a pool.

use futures_util::stream::{BoxStream, StreamExt};

use crate::DbPool;

// ── atomic-claim ────────────────────────────────────────────────────────────

/// How a worker claims the next available row without racing another consumer
/// (audit (c): `FOR UPDATE SKIP LOCKED`). Postgres locks-and-skips *within the
/// claim SELECT*, so the seam is the clause appended to
/// `SELECT … ORDER BY … [clause] LIMIT n` inside a transaction. SQLite (a later
/// card) will instead rely on `BEGIN IMMEDIATE` + a conditional update, so its
/// clause is empty — which is why this is a clause, not a whole query.
pub trait AtomicClaim {
    /// The row-locking clause placed after `ORDER BY` and before `LIMIT` in a
    /// claim SELECT. Postgres: `FOR UPDATE SKIP LOCKED`.
    fn claim_lock_clause(&self) -> &'static str;
}

// ── json ────────────────────────────────────────────────────────────────────

/// JSON access + containment (audit (c): `jsonb`). Each method returns an SQL
/// expression the caller splices into a query. Postgres uses the `jsonb`
/// operators/functions; SQLite (later) will use JSON1 (`json_extract`,
/// `json_each`, …).
pub trait Json {
    /// Extract a top-level field **as text**. Postgres: `{col} ->> '{key}'`.
    fn get_text(&self, col: &str, key: &str) -> String;
    /// Extract a top-level field **as json**. Postgres: `{col} -> '{key}'`.
    fn get_json(&self, col: &str, key: &str) -> String;
    /// Containment test with a **composed right-hand expression**. The caller
    /// supplies the RHS as SQL so a bound payload stays parameterized:
    /// `contains("capabilities", &cast("$1", "jsonb"))` → `capabilities @> $1::jsonb`
    /// (the exact audited form, node-supplied JSON bound to `$1`). NEVER splice
    /// untrusted JSON as a literal — bind it and `cast` the placeholder — or the
    /// sweep would open an injection. Postgres: `{col} @> {rhs}`.
    fn contains(&self, col: &str, rhs: &str) -> String;
    /// Expand a JSON array expression to a set of rows. Postgres:
    /// `jsonb_array_elements({expr})`.
    fn array_elements(&self, expr: &str) -> String;
    /// A **static** JSON literal — e.g. `'[]'::jsonb` as a COALESCE default.
    /// Postgres: `'{raw}'::jsonb`. For user-supplied JSON, bind a parameter and
    /// [`TypeMapping::cast`] the placeholder instead; never build a literal from
    /// untrusted input.
    fn literal(&self, raw: &str) -> String;
}

// ── type-mapping ────────────────────────────────────────────────────────────

/// Column types and casts that differ per engine (audit (c): `uuid` /
/// `timestamptz` columns, `::` casts, `now()`). Drives both DDL (column type
/// names) and query expressions (casts, current timestamp).
pub trait TypeMapping {
    /// DDL column type for a UUID. Postgres: `uuid`; SQLite (later): `TEXT`.
    fn uuid_column(&self) -> &'static str;
    /// DDL column type for a timestamptz. Postgres: `timestamptz`; SQLite: `TEXT`.
    fn timestamptz_column(&self) -> &'static str;
    /// DDL column type for a JSON blob. Postgres: `jsonb`; SQLite: `TEXT`.
    fn jsonb_column(&self) -> &'static str;
    /// A cast expression. Postgres: `{expr}::{ty}`; SQLite: `CAST({expr} AS {ty})`.
    fn cast(&self, expr: &str, ty: &str) -> String;
    /// The current-timestamp expression. Postgres: `now()`; SQLite:
    /// `CURRENT_TIMESTAMP`.
    fn now(&self) -> &'static str;
}

// ── event-bus ───────────────────────────────────────────────────────────────

/// Cross-instance event fan-out (audit (c): `LISTEN`/`NOTIFY`). Postgres
/// publishes with `pg_notify` and subscribes with a `LISTEN` connection; SQLite
/// (single-instance) will use an in-process broadcast — cross-instance fan-out
/// is a documented Postgres-only capability (MAIN-189). This is the one seam
/// that is an operation rather than an SQL fragment, so it holds a pool.
#[async_trait::async_trait]
pub trait EventBus: Send + Sync {
    /// Publish a small payload to a named channel (best-effort).
    async fn publish(&self, channel: &str, payload: &str) -> Result<(), sqlx::Error>;
    /// Subscribe to a channel, yielding each published payload. Boxed so the
    /// trait stays object-safe across engines.
    async fn subscribe(&self, channel: &str) -> Result<BoxStream<'static, String>, sqlx::Error>;
}

// ── Postgres arm ────────────────────────────────────────────────────────────

/// The Postgres implementation of the SQL-fragment seams. A zero-sized handle:
/// the fragments are pure, so no pool is needed here.
#[derive(Debug, Clone, Copy, Default)]
pub struct Postgres;

impl AtomicClaim for Postgres {
    fn claim_lock_clause(&self) -> &'static str {
        "FOR UPDATE SKIP LOCKED"
    }
}

impl Json for Postgres {
    fn get_text(&self, col: &str, key: &str) -> String {
        format!("{col} ->> '{key}'")
    }
    fn get_json(&self, col: &str, key: &str) -> String {
        format!("{col} -> '{key}'")
    }
    fn contains(&self, col: &str, rhs: &str) -> String {
        format!("{col} @> {rhs}")
    }
    fn array_elements(&self, expr: &str) -> String {
        format!("jsonb_array_elements({expr})")
    }
    fn literal(&self, raw: &str) -> String {
        format!("'{raw}'::jsonb")
    }
}

impl TypeMapping for Postgres {
    fn uuid_column(&self) -> &'static str {
        "uuid"
    }
    fn timestamptz_column(&self) -> &'static str {
        "timestamptz"
    }
    fn jsonb_column(&self) -> &'static str {
        "jsonb"
    }
    fn cast(&self, expr: &str, ty: &str) -> String {
        format!("{expr}::{ty}")
    }
    fn now(&self) -> &'static str {
        "now()"
    }
}

/// The Postgres event bus: `pg_notify` to publish, a `PgListener` to subscribe.
/// This is the exact mechanism `nook-chat`'s bus and the control-plane bus use
/// today; the sweep will route them through this seam.
pub struct PgEventBus {
    pool: DbPool,
}

impl PgEventBus {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl EventBus for PgEventBus {
    async fn publish(&self, channel: &str, payload: &str) -> Result<(), sqlx::Error> {
        sqlx::query("SELECT pg_notify($1, $2)")
            .bind(channel)
            .bind(payload)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn subscribe(&self, channel: &str) -> Result<BoxStream<'static, String>, sqlx::Error> {
        let mut listener = sqlx::postgres::PgListener::connect_with(&self.pool).await?;
        listener.listen(channel).await?;
        Ok(listener
            .into_stream()
            .filter_map(|r| async move { r.ok().map(|n| n.payload().to_string()) })
            .boxed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── fragment surfaces (no DB) ──────────────────────────────────────────

    #[test]
    fn atomic_claim_clause_is_pg_skip_locked() {
        assert_eq!(Postgres.claim_lock_clause(), "FOR UPDATE SKIP LOCKED");
    }

    #[test]
    fn json_fragments_are_the_jsonb_operators() {
        let pg = Postgres;
        assert_eq!(pg.get_text("caps", "runtime"), "caps ->> 'runtime'");
        assert_eq!(
            pg.get_json("caps", "runtime_auth"),
            "caps -> 'runtime_auth'"
        );
        // Containment composes an RHS expression: the audited site BINDS its
        // node-supplied payload, so `cast("$1","jsonb")` must compose to a
        // parameter, not a spliced literal (AC-3 must-fix — no injection).
        assert_eq!(
            pg.contains("capabilities", &pg.cast("$1", "jsonb")),
            "capabilities @> $1::jsonb"
        );
        // A static literal RHS still composes, for a constant (not user input).
        assert_eq!(
            pg.contains("capabilities", &pg.literal("{\"shared_operator\":true}")),
            "capabilities @> '{\"shared_operator\":true}'::jsonb"
        );
        assert_eq!(
            pg.array_elements("caps -> 'runtime_auth'"),
            "jsonb_array_elements(caps -> 'runtime_auth')"
        );
        assert_eq!(pg.literal("[]"), "'[]'::jsonb");
    }

    #[test]
    fn type_mapping_is_pg_native() {
        let pg = Postgres;
        assert_eq!(pg.uuid_column(), "uuid");
        assert_eq!(pg.timestamptz_column(), "timestamptz");
        assert_eq!(pg.jsonb_column(), "jsonb");
        assert_eq!(pg.cast("$1", "uuid"), "$1::uuid");
        assert_eq!(pg.now(), "now()");
    }

    // ── Postgres SQL is real, not plausible (needs a DB) ───────────────────
    // These connect to DATABASE_URL directly (nook-db is below nook-testkit) and
    // skip when it is absent, matching the suite's DB-optional convention.

    async fn pool() -> Option<DbPool> {
        let url = std::env::var("DATABASE_URL").ok()?;
        crate::connect(&url, 2).await.ok()
    }

    #[tokio::test]
    async fn pg_json_and_cast_expressions_execute() {
        let Some(pool) = pool().await else { return };
        let pg = Postgres;
        // The json get_text fragment runs and yields the field value.
        let expr = pg.get_text(&pg.literal("{\"a\":\"x\"}"), "a");
        let got: String = sqlx::query_scalar(&format!("SELECT {expr}"))
            .fetch_one(&pool)
            .await
            .expect("json ->> executes");
        assert_eq!(got, "x");
        // The cast fragment runs against a real uuid.
        let uuid = "00000000-0000-0000-0000-000000000001";
        let cast = pg.cast(&format!("'{uuid}'"), "uuid");
        let back: String = sqlx::query_scalar(&format!("SELECT ({cast})::text"))
            .fetch_one(&pool)
            .await
            .expect("::uuid cast executes");
        assert_eq!(back, uuid);
        // Containment with a BOUND payload — the injection-safe form the sweep
        // must adopt: the JSON travels as $1, never spliced into the SQL.
        let rhs = pg.cast("$1", "jsonb");
        let sql = format!("SELECT '{{\"a\":1,\"b\":2}}'::jsonb @> {rhs}");
        let hit: bool = sqlx::query_scalar(&sql)
            .bind("{\"a\":1}")
            .fetch_one(&pool)
            .await
            .expect("@> $1::jsonb executes with a bound payload");
        assert!(hit);
    }

    #[tokio::test]
    async fn pg_claim_clause_claims_a_row() {
        let Some(pool) = pool().await else { return };
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("CREATE TEMP TABLE claim_probe (id int) ON COMMIT DROP")
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("INSERT INTO claim_probe (id) VALUES (1), (2)")
            .execute(&mut *tx)
            .await
            .unwrap();
        // The claim query splices the seam's clause exactly where a consumer would.
        let sql = format!(
            "SELECT id FROM claim_probe ORDER BY id {} LIMIT 1",
            Postgres.claim_lock_clause()
        );
        let id: i32 = sqlx::query_scalar(&sql).fetch_one(&mut *tx).await.unwrap();
        assert_eq!(id, 1);
        tx.rollback().await.ok();
    }

    #[tokio::test]
    async fn pg_event_bus_round_trips_a_payload() {
        let Some(pool) = pool().await else { return };
        let bus = PgEventBus::new(pool);
        let channel = "nook_db_test_bus";
        let mut sub = bus.subscribe(channel).await.expect("subscribe");
        // Give LISTEN a beat to register before we NOTIFY.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        bus.publish(channel, "hello").await.expect("publish");
        let got = tokio::time::timeout(std::time::Duration::from_secs(3), sub.next())
            .await
            .expect("payload arrives before the timeout");
        assert_eq!(got.as_deref(), Some("hello"));
    }
}
