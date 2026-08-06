//! Engine-neutral row mapping (MAIN-327).
//!
//! Before this, [`crate::Db`]'s fetchers were bound
//! `T: FromRow<PgRow> + FromRow<SqliteRow>`, so **the row-mapping layer was
//! sqlx by contract**. Every DTO that travelled through the dispatch pool had
//! to name sqlx to satisfy it — that is why `nook-types` derived
//! `sqlx::FromRow` forty-odd times and `repo/notifications` hand-wrote it. Not
//! leakage anyone forgot to clean up: a requirement of the adapter's own trait.
//!
//! The replacement is two traits and one enum:
//!
//! - [`DbRow`] — one row from whichever engine produced it.
//! - [`FromDbColumn`] — a type readable out of ONE column. Implemented here for
//!   the scalars, and, crucially, implementable **outside** this crate for
//!   domain types: `nook-types` implements it for its UUID newtypes exactly as
//!   it already implements [`crate::IntoDbValue`], which is what lets those
//!   newtypes cross the boundary in both directions without naming sqlx.
//! - [`FromDbRow`] — a type built from a whole row. Derive it with
//!   `#[derive(nook_db::FromDbRow)]`.
//!
//! sqlx stays, but it stays *here*: `try_get` is called from inside this file
//! and nowhere else in the workspace.

use crate::DbError;

/// One row, from whichever engine produced it.
///
/// Deliberately an enum rather than a trait object: the two arms decode
/// differently for `text[]` (see [`FromDbColumn`] for `Vec<String>`), and a
/// boxed row would have to erase exactly the distinction that matters.
///
/// No `Debug`: sqlx's `SqliteRow` has none, and a row's contents are the last
/// thing that should end up in a log line by accident.
pub enum DbRow {
    Pg(sqlx::postgres::PgRow),
    Sqlite(sqlx::sqlite::SqliteRow),
}

impl DbRow {
    /// Read one column by name.
    ///
    /// Errors carry sqlx's own `ColumnNotFound` / `ColumnDecode`, so a mapping
    /// mistake still names the column it was reading — which matters more here
    /// than in a derive, because a wrong name is a runtime failure, not a
    /// compile error.
    pub fn get<T: FromDbColumn>(&self, name: &str) -> Result<T, DbError> {
        T::from_db_column(self, name)
    }

    /// Read one column by POSITION. Only tuples use this: a `SELECT count(*), …`
    /// has no column names to map by, so positional access is not a convenience
    /// here, it is the only thing that can read that row at all.
    pub fn get_at<T: FromDbColumn>(&self, index: usize) -> Result<T, DbError> {
        T::from_db_column_at(self, index)
    }

    /// Whether the column is SQL NULL. Backs the blanket `Option<T>` mapping:
    /// asking the row directly is what lets a DOMAIN type — which knows nothing
    /// about NULL — be wrapped in `Option` without writing a second impl for it.
    pub fn is_null(&self, name: &str) -> Result<bool, DbError> {
        use sqlx::{Row, ValueRef};
        Ok(match self {
            DbRow::Pg(r) => r.try_get_raw(name)?.is_null(),
            DbRow::Sqlite(r) => r.try_get_raw(name)?.is_null(),
        })
    }

    /// [`is_null`](Self::is_null) by position.
    pub fn is_null_at(&self, index: usize) -> Result<bool, DbError> {
        use sqlx::{Row, ValueRef};
        Ok(match self {
            DbRow::Pg(r) => r.try_get_raw(index)?.is_null(),
            DbRow::Sqlite(r) => r.try_get_raw(index)?.is_null(),
        })
    }

    /// Read one column, falling back to `Default` when the query did not select
    /// it. Backs `#[db(default)]`, for DTOs whose SELECT list varies.
    pub fn get_or_default<T: FromDbColumn + Default>(&self, name: &str) -> Result<T, DbError> {
        match T::from_db_column(self, name) {
            Err(e) if is_missing_column(&e) => Ok(T::default()),
            other => other,
        }
    }
}

/// Whether a failure is "that column was not in the result", as opposed to a
/// real decode error. Only the former may fall back to a default: swallowing a
/// decode failure would turn a type mismatch into a silent empty value.
fn is_missing_column(e: &DbError) -> bool {
    matches!(e, DbError::Query(sqlx::Error::ColumnNotFound(_)))
}

