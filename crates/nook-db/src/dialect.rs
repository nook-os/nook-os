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
    /// Set a value at a JSON path, creating missing keys — for an in-place
    /// `UPDATE … SET {col} = <this>`. The `value` is a **composed** SQL
    /// expression so a bound payload stays parameterized:
    /// `set("capabilities", "{runtime_auth}", "$2")` →
    /// `jsonb_set(capabilities, '{runtime_auth}', $2, true)` (the audited
    /// capabilities-merge site, node-supplied JSON bound to `$2`). The `path`
    /// is a static, code-controlled key path, never user input — spliced like
    /// [`Json::get_text`]'s key. Postgres: `jsonb_set({col}, '{path}', {value}, true)`.
    fn set(&self, col: &str, path: &str, value: &str) -> String;
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

// ── time-math (interval arithmetic) ──────────────────────────────────────────

/// `now() ± interval …` arithmetic (audit (d): INTERVAL arithmetic, MAIN-204).
/// The WHOLE expression is abstracted, not just the interval literal, because a
/// second engine expresses it entirely differently — SQLite has no `+ interval`
/// operator and would emit `datetime('now', '+14 days')`. Postgres delegates to
/// `now() ± interval '…'`. The `spec`/`unit` strings are **static**,
/// code-controlled interval literals (e.g. `"14 days"`), never user input —
/// spliced like [`Json::get_text`]'s key; bind a parameter for any variable
/// amount (see [`TimeMath::now_minus_scaled`]).
pub trait TimeMath {
    /// `now()` plus a fixed interval. Postgres: `now() + interval '{spec}'`.
    fn now_plus(&self, spec: &str) -> String;
    /// `now()` minus a fixed interval. Postgres: `now() - interval '{spec}'`.
    fn now_minus(&self, spec: &str) -> String;
    /// `now()` minus a **bound** multiple of a unit interval — `count` is a
    /// composed SQL expression (bind a parameter, e.g. `"$1::bigint"`) so the
    /// variable amount stays parameterized. Postgres:
    /// `now() - ({count} * interval '{unit}')`.
    fn now_minus_scaled(&self, count: &str, unit: &str) -> String;
}

// ── ci-match (case-insensitive matching) ─────────────────────────────────────

/// Case-insensitive matching (audit (b): `ILIKE`, MAIN-203). Postgres has the
/// `ILIKE` operator; SQLite has no such operator and instead relies on `LIKE`
/// with a case-insensitive collation (`COLLATE NOCASE`) — so the *whole* match
/// expression differs per engine, not just an operator keyword, which is why
/// this is a seam rather than a mechanical `ILIKE`→`LIKE` string swap.
///
/// **Adoption pattern** (how a per-crate sweep card replaces an inline `ILIKE`):
/// an inline `col ILIKE <pattern>` becomes `{}` in a `format!`, filled by
/// `engine.ci_match("col", "<pattern>")`. The `pattern` is a **composed** SQL
/// expression the caller supplies, so a bound search term stays parameterized —
/// e.g. `ci_match("n.title", "'%' || $2 || '%'")` reproduces
/// `n.title ILIKE '%' || $2 || '%'` on Postgres. Never splice an untrusted term
/// into `pattern` as a literal; bind it and wrap the placeholder.
pub trait CiMatch {
    /// A case-insensitive match of `col` against a composed `pattern` expression.
    /// Postgres: `{col} ILIKE {pattern}`; SQLite: `{col} LIKE {pattern} COLLATE NOCASE`.
    fn ci_match(&self, col: &str, pattern: &str) -> String;
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
    fn set(&self, col: &str, path: &str, value: &str) -> String {
        format!("jsonb_set({col}, '{path}', {value}, true)")
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

impl TimeMath for Postgres {
    fn now_plus(&self, spec: &str) -> String {
        format!("now() + interval '{spec}'")
    }
    fn now_minus(&self, spec: &str) -> String {
        format!("now() - interval '{spec}'")
    }
    fn now_minus_scaled(&self, count: &str, unit: &str) -> String {
        format!("now() - ({count} * interval '{unit}')")
    }
}

impl CiMatch for Postgres {
    fn ci_match(&self, col: &str, pattern: &str) -> String {
        format!("{col} ILIKE {pattern}")
    }
}

/// The SQLite fragment arm. A zero-sized handle like [`Postgres`]; only the
/// seams SQLite needs *today* are implemented here (currently [`CiMatch`], from
/// MAIN-203) — the rest arrive with the engine's own cards. SQLite has no
/// `ILIKE`, so case-insensitivity rides `LIKE … COLLATE NOCASE`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Sqlite;

impl CiMatch for Sqlite {
    fn ci_match(&self, col: &str, pattern: &str) -> String {
        format!("{col} LIKE {pattern} COLLATE NOCASE")
    }
}

impl TypeMapping for Sqlite {
    // SQLite has no uuid, timestamptz or jsonb: all three are TEXT, which is
    // what `migrations_sqlite/0001_init.sql` declares (MAIN-236) and what the
    // dialect audit's map prescribes. Its dynamic typing makes this faithful —
    // the values round-trip as the same strings Postgres renders.
    fn uuid_column(&self) -> &'static str {
        "TEXT"
    }
    fn timestamptz_column(&self) -> &'static str {
        "TEXT"
    }
    fn jsonb_column(&self) -> &'static str {
        "TEXT"
    }
    fn cast(&self, expr: &str, ty: &str) -> String {
        format!("CAST({expr} AS {ty})")
    }
    fn now(&self) -> &'static str {
        "CURRENT_TIMESTAMP"
    }
}

