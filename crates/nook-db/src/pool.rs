//! The engine-dispatching pool (MAIN-205).
//!
//! The owner's ruling (2026-07-28, resolving the builder's escalation) fixed the
//! mechanism: **a custom dispatch API on the pool** — not `sqlx::Any` (it cannot
//! encode `Uuid`, which the codebase binds ~1500 times), and not a bare enum
//! pretending to be an `Executor` (impossible: `Executor::Database` is one
//! concrete associated type). Every query flows through one execution
//! convention here, so "which engine" is a single-file concern forever.
//!
//! The shape: callers pass SQL text plus an owned, engine-neutral parameter list
//! ([`DbValue`]); each arm encodes those parameters with its own driver, and row
//! mapping rides `sqlx::FromRow` over both row types. The Postgres arm reproduces
//! today's binds exactly (bit-identical — MAIN-205 AC-3), so the flip changes the
//! transport shape, not behaviour. The SQLite arm is built to *compile* here; its
//! runtime is proven when the engine and its migration track land in MAIN-196.
//!
//! This module is the foundation the ~656 call-site migration dispatches onto; it
//! is introduced alongside the existing `DbPool` alias and only becomes the pool
//! type at the flip, so each build step stays green.

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
// The `params![…]` sites lean on these so a call reads `params![tenant, id]`
// rather than spelling every variant. UUID newtypes (`TenantId`, `NodeId`, …)
// are `#[sqlx(transparent)]` over `Uuid`, so unwrapping to `Uuid` here encodes
// identically to binding the newtype — the Postgres arm stays bit-identical.

impl From<uuid::Uuid> for DbValue {
    fn from(v: uuid::Uuid) -> Self {
        DbValue::Uuid(Some(v))
    }
}
impl From<Option<uuid::Uuid>> for DbValue {
    fn from(v: Option<uuid::Uuid>) -> Self {
        DbValue::Uuid(v)
    }
}
impl From<String> for DbValue {
    fn from(v: String) -> Self {
        DbValue::Text(Some(v))
    }
}
impl From<&str> for DbValue {
    fn from(v: &str) -> Self {
        DbValue::Text(Some(v.to_owned()))
    }
}
impl From<Option<String>> for DbValue {
    fn from(v: Option<String>) -> Self {
        DbValue::Text(v)
    }
}
impl From<bool> for DbValue {
    fn from(v: bool) -> Self {
        DbValue::Bool(Some(v))
    }
}
impl From<i16> for DbValue {
    fn from(v: i16) -> Self {
        DbValue::I16(Some(v))
    }
}
impl From<i32> for DbValue {
    fn from(v: i32) -> Self {
        DbValue::I32(Some(v))
    }
}
impl From<i64> for DbValue {
    fn from(v: i64) -> Self {
        DbValue::I64(Some(v))
    }
}
impl From<chrono::DateTime<chrono::Utc>> for DbValue {
    fn from(v: chrono::DateTime<chrono::Utc>) -> Self {
        DbValue::Timestamptz(Some(v))
    }
}
impl From<serde_json::Value> for DbValue {
    fn from(v: serde_json::Value) -> Self {
        DbValue::Json(Some(v))
    }
}
impl From<Vec<String>> for DbValue {
    fn from(v: Vec<String>) -> Self {
        DbValue::TextList(v)
    }
}
impl From<Vec<uuid::Uuid>> for DbValue {
    fn from(v: Vec<uuid::Uuid>) -> Self {
        DbValue::UuidList(v)
    }
}

/// Build a `Vec<DbValue>` from a heterogeneous parameter list, in bind order.
///
/// ```ignore
/// db.fetch_one::<Workspace>(
///     "SELECT * FROM workspaces WHERE tenant_id = $1 AND id = $2",
///     params![tenant, id],
/// ).await?
/// ```
#[macro_export]
macro_rules! params {
    () => { ::std::vec::Vec::<$crate::DbValue>::new() };
    ($($v:expr),+ $(,)?) => { ::std::vec![$($crate::DbValue::from($v)),+] };
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
            // Lists were flattened away by expand_lists; a list here is a bug.
            DbValue::TextList(_) | DbValue::UuidList(_) | DbValue::I64List(_) => {
                unreachable!("expand_lists flattens every list parameter")
            }
        }
    }
    Ok((rewritten, a))
}