/// A type that can be read out of one column of a [`DbRow`].
///
/// Implementable outside this crate — that is the point. A domain newtype
/// implements it by delegating to the primitive it wraps, and so never names
/// sqlx:
///
/// ```ignore
/// impl nook_db::FromDbColumn for TenantId {
///     fn from_db_column(row: &nook_db::DbRow, name: &str) -> Result<Self, nook_db::DbError> {
///         Ok(TenantId(row.get::<uuid::Uuid>(name)?))
///     }
/// }
/// ```
pub trait FromDbColumn: Sized {
    fn from_db_column(row: &DbRow, name: &str) -> Result<Self, DbError>;

    /// The same read, by position. Separate from the by-name read because the
    /// two are genuinely different lookups, and only tuple rows use this one.
    fn from_db_column_at(row: &DbRow, index: usize) -> Result<Self, DbError>;
}

/// A type built from a whole [`DbRow`]. Derive it:
/// `#[derive(nook_db::FromDbRow)]`.
///
/// Field attributes, mirroring the `#[sqlx(…)]` set this replaces:
/// `#[db(rename = "type")]`, `#[db(skip)]`, `#[db(default)]`.
pub trait FromDbRow: Sized {
    fn from_db_row(row: &DbRow) -> Result<Self, DbError>;
}

/// Column types that decode identically on both engines: hand the name to
/// sqlx's `try_get` on whichever arm we hold.
macro_rules! column_via_sqlx {
    ($($t:ty),+ $(,)?) => {
        $(
            impl FromDbColumn for $t {
                fn from_db_column(row: &DbRow, name: &str) -> Result<Self, DbError> {
                    use sqlx::Row;
                    match row {
                        DbRow::Pg(r) => r.try_get::<$t, _>(name).map_err(Into::into),
                        DbRow::Sqlite(r) => r.try_get::<$t, _>(name).map_err(Into::into),
                    }
                }
                fn from_db_column_at(row: &DbRow, index: usize) -> Result<Self, DbError> {
                    use sqlx::Row;
                    match row {
                        DbRow::Pg(r) => r.try_get::<$t, _>(index).map_err(Into::into),
                        DbRow::Sqlite(r) => r.try_get::<$t, _>(index).map_err(Into::into),
                    }
                }
            }
        )+
    };
}

column_via_sqlx!(
    bool,
    i16,
    i32,
    i64,
    f64,
    String,
    Vec<u8>,
    chrono::DateTime<chrono::Utc>,
    serde_json::Value,
);

// ── uuid — the one type whose SQLite storage is not uniform (MAIN-437) ──────
//
// Every uuid this workspace WRITES arrives as a 16-byte blob, because
// `DbValue::Uuid` binds it that way (`pool.rs`). One column is not written by a
// bind: `tenants.org_id` has a 36-char text DEFAULT in the SQLite `0001`, which
// is frozen and hand-owned (MAIN-236), so the value is a string nothing here
// ever encoded. Reads then failed with `ParseByteLength { len: 36 }` — and
// because `org_of` is on the path of every `require(_, Scope::Tenant(_))`, the
// tenant permission predicate had never executed on SQLite at all, while
// `POST /nodes/join` 500'd on a desktop install's first call.
//
// Fixed HERE rather than at the three call sites: a dialect branch per caller is
// the nested-dialect shape `check-nested-dialect.sh` exists to discourage, and
// the next text-stored uuid would rediscover it.

/// The strictness AC-2 asks for, in one place.
///
/// Only a well-formed **36-character hyphenated** uuid parses. That length is
/// what makes it exact: at 36 characters `parse_str` accepts the hyphenated
/// form and nothing else (simple is 32, braced 38, urn 45). A decoder that
/// coerced anything looser — a nil uuid for garbage, a swallowed type error —
/// would hide real corruption in every uuid column in the workspace, which is
/// the declared cost of taking this approach and the thing that must not come
/// true.
fn parse_text_uuid(s: &str) -> Option<uuid::Uuid> {
    (s.len() == 36)
        .then(|| uuid::Uuid::parse_str(s).ok())
        .flatten()
}

/// Name the column in the failure, so a bad value says which one it was.
fn uuid_decode_error(column: String, detail: String) -> DbError {
    DbError::Query(sqlx::Error::ColumnDecode {
        index: column,
        source: detail.into(),
    })
}

