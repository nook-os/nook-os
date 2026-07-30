//! The engine-dispatching pool (MAIN-205).
//!
//! The owner's ruling (2026-07-28, resolving the builder's escalation) fixed the
//! mechanism: **a custom dispatch API on the pool** — not `sqlx::Any` (it cannot
//! encode `Uuid`, which the codebase binds ~1500 times), and not a bare enum
//! pretending to be an `Executor` (impossible: `Executor::Database` is one
//! concrete associated type). Every query flows through one execution
//! convention here, so "which engine" is a single-file concern forever.
//!
//! The surface is the [`Db`] trait: callers pass SQL text plus an owned,
//! engine-neutral parameter list ([`DbValue`], usually via [`params!`]), and row
//! mapping rides `sqlx::FromRow` over both row types. The methods are named to
//! NOT collide with sqlx's `Executor` (`query_one`/`exec`/…, never `fetch_one`),
//! which is what lets the call-site migration run while `DbPool` is still
//! `PgPool`: `Db` is implemented for `PgPool` too, so a migrated `db.query_one(…)`
//! compiles green before the type flip, and the flip to [`EnginePool`] needs no
//! further site change. The Postgres arm reproduces today's binds exactly
//! (bit-identical — MAIN-205 AC-3). The SQLite arm is built to *compile* here;
//! its runtime is proven when the engine + migration track land in MAIN-196.

use sqlx::postgres::{PgArguments, PgPool, PgRow};
use sqlx::sqlite::{SqliteArguments, SqlitePool, SqliteRow};
use sqlx::{Arguments, FromRow};

use crate::Engine;

/// An owned, engine-neutral bound parameter.
///
/// Scalars carry an `Option` so a `NULL` bind keeps its type (the codebase binds
/// `Option<T>` widely, and a typed null is what Postgres infers from the
/// placeholder today — preserving that is what makes the Postgres arm
/// bit-identical). Lists are their own variants because they do NOT bind as a
/// single scalar: Postgres takes them as an array for `= ANY($n)`, while SQLite
/// (no array type) expands them into an `IN (…)` list of scalar binds.
#[derive(Debug, Clone, PartialEq)]
pub enum DbValue {
    Bool(Option<bool>),
    I16(Option<i16>),
    I32(Option<i32>),
    I64(Option<i64>),
    F64(Option<f64>),
    Text(Option<String>),
    Bytes(Option<Vec<u8>>),
    Uuid(Option<uuid::Uuid>),
    Timestamptz(Option<chrono::DateTime<chrono::Utc>>),
    Json(Option<serde_json::Value>),
    /// A `text[]` array parameter (Postgres `= ANY($n)`).
    TextList(Vec<String>),
    /// A `uuid[]` array parameter (Postgres `= ANY($n)`).
    UuidList(Vec<uuid::Uuid>),
    /// A `bigint[]` array parameter (Postgres `= ANY($n)`).
    I64List(Vec<i64>),
    /// A nullable `text[]` COLUMN value (e.g. inserting `choices text[]`) — a
    /// single array bind, NOT an `= ANY` operand, so it is not list-expanded.
    OptTextArray(Option<Vec<String>>),
}

impl DbValue {
    /// Whether this is a list parameter (expanded, not scalar-bound, on SQLite).
    fn is_list(&self) -> bool {
        matches!(
            self,
            DbValue::TextList(_) | DbValue::UuidList(_) | DbValue::I64List(_)
        )
    }
}

// ── ergonomic conversions ────────────────────────────────────────────────────

/// Conversion into a bound [`DbValue`], used by [`params!`]. Implemented here for
/// the scalar/list/reference types, and in `nook-types` for the UUID newtype IDs
/// (`TenantId`, `NodeId`, …) — a foreign-trait-for-local-type impl the orphan
/// rule allows, which `From<Id> for DbValue` did not. That is what lets
/// `params![tenant_id, opt_id, name]` take IDs, options, and scalars uniformly
/// with no `.0` at the call site. UUID newtypes are `#[sqlx(transparent)]` over
/// `Uuid`, so unwrapping encodes exactly as binding the newtype did — the
/// Postgres arm stays bit-identical.
pub trait IntoDbValue {
    fn into_db_value(self) -> DbValue;
}

macro_rules! into_db_value {
    ($($t:ty => |$s:ident| $body:expr),+ $(,)?) => {
        $( impl IntoDbValue for $t {
            fn into_db_value(self) -> DbValue { let $s = self; $body }
        } )+
    };
}