/// Rewrite `$1,$2,…`-numbered SQL so each list parameter's single placeholder
/// becomes a parenthesised group of scalar placeholders, renumbering the rest,
/// and flatten the parameters to match. Pure and engine-agnostic; the SQLite arm
/// is its only user today.
fn expand_lists(sql: &str, params: Vec<DbValue>) -> (String, Vec<DbValue>) {
    // Fast path: nothing to expand.
    if !params.iter().any(DbValue::is_list) {
        return (sql.to_owned(), params);
    }

    // Map each original 1-based parameter index to the run of new indices it
    // occupies after expansion, and build the flattened parameter vector.
    let mut flat: Vec<DbValue> = Vec::new();
    let mut groups: Vec<Vec<usize>> = Vec::with_capacity(params.len());
    let mut next = 1usize;
    for p in params {
        match p {
            DbValue::TextList(xs) => {
                let idxs = xs
                    .into_iter()
                    .map(|x| {
                        flat.push(DbValue::Text(Some(x)));
                        let i = next;
                        next += 1;
                        i
                    })
                    .collect();
                groups.push(idxs);
            }
            DbValue::UuidList(xs) => {
                let idxs = xs
                    .into_iter()
                    .map(|x| {
                        flat.push(DbValue::Uuid(Some(x)));
                        let i = next;
                        next += 1;
                        i
                    })
                    .collect();
                groups.push(idxs);
            }
            DbValue::I64List(xs) => {
                let idxs = xs
                    .into_iter()
                    .map(|x| {
                        flat.push(DbValue::I64(Some(x)));
                        let i = next;
                        next += 1;
                        i
                    })
                    .collect();
                groups.push(idxs);
            }
            scalar => {
                flat.push(scalar);
                groups.push(vec![next]);
                next += 1;
            }
        }
    }

    // Replace each `$k` token in the SQL with its group's rendering. A list group
    // renders as `$a, $b, …` (the caller's surrounding parentheses / `IN` stay);
    // an empty list renders `NULL` so `IN (NULL)` matches nothing, mirroring the
    // empty-`ANY` truth table.
    let rewritten = replace_placeholders(sql, |k| {
        let g = &groups[k - 1];
        if g.is_empty() {
            "NULL".to_owned()
        } else {
            g.iter()
                .map(|i| format!("${i}"))
                .collect::<Vec<_>>()
                .join(", ")
        }
    });
    (rewritten, flat)
}

