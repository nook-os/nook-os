//! The SQLite track's timestamp form, stated once (MAIN-442).
//!
//! SQLite has no timestamp type: a `timestamptz` column is TEXT, and TEXT
//! compares byte by byte. So two sides that render the same instant differently
//! do not merely look different — they stop being equal and stop being ordered,
//! silently, because both still DECODE fine (sqlx's reader accepts a dozen
//! shapes). That is the defect this module exists to make impossible:
//!
//! - `CURRENT_TIMESTAMP` wrote `2026-08-06 13:28:36`, while a bound
//!   `DateTime<Utc>` encodes as RFC 3339 (`2026-08-06T13:28:36.411979+00:00`).
//!   The optimistic-concurrency guard `AND updated_at = $11` therefore matched
//!   **0 rows on every guarded update**, so a matching version reported
//!   Conflict — with nothing in any log to say why.
//! - `CURRENT_TIMESTAMP` has second resolution, so two rows written in the same
//!   second were byte-identical and a strict `>` could never separate them.
//!   `chat_messages.created_at > cursor` counted a message posted in the
//!   cursor's second as already read.
//!
//! One form, named here, is what both halves now derive from: the SQL that
//! WRITES a timestamp ([`NOW_SQL`], [`COLUMN_DEFAULT_SQL`]) and the binder that
//! sends one ([`render`]). `migrations_sqlite/0055_sqlite_timestamp_form.sql`
//! puts the same form on every column default, and
//! `nook-control/tests/it/sqlite_boot.rs` checks the migrated schema against
//! [`COLUMN_DEFAULT_SQL`] — so a future table declaring
//! `DEFAULT CURRENT_TIMESTAMP` out of habit reddens CI instead of quietly
//! reintroducing this.
//!
//! **Postgres is untouched.** It has a real `timestamptz` and compares
//! instants, not text, so none of this applies there and none of it runs there.
//!
//! ## Milliseconds, and why not more
//!
//! Both halves must render the SAME WIDTH or equality breaks again, so the
//! width is the narrower half's: SQLite's clock is milliseconds
//! (`sqlite3OsCurrentTimeInt64`) and `strftime`'s `%f` prints exactly three
//! fractional digits. A bound value is therefore truncated to milliseconds on
//! this engine — Postgres keeps its microseconds — and two rows written inside
//! one millisecond can still tie. That is the engine's floor, not a choice
//! here; the alternative was to keep second resolution, which is 1000× worse.

/// The canonical form, as a macro so ONE spelling reaches every place that
/// needs it as a *literal* — `concat!` takes literals, not consts, and a
/// `format!`-built string cannot be a `&'static str` for the dialect seam.
///
/// SQLite's `%f` is "SS.SSS": it prints the seconds AND the fractional part, so
/// there is deliberately no `%S` before it.
macro_rules! timestamp_format {
    () => {
        "%Y-%m-%d %H:%M:%f"
    };
}

/// The canonical form in SQLite's `strftime` spelling — `2026-08-06 13:28:36.411`.
pub const TIMESTAMP_FORMAT: &str = timestamp_format!();

/// The same form in chrono's spelling, for the binder.
///
/// chrono has no equivalent of SQLite's combined `%f`; the pair is
/// `%S%.3f`, where `%.3f` is a dot plus exactly three digits (chrono truncates,
/// as SQLite does). `the_two_spellings_are_one_form` pins the translation, and
/// `a_bound_instant_matches_the_engines_rendering_of_it` proves the two agree
/// against a real database rather than on paper.
const CHRONO_FORMAT: &str = "%Y-%m-%d %H:%M:%S%.3f";

/// `now()` for the SQLite arm: the current instant in the canonical form.
pub const NOW_SQL: &str = concat!("strftime('", timestamp_format!(), "','now')");

/// The canonical form as a **column default**: parenthesised, because that is
/// how SQLite requires an expression default to be written. This is the
/// spelling DDL uses. (What comes back out of `pragma_table_info` is the
/// parentheses stripped — i.e. [`NOW_SQL`] — which is what the schema guard in
/// `nook-control/tests/it/sqlite_boot.rs` compares against.)
pub const COLUMN_DEFAULT_SQL: &str = concat!("(strftime('", timestamp_format!(), "','now'))");

/// `now()` shifted by a SQLite date modifier — `'+14 days'`, or a composed
/// expression such as `printf('%s seconds', -($1))`.
///
/// The modifier is spliced as SQL, so it is a **static, code-controlled**
/// fragment or a `printf` over a bound parameter — never user input. This is
/// [`crate::TimeMath`]'s SQLite arm in one place, so the interval forms cannot
/// drift from the plain [`NOW_SQL`] they are compared against.
pub fn now_shifted_sql(modifier: &str) -> String {
    format!("strftime('{TIMESTAMP_FORMAT}','now', {modifier})")
}