impl TimeMath for Sqlite {
    // `datetime('now', ±spec)` is SQLite's interval arithmetic. Postgres spells
    // an interval `'10 years'`; SQLite wants `'+10 years'` / `'-10 years'`, so
    // the sign is prefixed here rather than pushed onto every call site.
    fn now_plus(&self, spec: &str) -> String {
        format!("datetime('now', '+{spec}')")
    }
    fn now_minus(&self, spec: &str) -> String {
        format!("datetime('now', '-{spec}')")
    }
    fn now_minus_scaled(&self, count: &str, unit: &str) -> String {
        // `-N unit` has to be built at runtime because the count is a bind or
        // an expression: `printf` composes the modifier string in-database.
        format!("datetime('now', printf('-%s {unit}', {count}))")
    }
}

impl Json for Sqlite {
    // SQLite's JSON1 functions. `->>` exists in modern SQLite too, but
    // json_extract is the portable spelling and is what the seed path needs.
    fn get_text(&self, col: &str, key: &str) -> String {
        format!("json_extract({col}, '$.{key}')")
    }
    fn get_json(&self, col: &str, key: &str) -> String {
        format!("json_extract({col}, '$.{key}')")
    }
    fn contains(&self, col: &str, rhs: &str) -> String {
        // Postgres `@>` is containment; SQLite has no operator for it. This is
        // the honest approximation for the one shape the codebase uses — a flat
        // object of scalar keys — and it is why the sweep (MAIN-189 item 4) has
        // to look at each `contains` call rather than trusting the seam.
        format!("EXISTS (SELECT 1 FROM json_each({rhs}) k WHERE json_extract({col}, '$.' || k.key) = k.value)")
    }
    fn array_elements(&self, expr: &str) -> String {
        format!("json_each({expr})")
    }
    fn literal(&self, raw: &str) -> String {
        format!("json('{raw}')")
    }
    fn set(&self, col: &str, path: &str, value: &str) -> String {
        format!("json_set({col}, '{path}', {value})")
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
            .execute(self.pool.pg())
            .await?;
        Ok(())
    }