/// Read a uuid from a SQLite row that may hold it as a blob OR as text.
///
/// The blob path is tried FIRST and returns before anything else happens, so
/// the ordinary case is byte-for-byte what it was and the fallback is only
/// reached by a value the normal decode could not read.
///
/// When both fail, the ORIGINAL blob error is returned: it describes the type
/// actually present, where the text error would only say "not a string".
fn sqlite_uuid(
    blob: Result<uuid::Uuid, sqlx::Error>,
    text: impl FnOnce() -> Result<String, sqlx::Error>,
    column: impl FnOnce() -> String,
) -> Result<uuid::Uuid, DbError> {
    let original = match blob {
        Ok(v) => return Ok(v),
        Err(e) => e,
    };
    let Ok(s) = text() else {
        return Err(original.into());
    };
    parse_text_uuid(&s).ok_or_else(|| {
        uuid_decode_error(
            column(),
            format!("{s:?} is not a 36-character hyphenated uuid"),
        )
    })
}

impl FromDbColumn for uuid::Uuid {
    fn from_db_column(row: &DbRow, name: &str) -> Result<Self, DbError> {
        use sqlx::Row;
        match row {
            // Unchanged: Postgres has a real uuid type and always had.
            DbRow::Pg(r) => r.try_get::<uuid::Uuid, _>(name).map_err(Into::into),
            DbRow::Sqlite(r) => sqlite_uuid(
                r.try_get::<uuid::Uuid, _>(name),
                || r.try_get::<String, _>(name),
                || name.to_string(),
            ),
        }
    }
    fn from_db_column_at(row: &DbRow, index: usize) -> Result<Self, DbError> {
        use sqlx::Row;
        match row {
            DbRow::Pg(r) => r.try_get::<uuid::Uuid, _>(index).map_err(Into::into),
            DbRow::Sqlite(r) => sqlite_uuid(
                r.try_get::<uuid::Uuid, _>(index),
                || r.try_get::<String, _>(index),
                || index.to_string(),
            ),
        }
    }
}

/// `Option<T>` for any mappable `T`, resolved by asking the ROW whether the
/// column is NULL rather than by asking the type.
///
/// It has to be blanket, and it has to be here. `nook-types` cannot write
/// `impl FromDbColumn for Option<TenantId>` — the orphan rule refuses it,
/// because the local type is covered by foreign `Option` (the same wall its
/// `IntoDbValue` impls already hit). So every nullable domain column in the
/// workspace maps through this one impl, and a domain type only ever has to say
/// how to read itself when it IS there.
impl<T: FromDbColumn> FromDbColumn for Option<T> {
    fn from_db_column(row: &DbRow, name: &str) -> Result<Self, DbError> {
        if row.is_null(name)? {
            return Ok(None);
        }
        T::from_db_column(row, name).map(Some)
    }
    fn from_db_column_at(row: &DbRow, index: usize) -> Result<Self, DbError> {
        if row.is_null_at(index)? {
            return Ok(None);
        }
        T::from_db_column_at(row, index).map(Some)
    }
}

// ── text[] — the one column type the two engines do NOT share ───────────────
//
// MAIN-327 AC-4, and the reason the four hand-written `FromRow` impls existed:
// Postgres `text[]` has no SQLite equivalent, so those DTOs could not satisfy
// the old both-arms bound and shipped a `SqliteRow` impl that returned an error
// at runtime. The mapping layer had a hole in it shaped like `Vec<String>`.
//
// THE CHOICE, recorded: on SQLite a `text[]` column is **a JSON array in a TEXT
// column** — `["warn","error"]`. JSON because SQLite already stores every other
// structured value that way here (`jsonb` → `TEXT`, per docs/db-dialect-audit.md),
// because it round-trips through `serde_json` with no parser of our own, and
// because it is self-describing in a way a delimiter-joined string is not: a
// value containing a comma is not a new element.
//
// Reading tolerates `{}` and `{…}` — Postgres's own array-literal syntax —
// because the hand-owned SQLite `0001_init.sql` declares these columns
// `DEFAULT '{}'`, having been scaffolded from the Postgres DDL. A row inserted
// without naming the column therefore holds `{}`, and refusing to read it would
// make the default value unreadable by the mapper that owns the column. Writes
// always emit JSON.