into_db_value! {
    DbValue => |v| v,
    uuid::Uuid => |v| DbValue::Uuid(Some(v)),
    Option<uuid::Uuid> => |v| DbValue::Uuid(v),
    &uuid::Uuid => |v| DbValue::Uuid(Some(*v)),
    String => |v| DbValue::Text(Some(v)),
    Option<String> => |v| DbValue::Text(v),
    &str => |v| DbValue::Text(Some(v.to_owned())),
    &String => |v| DbValue::Text(Some(v.clone())),
    bool => |v| DbValue::Bool(Some(v)),
    Option<bool> => |v| DbValue::Bool(v),
    i16 => |v| DbValue::I16(Some(v)),
    Option<i16> => |v| DbValue::I16(v),
    i32 => |v| DbValue::I32(Some(v)),
    Option<i32> => |v| DbValue::I32(v),
    i64 => |v| DbValue::I64(Some(v)),
    Option<i64> => |v| DbValue::I64(v),
    f64 => |v| DbValue::F64(Some(v)),
    Option<f64> => |v| DbValue::F64(v),
    &[u8] => |v| DbValue::Bytes(Some(v.to_vec())),
    chrono::DateTime<chrono::Utc> => |v| DbValue::Timestamptz(Some(v)),
    Option<chrono::DateTime<chrono::Utc>> => |v| DbValue::Timestamptz(v),
    serde_json::Value => |v| DbValue::Json(Some(v)),
    Option<serde_json::Value> => |v| DbValue::Json(v),
    &serde_json::Value => |v| DbValue::Json(Some(v.clone())),
    Vec<u8> => |v| DbValue::Bytes(Some(v)),
    Option<Vec<u8>> => |v| DbValue::Bytes(v),
    Vec<String> => |v| DbValue::TextList(v),
    Vec<uuid::Uuid> => |v| DbValue::UuidList(v),
    Vec<i64> => |v| DbValue::I64List(v),
    &[String] => |v| DbValue::TextList(v.to_vec()),
    &[uuid::Uuid] => |v| DbValue::UuidList(v.to_vec()),
    &[i64] => |v| DbValue::I64List(v.to_vec()),
    Option<Vec<String>> => |v| DbValue::OptTextArray(v),
}

// Lifetime-carrying reference conversions the `$t:ty` macro can't express.
impl IntoDbValue for Option<&str> {
    fn into_db_value(self) -> DbValue {
        DbValue::Text(self.map(str::to_owned))
    }
}

/// Build a `Vec<DbValue>` from a heterogeneous parameter list, in bind order.
///
/// ```ignore
/// db.query_one::<Workspace>(
///     "SELECT * FROM workspaces WHERE tenant_id = $1 AND id = $2",
///     params![tenant, id],
/// ).await?
/// ```
#[macro_export]
macro_rules! params {
    () => { ::std::vec::Vec::<$crate::DbValue>::new() };
    ($($v:expr),+ $(,)?) => { ::std::vec![$($crate::IntoDbValue::into_db_value($v)),+] };
}

// ── argument encoding, per arm ───────────────────────────────────────────────

fn pg_args(params: Vec<DbValue>) -> Result<PgArguments, sqlx::Error> {
    let mut a = PgArguments::default();
    let add_err = |e: sqlx::error::BoxDynError| sqlx::Error::Encode(e);
    for v in params {
        match v {
            DbValue::Bool(x) => a.add(x).map_err(add_err)?,
            DbValue::I16(x) => a.add(x).map_err(add_err)?,
            DbValue::I32(x) => a.add(x).map_err(add_err)?,
            DbValue::I64(x) => a.add(x).map_err(add_err)?,
            DbValue::F64(x) => a.add(x).map_err(add_err)?,
            DbValue::Text(x) => a.add(x).map_err(add_err)?,
            DbValue::Bytes(x) => a.add(x).map_err(add_err)?,
            DbValue::Uuid(x) => a.add(x).map_err(add_err)?,
            DbValue::Timestamptz(x) => a.add(x).map_err(add_err)?,
            DbValue::Json(x) => a.add(x).map_err(add_err)?,
            // Arrays bind as a single Postgres array parameter — this is exactly
            // the `.bind(&vec)` against `= ANY($n)` the codebase does today.
            DbValue::TextList(x) => a.add(x).map_err(add_err)?,
            DbValue::UuidList(x) => a.add(x).map_err(add_err)?,
            DbValue::I64List(x) => a.add(x).map_err(add_err)?,
            DbValue::OptTextArray(x) => a.add(x).map_err(add_err)?,
        }
    }
    Ok(a)
}

/// SQLite has no array type, so a list parameter cannot bind as one value: the
/// `= ANY($n)` it fills must become `IN ($a, $b, …)` with one scalar bind per
/// element, and every later placeholder renumbered. This produces the rewritten
/// SQL and the flattened scalar arguments.
///
/// SQLite is compiled here but proven in MAIN-196; [`expand_lists`] carries its
/// own pure unit tests so the renumbering contract is pinned now.
fn sqlite_args(
    sql: &str,
    params: Vec<DbValue>,
) -> Result<(String, SqliteArguments<'static>), sqlx::Error> {
    let (rewritten, flat) = expand_lists(sql, params);
    let mut a = SqliteArguments::default();
    let add_err = |e: sqlx::error::BoxDynError| sqlx::Error::Encode(e);
    for v in flat {
        match v {
            DbValue::Bool(x) => a.add(x).map_err(add_err)?,
            DbValue::I16(x) => a.add(x).map_err(add_err)?,
            DbValue::I32(x) => a.add(x).map_err(add_err)?,
            DbValue::I64(x) => a.add(x).map_err(add_err)?,
            DbValue::F64(x) => a.add(x).map_err(add_err)?,
            DbValue::Text(x) => a.add(x).map_err(add_err)?,
            DbValue::Bytes(x) => a.add(x).map_err(add_err)?,
            DbValue::Uuid(x) => a.add(x).map_err(add_err)?,
            DbValue::Timestamptz(x) => a.add(x).map_err(add_err)?,
            DbValue::Json(x) => a.add(x).map_err(add_err)?,
            DbValue::TextList(_) | DbValue::UuidList(_) | DbValue::I64List(_) => {
                unreachable!("expand_lists flattens every list parameter")
            }
            // A `text[]` column value has no SQLite array encoding; MAIN-196
            // decides its storage shape. Never runs under the Postgres proof.
            DbValue::OptTextArray(_) => {
                return Err(sqlx::Error::Encode(
                    "text[] column parameters on SQLite land in MAIN-196".into(),
                ))
            }
        }
    }
    Ok((rewritten, a))
}