    async fn subscribe(&self, channel: &str) -> Result<BoxStream<'static, String>, sqlx::Error> {
        let mut listener = sqlx::postgres::PgListener::connect_with(self.pool.pg()).await?;
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
        // set() composes a BOUND value into jsonb_set — the ws/node.rs
        // capabilities merge, node JSON bound to $2, create_missing = true.
        assert_eq!(
            pg.set("capabilities", "{runtime_auth}", "$2"),
            "jsonb_set(capabilities, '{runtime_auth}', $2, true)"
        );
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

    #[test]
    fn time_math_is_pg_interval_arithmetic() {
        let pg = Postgres;
        // The exact audited forms (MAIN-204), byte-for-byte.
        assert_eq!(pg.now_plus("14 days"), "now() + interval '14 days'");
        assert_eq!(pg.now_minus("60 seconds"), "now() - interval '60 seconds'");
        // A bound multiplier stays parameterized (the jobs.rs reaper cutoff).
        assert_eq!(
            pg.now_minus_scaled("$1::bigint", "1 second"),
            "now() - ($1::bigint * interval '1 second')"
        );
    }

    #[test]
    fn ci_match_differs_per_engine() {
        // The composed-pattern form the sweep adopts (bound term stays in $2).
        let pattern = "'%' || $2 || '%'";
        // Postgres delegates to ILIKE — byte-identical to today's inline usage.
        assert_eq!(
            Postgres.ci_match("n.title", pattern),
            "n.title ILIKE '%' || $2 || '%'"
        );
        // SQLite has no ILIKE: LIKE + a case-insensitive collation.
        assert_eq!(
            Sqlite.ci_match("n.title", pattern),
            "n.title LIKE '%' || $2 || '%' COLLATE NOCASE"
        );
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
            .fetch_one(pool.pg())
            .await
            .expect("json ->> executes");
        assert_eq!(got, "x");
        // The cast fragment runs against a real uuid.
        let uuid = "00000000-0000-0000-0000-000000000001";
        let cast = pg.cast(&format!("'{uuid}'"), "uuid");
        let back: String = sqlx::query_scalar(&format!("SELECT ({cast})::text"))
            .fetch_one(pool.pg())
            .await
            .expect("::uuid cast executes");
        assert_eq!(back, uuid);
        // Containment with a BOUND payload — the injection-safe form the sweep
        // must adopt: the JSON travels as $1, never spliced into the SQL.
        let rhs = pg.cast("$1", "jsonb");
        let sql = format!("SELECT '{{\"a\":1,\"b\":2}}'::jsonb @> {rhs}");
        let hit: bool = sqlx::query_scalar(&sql)
            .bind("{\"a\":1}")
            .fetch_one(pool.pg())
            .await
            .expect("@> $1::jsonb executes with a bound payload");
        assert!(hit);
        // set() runs: jsonb_set merges a BOUND value at a path, create_missing.
        let set = pg.set(&pg.literal("{\"a\":1}"), "{b}", &pg.cast("$1", "jsonb"));
        let merged: String = sqlx::query_scalar(&format!("SELECT ({set})::text"))
            .bind("2")
            .fetch_one(pool.pg())
            .await
            .expect("jsonb_set executes with a bound value");
        assert_eq!(merged, "{\"a\": 1, \"b\": 2}");
    }

    #[tokio::test]
    async fn pg_interval_expressions_execute() {
        let Some(pool) = pool().await else { return };
        let pg = Postgres;
        // A fixed positive offset lands in the future; a negative one in the past.
        let (future_ok, past_ok): (bool, bool) = sqlx::query_as(&format!(
            "SELECT ({plus}) > now(), ({minus}) < now()",
            plus = pg.now_plus("1 hour"),
            minus = pg.now_minus("1 hour"),
        ))
        .fetch_one(pool.pg())
        .await
        .expect("now() ± interval executes");
        assert!(future_ok && past_ok);
        // The bound-multiplier form runs and scales by the parameter: 3600s ago
        // is ~1 hour in the past.
        let scaled_ok: bool = sqlx::query_scalar(&format!(
            "SELECT ({cutoff}) < now()",
            cutoff = pg.now_minus_scaled("$1::bigint", "1 second"),
        ))
        .bind(3600_i64)
        .fetch_one(pool.pg())
        .await
        .expect("now() - ($1 * interval) executes with a bound multiplier");
        assert!(scaled_ok);
    }