fn parse_text_array(raw: &str, name: &str) -> Result<Vec<String>, DbError> {
    let s = raw.trim();
    if s.is_empty() || s == "{}" || s == "[]" {
        return Ok(Vec::new());
    }
    if s.starts_with('[') {
        return serde_json::from_str::<Vec<String>>(s).map_err(|e| {
            DbError::Query(sqlx::Error::ColumnDecode {
                index: name.to_string(),
                source: Box::new(e),
            })
        });
    }
    // A Postgres array literal, which only the frozen SQLite default produces.
    // Deliberately not a general parser: no quoting, no escapes, no nesting —
    // anything richer than `{a,b}` is a value we never wrote.
    if let Some(inner) = s.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
        return Ok(inner
            .split(',')
            .map(|p| p.trim().trim_matches('"').to_string())
            .filter(|p| !p.is_empty())
            .collect());
    }
    Err(DbError::Query(sqlx::Error::ColumnDecode {
        index: name.to_string(),
        source: "expected a JSON array of strings".into(),
    }))
}

impl FromDbColumn for Vec<String> {
    fn from_db_column(row: &DbRow, name: &str) -> Result<Self, DbError> {
        use sqlx::Row;
        match row {
            DbRow::Pg(r) => r.try_get::<Vec<String>, _>(name).map_err(Into::into),
            DbRow::Sqlite(r) => {
                let raw = r.try_get::<String, _>(name)?;
                parse_text_array(&raw, name)
            }
        }
    }
    fn from_db_column_at(row: &DbRow, index: usize) -> Result<Self, DbError> {
        use sqlx::Row;
        match row {
            DbRow::Pg(r) => r.try_get::<Vec<String>, _>(index).map_err(Into::into),
            DbRow::Sqlite(r) => {
                let raw = r.try_get::<String, _>(index)?;
                parse_text_array(&raw, &index.to_string())
            }
        }
    }
}

/// `FromDbRow` for tuples, mapped BY POSITION — the shape sqlx's own
/// `FromRow`-for-tuples had, kept because a `SELECT count(*), count(*)` has no
/// column names to map by. Named DTOs should use `#[derive(FromDbRow)]`; a
/// tuple is for the handful of rows that have nothing to name.
macro_rules! tuple_rows {
    ($( ($($n:tt : $t:ident),+) ),+ $(,)?) => {
        $(
            impl<$($t: FromDbColumn),+> FromDbRow for ($($t,)+) {
                fn from_db_row(row: &DbRow) -> Result<Self, DbError> {
                    Ok(($(row.get_at::<$t>($n)?,)+))
                }
            }
        )+
    };
}

tuple_rows!(
    (0: A),
    (0: A, 1: B),
    (0: A, 1: B, 2: C),
    (0: A, 1: B, 2: C, 3: D),
    (0: A, 1: B, 2: C, 3: D, 4: E),
    (0: A, 1: B, 2: C, 3: D, 4: E, 5: F),
    (0: A, 1: B, 2: C, 3: D, 4: E, 5: F, 6: G),
    (0: A, 1: B, 2: C, 3: D, 4: E, 5: F, 6: G, 7: H),
    (0: A, 1: B, 2: C, 3: D, 4: E, 5: F, 6: G, 7: H, 8: I),
    (0: A, 1: B, 2: C, 3: D, 4: E, 5: F, 6: G, 7: H, 8: I, 9: J),
    (0: A, 1: B, 2: C, 3: D, 4: E, 5: F, 6: G, 7: H, 8: I, 9: J, 10: K),
    (0: A, 1: B, 2: C, 3: D, 4: E, 5: F, 6: G, 7: H, 8: I, 9: J, 10: K, 11: L),
);