/// Rewrite Postgres array-membership predicates into their SQLite equivalents
/// and flatten list parameters to scalar binds.
///
/// SQLite has no array type, so a list bound to `= ANY($n)` — or `!= ALL($n)` /
/// `<> ALL($n)` — cannot travel as one value. The predicate becomes
/// `IN ($a, $b, …)` (`NOT IN (…)` for the `ALL` forms), one scalar placeholder
/// per element, with every later placeholder renumbered and the parameters
/// flattened to match. `= ANY($n)` left as `ANY($n, …)` is not valid SQLite —
/// the operator itself must change, not only the placeholder count. Pure and
/// engine-agnostic; the SQLite arm is its only user today (Postgres binds the
/// array directly and never calls this).
///
/// An empty list renders `IN (NULL)` / `NOT IN (NULL)` — always-unknown, so a
/// `= ANY(empty)` still matches nothing, exactly as Postgres. (Its dual
/// `!= ALL(empty)` is vacuously *true* in Postgres — matching everything — which
/// `NOT IN (NULL)` does not reproduce; no call site binds an empty list to an
/// `ALL` predicate, and MAIN-196, where SQLite is actually executed, owns that
/// edge if one ever arises.)
fn expand_lists(sql: &str, params: Vec<DbValue>) -> (String, Vec<DbValue>) {
    if !params.iter().any(DbValue::is_list) {
        return (sql.to_owned(), params);
    }

    // Flatten every list into scalar binds. For each ORIGINAL placeholder record
    // the new placeholder number(s) it maps to and whether it was a list — the
    // walk below rewrites a list's enclosing `ANY`/`ALL` operator, a scalar's it
    // leaves alone.
    let mut flat: Vec<DbValue> = Vec::new();
    let mut groups: Vec<Vec<usize>> = Vec::with_capacity(params.len());
    let mut is_list: Vec<bool> = Vec::with_capacity(params.len());
    let mut next = 1usize;
    for p in params {
        let list = p.is_list();
        let idxs: Vec<usize> = match p {
            DbValue::TextList(xs) => xs
                .into_iter()
                .map(|x| push_scalar(&mut flat, DbValue::Text(Some(x)), &mut next))
                .collect(),
            DbValue::UuidList(xs) => xs
                .into_iter()
                .map(|x| push_scalar(&mut flat, DbValue::Uuid(Some(x)), &mut next))
                .collect(),
            DbValue::I64List(xs) => xs
                .into_iter()
                .map(|x| push_scalar(&mut flat, DbValue::I64(Some(x)), &mut next))
                .collect(),
            scalar => vec![push_scalar(&mut flat, scalar, &mut next)],
        };
        groups.push(idxs);
        is_list.push(list);
    }

    (rewrite_membership(sql, &groups, &is_list), flat)
}

/// Push one flattened scalar and hand back the placeholder number it takes.
fn push_scalar(flat: &mut Vec<DbValue>, v: DbValue, next: &mut usize) -> usize {
    flat.push(v);
    let i = *next;
    *next += 1;
    i
}

/// Walk `sql`, substituting each `$k` placeholder (1-based, original numbering)
/// with its flattened group and, for a list group, rewriting the Postgres
/// array-membership operator that wraps it into SQLite's `IN` / `NOT IN`.
fn rewrite_membership(sql: &str, groups: &[Vec<usize>], is_list: &[bool]) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len() + 16);
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() {
            let mut j = i + 1;
            let mut n = 0usize;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                n = n * 10 + (bytes[j] - b'0') as usize;
                j += 1;
            }
            let g = &groups[n - 1];
            if is_list[n - 1] {
                // The `<op> ANY(` / `<op> ALL(` opening this placeholder is
                // already in `out`; convert it to `IN (` / `NOT IN (`.
                open_membership(&mut out);
                if g.is_empty() {
                    out.push_str("NULL");
                } else {
                    let joined = g.iter().map(|i| format!("${i}")).collect::<Vec<_>>();
                    out.push_str(&joined.join(", "));
                }
            } else {
                out.push('$');
                out.push_str(&g[0].to_string());
            }
            i = j;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