/// Render an instant the way the database writes one.
///
/// Called for every `Timestamptz` bind on the SQLite arm, which is what makes a
/// bound value comparable — by `=` and by `>` — with a value the column's own
/// default wrote.
pub fn render(dt: chrono::DateTime<chrono::Utc>) -> String {
    dt.format(CHRONO_FORMAT).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// The two spellings are one form: SQLite's `%f` is chrono's `%S%.3f`, and
    /// nothing else differs. Written as the translation rather than as a second
    /// literal, so changing the form in one place cannot leave the other behind.
    #[test]
    fn the_two_spellings_are_one_form() {
        assert_eq!(TIMESTAMP_FORMAT.replace("%f", "%S%.3f"), CHRONO_FORMAT);
    }

    #[test]
    fn render_is_the_canonical_shape() {
        let dt = chrono::Utc
            .with_ymd_and_hms(2026, 8, 6, 13, 28, 36)
            .unwrap()
            .checked_add_signed(chrono::Duration::microseconds(411_979))
            .unwrap();
        // Milliseconds, truncated — not rounded up to `.412`.
        assert_eq!(render(dt), "2026-08-06 13:28:36.411");
    }

    /// A whole second still carries its three digits. Fixed width is the whole
    /// point: `13:28:36` and `13:28:36.000` are different bytes, and one of
    /// them would never compare equal to what the column default wrote.
    #[test]
    fn a_whole_second_keeps_its_fraction() {
        let dt = chrono::Utc
            .with_ymd_and_hms(2026, 8, 6, 13, 28, 36)
            .unwrap();
        assert_eq!(render(dt), "2026-08-06 13:28:36.000");
    }

    #[test]
    fn the_sql_forms_carry_the_one_format() {
        assert_eq!(NOW_SQL, "strftime('%Y-%m-%d %H:%M:%f','now')");
        assert_eq!(COLUMN_DEFAULT_SQL, "(strftime('%Y-%m-%d %H:%M:%f','now'))");
        assert_eq!(
            now_shifted_sql("'+14 days'"),
            "strftime('%Y-%m-%d %H:%M:%f','now', '+14 days')"
        );
    }

    // ── against a real database ────────────────────────────────────────────
    //
    // The unit tests above pin each half on its own, which is exactly the
    // "two independent code paths agreeing by luck" this module exists to
    // replace. These execute both halves against SQLite itself. In memory: the
    // claim is about the engine's date functions and sqlx's encoder, neither of
    // which cares where the file is.

    async fn stamped_table() -> crate::DbPool {
        use crate::Db;
        let db = crate::connect("sqlite::memory:", 1)
            .await
            .expect("an in-memory sqlite database");
        db.exec(
            &format!("CREATE TABLE t (id INTEGER PRIMARY KEY, at TEXT NOT NULL DEFAULT {COLUMN_DEFAULT_SQL})"),
            vec![],
        )
        .await
        .expect("the canonical default is a default sqlite accepts");
        db
    }

    /// AC-1, at the seam it broke: a value the COLUMN wrote, read into Rust and
    /// bound straight back, finds its own row. Before this, `updated_at = $1`
    /// matched nothing — silently, because the read half was always fine.
    #[tokio::test]
    async fn a_stored_instant_bound_back_finds_its_own_row() {
        use crate::Db;
        let db = stamped_table().await;
        db.exec("INSERT INTO t (id) VALUES (1)", vec![])
            .await
            .expect("insert");

        let stored: chrono::DateTime<chrono::Utc> = db
            .query_scalar("SELECT at FROM t WHERE id = 1", vec![])
            .await
            .expect("read it back");

        let found: i64 = db
            .query_scalar(
                "SELECT count(*) FROM t WHERE at = $1",
                crate::params![stored],
            )
            .await
            .expect("compare");
        assert_eq!(found, 1, "the round-tripped instant must equal itself");
    }

    /// The same claim from the other direction: an instant BOUND in is found by
    /// the SQL side's own rendering of it.
    #[tokio::test]
    async fn a_bound_instant_matches_the_engines_rendering_of_it() {
        use crate::Db;
        let db = stamped_table().await;
        let ts = chrono::Utc::now();
        db.exec("INSERT INTO t (id, at) VALUES (1, $1)", crate::params![ts])
            .await
            .expect("insert");

        let stored: String = db
            .query_scalar("SELECT at FROM t WHERE id = 1", vec![])
            .await
            .expect("read the raw text");
        assert_eq!(
            stored,
            db.query_scalar::<String>(
                &format!("SELECT strftime('{TIMESTAMP_FORMAT}', $1)"),
                crate::params![ts]
            )
            .await
            .expect("the engine's own rendering"),
            "the binder and the database render one instant one way"
        );
    }

    /// AC-2: rows written in succession are strictly ordered. `CURRENT_TIMESTAMP`
    /// made this impossible below one second — two rows were byte-identical, and
    /// no strict `>` can separate equal values.
    #[tokio::test]
    async fn two_rows_written_in_succession_are_strictly_ordered() {
        use crate::Db;
        let db = stamped_table().await;
        db.exec("INSERT INTO t (id) VALUES (1)", vec![])
            .await
            .expect("first");
        // Past the engine's own resolution — a millisecond is SQLite's floor,
        // so this asserts what the form can deliver and not more.
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        db.exec("INSERT INTO t (id) VALUES (2)", vec![])
            .await
            .expect("second");

        let ordered: i64 = db
            .query_scalar("SELECT count(*) FROM t a, t b WHERE a.at > b.at", vec![])
            .await
            .expect("order them");
        assert_eq!(ordered, 1, "the later row must sort after the earlier one");
    }
}