/// Walk `sql` replacing each `$N` placeholder (1-based) using `render`. Only
/// bare `$<digits>` tokens are touched; `$$` and dollar-quoted bodies are not a
/// concern in this codebase's parameterised statements.
fn replace_placeholders(sql: &str, mut render: impl FnMut(usize) -> String) -> String {
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
            out.push_str(&render(n));
            i = j;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

// ── the pool ─────────────────────────────────────────────────────────────────

/// The engine-dispatching pool. Cheap to clone (each arm is an `sqlx` pool
/// handle). At the MAIN-205 flip this becomes the workspace's `DbPool`.
#[derive(Clone)]
pub struct EnginePool(Arm);

#[derive(Clone)]
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

    /// Which engine this pool dispatches to.
    pub fn engine(&self) -> Engine {
        match self.0 {
            Arm::Pg(_) => Engine::Postgres,
            Arm::Sqlite(_) => Engine::Sqlite,
        }
    }

    /// Run a statement, returning the number of affected rows. For `RETURNING`
    /// statements use [`fetch_one`](Self::fetch_one) / friends instead.
    pub async fn execute(&self, sql: &str, params: Vec<DbValue>) -> Result<u64, sqlx::Error> {
        match &self.0 {
            Arm::Pg(p) => {
                let args = pg_args(params)?;
                let r = sqlx::query_with(sql, args).execute(p).await?;
                Ok(r.rows_affected())
            }
            Arm::Sqlite(p) => {
                let (sql, args) = sqlite_args(sql, params)?;
                let r = sqlx::query_with(&sql, args).execute(p).await?;
                Ok(r.rows_affected())
            }
        }
    }

    /// Fetch exactly one row, mapped via `FromRow`.
    pub async fn fetch_one<T>(&self, sql: &str, params: Vec<DbValue>) -> Result<T, sqlx::Error>
    where
        T: Send + Unpin,
        T: for<'r> FromRow<'r, PgRow>,
        T: for<'r> FromRow<'r, SqliteRow>,
    {
        match &self.0 {
            Arm::Pg(p) => {
                let args = pg_args(params)?;
                sqlx::query_as_with::<sqlx::Postgres, T, _>(sql, args)
                    .fetch_one(p)
                    .await
            }
            Arm::Sqlite(p) => {
                let (sql, args) = sqlite_args(sql, params)?;
                sqlx::query_as_with::<sqlx::Sqlite, T, _>(&sql, args)
                    .fetch_one(p)
                    .await
            }
        }
    }

    /// Fetch at most one row, mapped via `FromRow`.
    pub async fn fetch_optional<T>(
        &self,
        sql: &str,
        params: Vec<DbValue>,
    ) -> Result<Option<T>, sqlx::Error>
    where
        T: Send + Unpin,
        T: for<'r> FromRow<'r, PgRow>,
        T: for<'r> FromRow<'r, SqliteRow>,
    {
        match &self.0 {
            Arm::Pg(p) => {
                let args = pg_args(params)?;
                sqlx::query_as_with::<sqlx::Postgres, T, _>(sql, args)
                    .fetch_optional(p)
                    .await
            }
            Arm::Sqlite(p) => {
                let (sql, args) = sqlite_args(sql, params)?;
                sqlx::query_as_with::<sqlx::Sqlite, T, _>(&sql, args)
                    .fetch_optional(p)
                    .await
            }
        }
    }

    /// Fetch every row, mapped via `FromRow`.
    pub async fn fetch_all<T>(&self, sql: &str, params: Vec<DbValue>) -> Result<Vec<T>, sqlx::Error>
    where
        T: Send + Unpin,
        T: for<'r> FromRow<'r, PgRow>,
        T: for<'r> FromRow<'r, SqliteRow>,
    {
        match &self.0 {
            Arm::Pg(p) => {
                let args = pg_args(params)?;
                sqlx::query_as_with::<sqlx::Postgres, T, _>(sql, args)
                    .fetch_all(p)
                    .await
            }
            Arm::Sqlite(p) => {
                let (sql, args) = sqlite_args(sql, params)?;
                sqlx::query_as_with::<sqlx::Sqlite, T, _>(&sql, args)
                    .fetch_all(p)
                    .await
            }
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
    fn a_list_expands_and_renumbers_the_tail() {
        // $2 is a 3-element list; the trailing $3 must slide to $5.
        let (sql, params) = expand_lists(
            "SELECT * FROM t WHERE tenant = $1 AND name = ANY($2) AND live = $3",
            vec![
                DbValue::Uuid(Some(uuid::Uuid::nil())),
                DbValue::TextList(vec!["a".into(), "b".into(), "c".into()]),
                DbValue::Bool(Some(true)),
            ],
        );
        assert_eq!(
            sql,
            "SELECT * FROM t WHERE tenant = $1 AND name = ANY($2, $3, $4) AND live = $5"
        );
        // 1 uuid + 3 texts + 1 bool, in order.
        assert_eq!(params.len(), 5);
        assert!(matches!(params[0], DbValue::Uuid(_)));
        assert!(matches!(params[1], DbValue::Text(_)));
        assert!(matches!(params[4], DbValue::Bool(_)));
    }

    #[test]
    fn an_empty_list_renders_null() {
        let (sql, params) = expand_lists(
            "SELECT * FROM t WHERE id = ANY($1)",
            vec![DbValue::UuidList(vec![])],
        );
        assert_eq!(sql, "SELECT * FROM t WHERE id = ANY(NULL)");
        assert_eq!(params.len(), 0);
    }

    #[test]
    fn two_lists_both_expand() {
        let (sql, params) = expand_lists(
            "WHERE a = ANY($1) AND b = $2 AND c = ANY($3)",
            vec![
                DbValue::I64List(vec![1, 2]),
                DbValue::Text(Some("m".into())),
                DbValue::TextList(vec!["x".into()]),
            ],
        );
        assert_eq!(sql, "WHERE a = ANY($1, $2) AND b = $3 AND c = ANY($4)");
        assert_eq!(params.len(), 4);
    }
}