/// `out` ends with the opening of a Postgres array-membership test that wraps a
/// list placeholder — `<op> ANY(` or `<op> ALL(`, with `<op>` one of `=`, `!=`,
/// `<>` and any whitespace between the tokens. Rewrite that tail in place to
/// SQLite's `IN (` (for `ANY`) or `NOT IN (` (for `ALL`). If the tail is not a
/// recognised membership open — which never happens for the generated SQL — the
/// buffer is left exactly as it was.
fn open_membership(out: &mut String) {
    let original_len = out.len();
    trim_end_ws(out);
    if !out.ends_with('(') {
        out.truncate(original_len);
        return;
    }
    out.pop(); // the '('
    trim_end_ws(out);
    let negated = if ends_with_ci(out, "ALL") {
        true
    } else if ends_with_ci(out, "ANY") {
        false
    } else {
        out.truncate(original_len);
        return;
    };
    pop_chars(out, 3); // ANY / ALL
    trim_end_ws(out);
    if out.ends_with("!=") || out.ends_with("<>") {
        pop_chars(out, 2);
    } else if out.ends_with('=') {
        pop_chars(out, 1);
    }
    trim_end_ws(out);
    out.push(' ');
    out.push_str(if negated { "NOT IN (" } else { "IN (" });
}

/// Drop trailing whitespace from `s` in place.
fn trim_end_ws(s: &mut String) {
    while s.chars().next_back().is_some_and(char::is_whitespace) {
        s.pop();
    }
}

/// Drop the last `n` `char`s from `s`.
fn pop_chars(s: &mut String, n: usize) {
    for _ in 0..n {
        s.pop();
    }
}

/// ASCII-case-insensitive suffix test on the raw bytes (the keyword is ASCII, so
/// this needs no char-boundary care).
fn ends_with_ci(s: &str, suffix: &str) -> bool {
    let b = s.as_bytes();
    let sb = suffix.as_bytes();
    b.len() >= sb.len() && b[b.len() - sb.len()..].eq_ignore_ascii_case(sb)
}

// ── the dispatch surface ─────────────────────────────────────────────────────

/// The query surface every call site routes through. Named to avoid sqlx's
/// `Executor` methods (`query_one`, never `fetch_one`) so it can be implemented
/// for `PgPool` during the migration and for [`EnginePool`] after the flip, with
/// call sites unchanged across it.
#[allow(async_fn_in_trait)]
pub trait Db {
    /// Run a statement, returning affected rows. For `RETURNING`, use the
    /// `query_*` fetchers instead.
    async fn exec(&self, sql: &str, params: Vec<DbValue>) -> Result<u64, crate::DbError>;

    /// Fetch exactly one row, mapped via `FromRow`.
    async fn query_one<T>(&self, sql: &str, params: Vec<DbValue>) -> Result<T, crate::DbError>
    where
        T: Send + Unpin,
        T: for<'r> FromRow<'r, PgRow>,
        T: for<'r> FromRow<'r, SqliteRow>;

    /// Fetch at most one row, mapped via `FromRow`.
    async fn query_opt<T>(
        &self,
        sql: &str,
        params: Vec<DbValue>,
    ) -> Result<Option<T>, crate::DbError>
    where
        T: Send + Unpin,
        T: for<'r> FromRow<'r, PgRow>,
        T: for<'r> FromRow<'r, SqliteRow>;

    /// Fetch every row, mapped via `FromRow`.
    async fn query_all<T>(&self, sql: &str, params: Vec<DbValue>) -> Result<Vec<T>, crate::DbError>
    where
        T: Send + Unpin,
        T: for<'r> FromRow<'r, PgRow>,
        T: for<'r> FromRow<'r, SqliteRow>;

    /// Fetch a single scalar (one column of one row), e.g. `SELECT count(*)`.
    async fn query_scalar<T>(&self, sql: &str, params: Vec<DbValue>) -> Result<T, crate::DbError>
    where
        T: Send + Unpin,
        for<'r> T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
        for<'r> T: sqlx::Decode<'r, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite>;

    /// Fetch a single scalar column from at most one row.
    async fn query_scalar_opt<T>(
        &self,
        sql: &str,
        params: Vec<DbValue>,
    ) -> Result<Option<T>, crate::DbError>
    where
        T: Send + Unpin,
        for<'r> T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
        for<'r> T: sqlx::Decode<'r, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite>;

    /// Fetch a single scalar column from every row (`SELECT id FROM …`).
    async fn query_scalar_all<T>(
        &self,
        sql: &str,
        params: Vec<DbValue>,
    ) -> Result<Vec<T>, crate::DbError>
    where
        T: Send + Unpin,
        for<'r> T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
        for<'r> T: sqlx::Decode<'r, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite>;
}