/// The JSON text a `text[]` column value is written as on SQLite — the write
/// half of the representation chosen above, used by the SQLite bind path.
pub(crate) fn text_array_to_json(xs: &[String]) -> String {
    serde_json::to_string(xs).unwrap_or_else(|_| "[]".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_json_array_round_trips_through_the_sqlite_representation() {
        let xs = vec!["warn".to_string(), "error".to_string()];
        let written = text_array_to_json(&xs);
        assert_eq!(written, r#"["warn","error"]"#);
        assert_eq!(parse_text_array(&written, "levels").unwrap(), xs);
    }

    #[test]
    fn a_value_containing_a_comma_survives() {
        // The reason the representation is JSON and not a joined string: a
        // delimiter inside a value must not become a new element.
        let xs = vec!["a,b".to_string(), "c".to_string()];
        let round = parse_text_array(&text_array_to_json(&xs), "kinds").unwrap();
        assert_eq!(round, xs);
    }

    #[test]
    fn the_frozen_sqlite_default_reads_as_empty() {
        // `0001_init.sql` declares these columns DEFAULT '{}' — Postgres array
        // literal syntax, scaffolded from the Postgres DDL. A row that took the
        // default must still be readable.
        assert!(parse_text_array("{}", "levels").unwrap().is_empty());
        assert!(parse_text_array("[]", "levels").unwrap().is_empty());
        assert!(parse_text_array("", "levels").unwrap().is_empty());
    }

    #[test]
    fn a_postgres_array_literal_with_elements_still_reads() {
        assert_eq!(
            parse_text_array("{warn,error}", "levels").unwrap(),
            vec!["warn".to_string(), "error".to_string()]
        );
    }

    #[test]
    fn garbage_is_an_error_naming_the_column_rather_than_an_empty_list() {
        // Failing open here would turn "this column holds something we did not
        // write" into "you have no notification levels", silently.
        let e = parse_text_array("not an array", "levels").unwrap_err();
        assert!(e.to_string().contains("levels"), "{e}");
    }

    // ── uuid on SQLite (MAIN-437) ───────────────────────────────────────────

    /// One SQLite row from a literal `SELECT`, so a decode can be exercised
    /// against a value stored exactly as the schema stores it.
    async fn sqlite_row(select: &str) -> DbRow {
        use sqlx::Executor;
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        let row = pool.fetch_one(select).await.expect("select");
        DbRow::Sqlite(row)
    }

    /// The fix: `tenants.org_id`'s shape — a 36-char hyphenated string, written
    /// by a schema DEFAULT and therefore never bound as a blob.
    #[tokio::test]
    async fn a_text_stored_uuid_decodes() {
        let want = uuid::Uuid::parse_str("00000000-0000-0000-0000-0000000000a1").unwrap();
        let row = sqlite_row("SELECT '00000000-0000-0000-0000-0000000000a1' AS org_id").await;
        assert_eq!(uuid::Uuid::from_db_column(&row, "org_id").unwrap(), want);
        assert_eq!(uuid::Uuid::from_db_column_at(&row, 0).unwrap(), want);
    }

    /// AC-3. The ordinary path is tried first and returns before the fallback is
    /// reached, so a bound uuid is unaffected — proven by reading one back,
    /// not by inspecting the branch.
    #[tokio::test]
    async fn a_blob_stored_uuid_is_unaffected() {
        let want = uuid::Uuid::now_v7();
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        sqlx::query("CREATE TABLE t (id BLOB)")
            .execute(&pool)
            .await
            .unwrap();
        // Bound exactly as `DbValue::Uuid` binds it — 16 bytes, not text.
        sqlx::query("INSERT INTO t (id) VALUES (?)")
            .bind(want)
            .execute(&pool)
            .await
            .unwrap();
        let row = DbRow::Sqlite(
            sqlx::query("SELECT id FROM t")
                .fetch_one(&pool)
                .await
                .unwrap(),
        );
        assert_eq!(uuid::Uuid::from_db_column(&row, "id").unwrap(), want);
    }

    /// AC-2, and the cost this approach declares. A decoder that coerced
    /// garbage into a nil uuid would hide real corruption in every uuid column
    /// in the workspace; anything that is not a well-formed 36-char hyphenated
    /// uuid must still fail, naming the column.
    #[tokio::test]
    async fn garbage_text_still_fails_and_names_the_column() {
        for bad in [
            "'definitely not a uuid'",
            // Right length, wrong content — length alone is not the test.
            "'zzzzzzzz-zzzz-zzzz-zzzz-zzzzzzzzzzzz'",
            // A VALID uuid in the 32-char simple form: refused on purpose, so
            // the accepted shape is exactly what the schema writes.
            "'000000000000000000000000000000a1'",
        ] {
            let row = sqlite_row(&format!("SELECT {bad} AS org_id")).await;
            let e = uuid::Uuid::from_db_column(&row, "org_id")
                .expect_err("a malformed uuid must not decode");
            assert!(
                e.to_string().contains("org_id"),
                "must name the column: {e}"
            );
        }
    }

    /// A value that is neither a blob nor text keeps the ORIGINAL decode error,
    /// which describes the type actually present.
    #[tokio::test]
    async fn a_non_text_non_blob_value_reports_the_real_mismatch() {
        let row = sqlite_row("SELECT 42 AS org_id").await;
        assert!(uuid::Uuid::from_db_column(&row, "org_id").is_err());
    }
}