    #[tokio::test]
    async fn pg_ci_match_is_case_insensitive() {
        let Some(pool) = pool().await else { return };
        // The exact form the exemplar adopts: 'Work' matched by a lowercase
        // bound term through the ILIKE the Postgres arm emits.
        let expr = Postgres.ci_match("'Work'", "'%' || $1 || '%'");
        let hit: bool = sqlx::query_scalar(&format!("SELECT {expr}"))
            .bind("work")
            .fetch_one(pool.pg())
            .await
            .expect("ci_match executes");
        assert!(hit, "ILIKE matches across case");
    }

    #[tokio::test]
    async fn pg_claim_clause_claims_a_row() {
        let Some(pool) = pool().await else { return };
        let mut tx = pool.pg().begin().await.unwrap();
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

// ── Runtime dialect selection (MAIN-196) ────────────────────────────────────
//
// Until now every call site named `Postgres` statically, which is correct while
// there is one engine and a lie the moment there are two. These pick the arm
// from the pool's engine. They return trait objects because the fragments are
// pure `&self -> String` methods — no state, no lifetimes to thread.
//
// The sweep (MAIN-189 item 4) converts the query surface site by site; the boot,
// seed and bus paths go first because nothing can start without them.

/// The [`TypeMapping`] arm for `engine`.
pub fn type_mapping(engine: crate::Engine) -> &'static dyn TypeMapping {
    match engine {
        crate::Engine::Postgres => &Postgres,
        crate::Engine::Sqlite => &Sqlite,
    }
}

/// The [`TimeMath`] arm for `engine`.
pub fn time_math(engine: crate::Engine) -> &'static dyn TimeMath {
    match engine {
        crate::Engine::Postgres => &Postgres,
        crate::Engine::Sqlite => &Sqlite,
    }
}

/// The [`Json`] arm for `engine`.
pub fn json(engine: crate::Engine) -> &'static dyn Json {
    match engine {
        crate::Engine::Postgres => &Postgres,
        crate::Engine::Sqlite => &Sqlite,
    }
}

#[cfg(test)]
mod sqlite_dialect_tests {
    use super::*;
    use crate::Engine;

    #[test]
    fn sqlite_speaks_its_own_now_and_cast() {
        let t = type_mapping(Engine::Sqlite);
        assert_eq!(t.now(), "CURRENT_TIMESTAMP");
        assert_eq!(t.cast("$1", "text"), "CAST($1 AS text)");
        // The three category-(c) column types collapse to TEXT, matching the
        // committed SQLite DDL.
        assert_eq!(t.uuid_column(), "TEXT");
        assert_eq!(t.timestamptz_column(), "TEXT");
        assert_eq!(t.jsonb_column(), "TEXT");
    }

    #[test]
    fn postgres_is_untouched_by_the_selection() {
        let t = type_mapping(Engine::Postgres);
        assert_eq!(t.now(), "now()");
        assert_eq!(t.cast("$1", "uuid"), "$1::uuid");
        assert_eq!(t.uuid_column(), "uuid");
        let m = time_math(Engine::Postgres);
        assert_eq!(m.now_plus("10 years"), "now() + interval '10 years'");
    }

    #[test]
    fn sqlite_interval_arithmetic_carries_its_own_sign() {
        let m = time_math(Engine::Sqlite);
        // Postgres spells the interval unsigned and adds; SQLite needs the sign
        // inside the modifier string.
        assert_eq!(m.now_plus("10 years"), "datetime('now', '+10 years')");
        assert_eq!(m.now_minus("7 days"), "datetime('now', '-7 days')");
        assert!(m.now_minus_scaled("$1", "second").contains("printf"));
    }
}