impl Db for PgPool {
    async fn exec(&self, sql: &str, params: Vec<DbValue>) -> Result<u64, crate::DbError> {
        let args = pg_args(params)?;
        Ok(sqlx::query_with(sql, args)
            .execute(self)
            .await?
            .rows_affected())
    }
    async fn query_one<T>(&self, sql: &str, params: Vec<DbValue>) -> Result<T, crate::DbError>
    where
        T: Send + Unpin,
        T: for<'r> FromRow<'r, PgRow>,
        T: for<'r> FromRow<'r, SqliteRow>,
    {
        let args = pg_args(params)?;
        sqlx::query_as_with::<sqlx::Postgres, T, _>(sql, args)
            .fetch_one(self)
            .await
            .map_err(Into::into)
    }
    async fn query_opt<T>(
        &self,
        sql: &str,
        params: Vec<DbValue>,
    ) -> Result<Option<T>, crate::DbError>
    where
        T: Send + Unpin,
        T: for<'r> FromRow<'r, PgRow>,
        T: for<'r> FromRow<'r, SqliteRow>,
    {
        let args = pg_args(params)?;
        sqlx::query_as_with::<sqlx::Postgres, T, _>(sql, args)
            .fetch_optional(self)
            .await
            .map_err(Into::into)
    }
    async fn query_all<T>(&self, sql: &str, params: Vec<DbValue>) -> Result<Vec<T>, crate::DbError>
    where
        T: Send + Unpin,
        T: for<'r> FromRow<'r, PgRow>,
        T: for<'r> FromRow<'r, SqliteRow>,
    {
        let args = pg_args(params)?;
        sqlx::query_as_with::<sqlx::Postgres, T, _>(sql, args)
            .fetch_all(self)
            .await
            .map_err(Into::into)
    }
    async fn query_scalar<T>(&self, sql: &str, params: Vec<DbValue>) -> Result<T, crate::DbError>
    where
        T: Send + Unpin,
        for<'r> T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
        for<'r> T: sqlx::Decode<'r, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite>,
    {
        let args = pg_args(params)?;
        sqlx::query_scalar_with::<sqlx::Postgres, T, _>(sql, args)
            .fetch_one(self)
            .await
            .map_err(Into::into)
    }
    async fn query_scalar_opt<T>(
        &self,
        sql: &str,
        params: Vec<DbValue>,
    ) -> Result<Option<T>, crate::DbError>
    where
        T: Send + Unpin,
        for<'r> T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
        for<'r> T: sqlx::Decode<'r, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite>,
    {
        let args = pg_args(params)?;
        sqlx::query_scalar_with::<sqlx::Postgres, T, _>(sql, args)
            .fetch_optional(self)
            .await
            .map_err(Into::into)
    }
    async fn query_scalar_all<T>(
        &self,
        sql: &str,
        params: Vec<DbValue>,
    ) -> Result<Vec<T>, crate::DbError>
    where
        T: Send + Unpin,
        for<'r> T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
        for<'r> T: sqlx::Decode<'r, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite>,
    {
        let args = pg_args(params)?;
        sqlx::query_scalar_with::<sqlx::Postgres, T, _>(sql, args)
            .fetch_all(self)
            .await
            .map_err(Into::into)
    }
}

impl Db for SqlitePool {
    async fn exec(&self, sql: &str, params: Vec<DbValue>) -> Result<u64, crate::DbError> {
        let (sql, args) = sqlite_args(sql, params)?;
        Ok(sqlx::query_with(&sql, args)
            .execute(self)
            .await?
            .rows_affected())
    }
    async fn query_one<T>(&self, sql: &str, params: Vec<DbValue>) -> Result<T, crate::DbError>
    where
        T: Send + Unpin,
        T: for<'r> FromRow<'r, PgRow>,
        T: for<'r> FromRow<'r, SqliteRow>,
    {
        let (sql, args) = sqlite_args(sql, params)?;
        sqlx::query_as_with::<sqlx::Sqlite, T, _>(&sql, args)
            .fetch_one(self)
            .await
            .map_err(Into::into)
    }
    async fn query_opt<T>(
        &self,
        sql: &str,
        params: Vec<DbValue>,
    ) -> Result<Option<T>, crate::DbError>
    where
        T: Send + Unpin,
        T: for<'r> FromRow<'r, PgRow>,
        T: for<'r> FromRow<'r, SqliteRow>,
    {
        let (sql, args) = sqlite_args(sql, params)?;
        sqlx::query_as_with::<sqlx::Sqlite, T, _>(&sql, args)
            .fetch_optional(self)
            .await
            .map_err(Into::into)
    }
    async fn query_all<T>(&self, sql: &str, params: Vec<DbValue>) -> Result<Vec<T>, crate::DbError>
    where
        T: Send + Unpin,
        T: for<'r> FromRow<'r, PgRow>,
        T: for<'r> FromRow<'r, SqliteRow>,
    {
        let (sql, args) = sqlite_args(sql, params)?;
        sqlx::query_as_with::<sqlx::Sqlite, T, _>(&sql, args)
            .fetch_all(self)
            .await
            .map_err(Into::into)
    }
    async fn query_scalar<T>(&self, sql: &str, params: Vec<DbValue>) -> Result<T, crate::DbError>
    where
        T: Send + Unpin,
        for<'r> T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
        for<'r> T: sqlx::Decode<'r, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite>,
    {
        let (sql, args) = sqlite_args(sql, params)?;
        sqlx::query_scalar_with::<sqlx::Sqlite, T, _>(&sql, args)
            .fetch_one(self)
            .await
            .map_err(Into::into)
    }
    async fn query_scalar_opt<T>(
        &self,
        sql: &str,
        params: Vec<DbValue>,
    ) -> Result<Option<T>, crate::DbError>
    where
        T: Send + Unpin,
        for<'r> T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
        for<'r> T: sqlx::Decode<'r, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite>,
    {
        let (sql, args) = sqlite_args(sql, params)?;
        sqlx::query_scalar_with::<sqlx::Sqlite, T, _>(&sql, args)
            .fetch_optional(self)
            .await
            .map_err(Into::into)
    }
    async fn query_scalar_all<T>(
        &self,
        sql: &str,
        params: Vec<DbValue>,
    ) -> Result<Vec<T>, crate::DbError>
    where
        T: Send + Unpin,
        for<'r> T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
        for<'r> T: sqlx::Decode<'r, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite>,
    {
        let (sql, args) = sqlite_args(sql, params)?;
        sqlx::query_scalar_with::<sqlx::Sqlite, T, _>(&sql, args)
            .fetch_all(self)
            .await
            .map_err(Into::into)
    }
}

/// The engine-dispatching pool. Cheap to clone (each arm is an `sqlx` pool
/// handle). At the MAIN-205 flip this becomes the workspace's `DbPool`; its [`Db`]
/// impl simply forwards to whichever concrete pool it holds.
#[derive(Clone, Debug)]
pub struct EnginePool(Arm);

#[derive(Clone, Debug)]
enum Arm {
    Pg(PgPool),
    Sqlite(SqlitePool),
}

impl EnginePool {
    /// Wrap an already-open Postgres pool.
    pub fn from_pg(pool: PgPool) -> Self {
        EnginePool(Arm::Pg(pool))
    }

    /// Wrap an already-open SQLite pool (constructed by MAIN-196's boot path).
    pub fn from_sqlite(pool: SqlitePool) -> Self {
        EnginePool(Arm::Sqlite(pool))
    }

    /// The underlying SQLite pool, for the same narrow reason [`pg`] exists:
    /// migrations take a concrete sqlx pool, not the dispatch surface. Panics on
    /// a Postgres pool — callers branch on [`engine`] first.
    pub fn sqlite(&self) -> &SqlitePool {
        match &self.0 {
            Arm::Sqlite(p) => p,
            Arm::Pg(_) => panic!("EnginePool::sqlite() on a Postgres pool"),
        }
    }

    /// Which engine this pool dispatches to.
    pub fn engine(&self) -> Engine {
        match self.0 {
            Arm::Pg(_) => Engine::Postgres,
            Arm::Sqlite(_) => Engine::Sqlite,
        }
    }

    /// The underlying Postgres pool, for the two sqlx APIs that are not part of
    /// the [`Db`] dispatch surface and stay Postgres-shaped for now: schema
    /// migrations (`sqlx::migrate!` embeds Postgres DDL; the SQLite migration
    /// track is MAIN-196) and any boot-time bootstrap that must reach the raw
    /// pool. Panics on the SQLite arm — callers here only ever run under Postgres
    /// until MAIN-196 supplies the SQLite path.
    pub fn pg(&self) -> &PgPool {
        match &self.0 {
            Arm::Pg(p) => p,
            Arm::Sqlite(_) => {
                panic!("EnginePool::pg() on a SQLite pool — migrations are Postgres-only until MAIN-196")
            }
        }
    }

    /// Begin a transaction. The returned [`DbTx`] carries the same query surface;
    /// finish with [`DbTx::commit`] / [`DbTx::rollback`] (drop rolls back).
    pub async fn begin(&self) -> Result<DbTx<'_>, sqlx::Error> {
        match &self.0 {
            Arm::Pg(p) => Ok(DbTx::Pg(p.begin().await?)),
            Arm::Sqlite(p) => Ok(DbTx::Sqlite(p.begin().await?)),
        }
    }
}

impl Db for EnginePool {
    async fn exec(&self, sql: &str, params: Vec<DbValue>) -> Result<u64, crate::DbError> {
        match &self.0 {
            Arm::Pg(p) => p.exec(sql, params).await,
            Arm::Sqlite(p) => p.exec(sql, params).await,
        }
    }
    async fn query_one<T>(&self, sql: &str, params: Vec<DbValue>) -> Result<T, crate::DbError>
    where
        T: Send + Unpin,
        T: for<'r> FromRow<'r, PgRow>,
        T: for<'r> FromRow<'r, SqliteRow>,
    {
        match &self.0 {
            Arm::Pg(p) => p.query_one(sql, params).await,
            Arm::Sqlite(p) => p.query_one(sql, params).await,
        }
    }
    async fn query_opt<T>(
        &self,
        sql: &str,
        params: Vec<DbValue>,
    ) -> Result<Option<T>, crate::DbError>
    where
        T: Send + Unpin,
        T: for<'r> FromRow<'r, PgRow>,
        T: for<'r> FromRow<'r, SqliteRow>,
    {
        match &self.0 {
            Arm::Pg(p) => p.query_opt(sql, params).await,
            Arm::Sqlite(p) => p.query_opt(sql, params).await,
        }
    }
    async fn query_all<T>(&self, sql: &str, params: Vec<DbValue>) -> Result<Vec<T>, crate::DbError>
    where
        T: Send + Unpin,
        T: for<'r> FromRow<'r, PgRow>,
        T: for<'r> FromRow<'r, SqliteRow>,
    {
        match &self.0 {
            Arm::Pg(p) => p.query_all(sql, params).await,
            Arm::Sqlite(p) => p.query_all(sql, params).await,
        }
    }
    async fn query_scalar<T>(&self, sql: &str, params: Vec<DbValue>) -> Result<T, crate::DbError>
    where
        T: Send + Unpin,
        for<'r> T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
        for<'r> T: sqlx::Decode<'r, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite>,
    {
        match &self.0 {
            Arm::Pg(p) => p.query_scalar(sql, params).await,
            Arm::Sqlite(p) => p.query_scalar(sql, params).await,
        }
    }
    async fn query_scalar_opt<T>(
        &self,
        sql: &str,
        params: Vec<DbValue>,
    ) -> Result<Option<T>, crate::DbError>
    where
        T: Send + Unpin,
        for<'r> T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
        for<'r> T: sqlx::Decode<'r, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite>,
    {
        match &self.0 {
            Arm::Pg(p) => p.query_scalar_opt(sql, params).await,
            Arm::Sqlite(p) => p.query_scalar_opt(sql, params).await,
        }
    }
    async fn query_scalar_all<T>(
        &self,
        sql: &str,
        params: Vec<DbValue>,
    ) -> Result<Vec<T>, crate::DbError>
    where
        T: Send + Unpin,
        for<'r> T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
        for<'r> T: sqlx::Decode<'r, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite>,
    {
        match &self.0 {
            Arm::Pg(p) => p.query_scalar_all(sql, params).await,
            Arm::Sqlite(p) => p.query_scalar_all(sql, params).await,
        }
    }
}

/// An in-flight transaction, dispatching to whichever engine opened it. Mirrors
/// the [`Db`] query surface so a `db.begin()` block reads the same on either arm.
/// Not `Clone`: a transaction is a single borrowed session.
pub enum DbTx<'c> {
    Pg(sqlx::Transaction<'c, sqlx::Postgres>),
    Sqlite(sqlx::Transaction<'c, sqlx::Sqlite>),
}

impl DbTx<'_> {
    /// Run a statement inside the transaction, returning affected rows.
    pub async fn exec(&mut self, sql: &str, params: Vec<DbValue>) -> Result<u64, crate::DbError> {
        match self {
            DbTx::Pg(tx) => {
                let args = pg_args(params)?;
                Ok(sqlx::query_with(sql, args)
                    .execute(&mut **tx)
                    .await?
                    .rows_affected())
            }
            DbTx::Sqlite(tx) => {
                let (sql, args) = sqlite_args(sql, params)?;
                Ok(sqlx::query_with(&sql, args)
                    .execute(&mut **tx)
                    .await?
                    .rows_affected())
            }
        }
    }

    /// Fetch exactly one row inside the transaction, mapped via `FromRow`.
    pub async fn query_one<T>(
        &mut self,
        sql: &str,
        params: Vec<DbValue>,
    ) -> Result<T, crate::DbError>
    where
        T: Send + Unpin,
        T: for<'r> FromRow<'r, PgRow>,
        T: for<'r> FromRow<'r, SqliteRow>,
    {
        match self {
            DbTx::Pg(tx) => {
                let args = pg_args(params)?;
                sqlx::query_as_with::<sqlx::Postgres, T, _>(sql, args)
                    .fetch_one(&mut **tx)
                    .await
                    .map_err(Into::into)
            }
            DbTx::Sqlite(tx) => {
                let (sql, args) = sqlite_args(sql, params)?;
                sqlx::query_as_with::<sqlx::Sqlite, T, _>(&sql, args)
                    .fetch_one(&mut **tx)
                    .await
                    .map_err(Into::into)
            }
        }
    }

    /// Fetch at most one row inside the transaction, mapped via `FromRow`.
    pub async fn query_opt<T>(
        &mut self,
        sql: &str,
        params: Vec<DbValue>,
    ) -> Result<Option<T>, crate::DbError>
    where
        T: Send + Unpin,
        T: for<'r> FromRow<'r, PgRow>,
        T: for<'r> FromRow<'r, SqliteRow>,
    {
        match self {
            DbTx::Pg(tx) => {
                let args = pg_args(params)?;
                sqlx::query_as_with::<sqlx::Postgres, T, _>(sql, args)
                    .fetch_optional(&mut **tx)
                    .await
                    .map_err(Into::into)
            }
            DbTx::Sqlite(tx) => {
                let (sql, args) = sqlite_args(sql, params)?;
                sqlx::query_as_with::<sqlx::Sqlite, T, _>(&sql, args)
                    .fetch_optional(&mut **tx)
                    .await
                    .map_err(Into::into)
            }
        }
    }

    /// Fetch a single scalar inside the transaction.
    pub async fn query_scalar<T>(
        &mut self,
        sql: &str,
        params: Vec<DbValue>,
    ) -> Result<T, crate::DbError>
    where
        T: Send + Unpin,
        for<'r> T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
        for<'r> T: sqlx::Decode<'r, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite>,
    {
        match self {
            DbTx::Pg(tx) => {
                let args = pg_args(params)?;
                sqlx::query_scalar_with::<sqlx::Postgres, T, _>(sql, args)
                    .fetch_one(&mut **tx)
                    .await
                    .map_err(Into::into)
            }
            DbTx::Sqlite(tx) => {
                let (sql, args) = sqlite_args(sql, params)?;
                sqlx::query_scalar_with::<sqlx::Sqlite, T, _>(&sql, args)
                    .fetch_one(&mut **tx)
                    .await
                    .map_err(Into::into)
            }
        }
    }

    /// Fetch every row inside the transaction, mapped via `FromRow`.
    pub async fn query_all<T>(
        &mut self,
        sql: &str,
        params: Vec<DbValue>,
    ) -> Result<Vec<T>, crate::DbError>
    where
        T: Send + Unpin,
        T: for<'r> FromRow<'r, PgRow>,
        T: for<'r> FromRow<'r, SqliteRow>,
    {
        match self {
            DbTx::Pg(tx) => {
                let args = pg_args(params)?;
                sqlx::query_as_with::<sqlx::Postgres, T, _>(sql, args)
                    .fetch_all(&mut **tx)
                    .await
                    .map_err(Into::into)
            }
            DbTx::Sqlite(tx) => {
                let (sql, args) = sqlite_args(sql, params)?;
                sqlx::query_as_with::<sqlx::Sqlite, T, _>(&sql, args)
                    .fetch_all(&mut **tx)
                    .await
                    .map_err(Into::into)
            }
        }
    }

    /// Commit the transaction.
    pub async fn commit(self) -> Result<(), crate::DbError> {
        match self {
            DbTx::Pg(tx) => tx.commit().await.map_err(Into::into),
            DbTx::Sqlite(tx) => tx.commit().await.map_err(Into::into),
        }
    }

    /// Roll the transaction back explicitly (dropping it rolls back too).
    pub async fn rollback(self) -> Result<(), crate::DbError> {
        match self {
            DbTx::Pg(tx) => tx.rollback().await.map_err(Into::into),
            DbTx::Sqlite(tx) => tx.rollback().await.map_err(Into::into),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_lists_is_a_passthrough() {
        let (sql, params) = expand_lists(
            "SELECT * FROM t WHERE a = $1 AND b = $2",
            vec![DbValue::I64(Some(1)), DbValue::Text(Some("x".into()))],
        );
        assert_eq!(sql, "SELECT * FROM t WHERE a = $1 AND b = $2");
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn a_list_expands_to_in_and_renumbers_the_tail() {
        let (sql, params) = expand_lists(
            "SELECT * FROM t WHERE tenant = $1 AND name = ANY($2) AND live = $3",
            vec![
                DbValue::Uuid(Some(uuid::Uuid::nil())),
                DbValue::TextList(vec!["a".into(), "b".into(), "c".into()]),
                DbValue::Bool(Some(true)),
            ],
        );
        // `= ANY($2)` becomes `IN ($2, $3, $4)` — the operator changes, not just
        // the placeholder count: `ANY($2, $3, $4)` is not valid SQLite.
        assert_eq!(
            sql,
            "SELECT * FROM t WHERE tenant = $1 AND name IN ($2, $3, $4) AND live = $5"
        );
        assert_eq!(params.len(), 5);
        assert!(matches!(params[0], DbValue::Uuid(_)));
        assert!(matches!(params[1], DbValue::Text(_)));
        assert!(matches!(params[4], DbValue::Bool(_)));
    }

    #[test]
    fn an_empty_list_becomes_in_null() {
        let (sql, params) = expand_lists(
            "SELECT * FROM t WHERE id = ANY($1)",
            vec![DbValue::UuidList(vec![])],
        );
        // `IN (NULL)` is valid SQLite and matches nothing, exactly as an empty
        // `= ANY` does in Postgres.
        assert_eq!(sql, "SELECT * FROM t WHERE id IN (NULL)");
        assert_eq!(params.len(), 0);
    }

    #[test]
    fn two_lists_both_become_in() {
        let (sql, params) = expand_lists(
            "WHERE a = ANY($1) AND b = $2 AND c = ANY($3)",
            vec![
                DbValue::I64List(vec![1, 2]),
                DbValue::Text(Some("m".into())),
                DbValue::TextList(vec!["x".into()]),
            ],
        );
        assert_eq!(sql, "WHERE a IN ($1, $2) AND b = $3 AND c IN ($4)");
        assert_eq!(params.len(), 4);
    }

    #[test]
    fn not_all_becomes_not_in() {
        // The `!= ALL($n)` form (discovery.rs / ws/node.rs) is non-membership.
        let (sql, params) = expand_lists(
            "DELETE FROM t WHERE node = $1 AND path != ALL($2)",
            vec![
                DbValue::Uuid(Some(uuid::Uuid::nil())),
                DbValue::TextList(vec!["a".into(), "b".into()]),
            ],
        );
        assert_eq!(
            sql,
            "DELETE FROM t WHERE node = $1 AND path NOT IN ($2, $3)"
        );
        assert_eq!(params.len(), 3);
    }

    #[test]
    fn angle_all_also_becomes_not_in() {
        let (sql, params) = expand_lists(
            "WHERE tmux <> ALL($1)",
            vec![DbValue::TextList(vec!["x".into()])],
        );
        assert_eq!(sql, "WHERE tmux NOT IN ($1)");
        assert_eq!(params.len(), 1);
    }

    #[test]
    fn no_lists_is_still_a_passthrough_with_any_present() {
        // A bare `ANY` with no list bound to it (e.g. an `OptTextArray` under the
        // Postgres arm) is untouched — only list groups trigger the rewrite.
        let (sql, params) = expand_lists(
            "WHERE x = ANY($1) AND y = ANY($2)",
            vec![
                DbValue::OptTextArray(Some(vec!["a".into()])),
                DbValue::I64List(vec![7]),
            ],
        );
        assert_eq!(sql, "WHERE x = ANY($1) AND y IN ($2)");
        assert_eq!(params.len(), 2);
    }
}
