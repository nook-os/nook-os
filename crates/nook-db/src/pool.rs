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
        }
    }
    Ok((rewritten, a))
}

/// Rewrite `$1,$2,…`-numbered SQL so each list parameter's single placeholder
/// becomes a parenthesised group of scalar placeholders, renumbering the rest,
/// and flatten the parameters to match. Pure and engine-agnostic; the SQLite arm
/// is its only user today.
fn expand_lists(sql: &str, params: Vec<DbValue>) -> (String, Vec<DbValue>) {
    if !params.iter().any(DbValue::is_list) {
        return (sql.to_owned(), params);
    }

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

/// Walk `sql` replacing each `$N` placeholder (1-based) using `render`.
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

// ── the dispatch surface ─────────────────────────────────────────────────────

/// The query surface every call site routes through. Named to avoid sqlx's
/// `Executor` methods (`query_one`, never `fetch_one`) so it can be implemented
/// for `PgPool` during the migration and for [`EnginePool`] after the flip, with
/// call sites unchanged across it.
#[allow(async_fn_in_trait)]
pub trait Db {
    /// Run a statement, returning affected rows. For `RETURNING`, use the
    /// `query_*` fetchers instead.
    async fn exec(&self, sql: &str, params: Vec<DbValue>) -> Result<u64, sqlx::Error>;

    /// Fetch exactly one row, mapped via `FromRow`.
    async fn query_one<T>(&self, sql: &str, params: Vec<DbValue>) -> Result<T, sqlx::Error>
    where
        T: Send + Unpin,
        T: for<'r> FromRow<'r, PgRow>,
        T: for<'r> FromRow<'r, SqliteRow>;

    /// Fetch at most one row, mapped via `FromRow`.
    async fn query_opt<T>(&self, sql: &str, params: Vec<DbValue>) -> Result<Option<T>, sqlx::Error>
    where
        T: Send + Unpin,
        T: for<'r> FromRow<'r, PgRow>,
        T: for<'r> FromRow<'r, SqliteRow>;

    /// Fetch every row, mapped via `FromRow`.
    async fn query_all<T>(&self, sql: &str, params: Vec<DbValue>) -> Result<Vec<T>, sqlx::Error>
    where
        T: Send + Unpin,
        T: for<'r> FromRow<'r, PgRow>,
        T: for<'r> FromRow<'r, SqliteRow>;

    /// Fetch a single scalar (one column of one row), e.g. `SELECT count(*)`.
    async fn query_scalar<T>(&self, sql: &str, params: Vec<DbValue>) -> Result<T, sqlx::Error>
    where
        T: Send + Unpin,
        for<'r> T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
        for<'r> T: sqlx::Decode<'r, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite>;
}

impl Db for PgPool {
    async fn exec(&self, sql: &str, params: Vec<DbValue>) -> Result<u64, sqlx::Error> {
        let args = pg_args(params)?;
        Ok(sqlx::query_with(sql, args)
            .execute(self)
            .await?
            .rows_affected())
    }
    async fn query_one<T>(&self, sql: &str, params: Vec<DbValue>) -> Result<T, sqlx::Error>
    where
        T: Send + Unpin,
        T: for<'r> FromRow<'r, PgRow>,
        T: for<'r> FromRow<'r, SqliteRow>,
    {
        let args = pg_args(params)?;
        sqlx::query_as_with::<sqlx::Postgres, T, _>(sql, args)
            .fetch_one(self)
            .await
    }
    async fn query_opt<T>(&self, sql: &str, params: Vec<DbValue>) -> Result<Option<T>, sqlx::Error>
    where
        T: Send + Unpin,
        T: for<'r> FromRow<'r, PgRow>,
        T: for<'r> FromRow<'r, SqliteRow>,
    {
        let args = pg_args(params)?;
        sqlx::query_as_with::<sqlx::Postgres, T, _>(sql, args)
            .fetch_optional(self)
            .await
    }
    async fn query_all<T>(&self, sql: &str, params: Vec<DbValue>) -> Result<Vec<T>, sqlx::Error>
    where
        T: Send + Unpin,
        T: for<'r> FromRow<'r, PgRow>,
        T: for<'r> FromRow<'r, SqliteRow>,
    {
        let args = pg_args(params)?;
        sqlx::query_as_with::<sqlx::Postgres, T, _>(sql, args)
            .fetch_all(self)
            .await
    }
    async fn query_scalar<T>(&self, sql: &str, params: Vec<DbValue>) -> Result<T, sqlx::Error>
    where
        T: Send + Unpin,
        for<'r> T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
        for<'r> T: sqlx::Decode<'r, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite>,
    {
        let args = pg_args(params)?;
        sqlx::query_scalar_with::<sqlx::Postgres, T, _>(sql, args)
            .fetch_one(self)
            .await
    }
}

impl Db for SqlitePool {
    async fn exec(&self, sql: &str, params: Vec<DbValue>) -> Result<u64, sqlx::Error> {
        let (sql, args) = sqlite_args(sql, params)?;
        Ok(sqlx::query_with(&sql, args)
            .execute(self)
            .await?
            .rows_affected())
    }
    async fn query_one<T>(&self, sql: &str, params: Vec<DbValue>) -> Result<T, sqlx::Error>
    where
        T: Send + Unpin,
        T: for<'r> FromRow<'r, PgRow>,
        T: for<'r> FromRow<'r, SqliteRow>,
    {
        let (sql, args) = sqlite_args(sql, params)?;
        sqlx::query_as_with::<sqlx::Sqlite, T, _>(&sql, args)
            .fetch_one(self)
            .await
    }
    async fn query_opt<T>(&self, sql: &str, params: Vec<DbValue>) -> Result<Option<T>, sqlx::Error>
    where
        T: Send + Unpin,
        T: for<'r> FromRow<'r, PgRow>,
        T: for<'r> FromRow<'r, SqliteRow>,
    {
        let (sql, args) = sqlite_args(sql, params)?;
        sqlx::query_as_with::<sqlx::Sqlite, T, _>(&sql, args)
            .fetch_optional(self)
            .await
    }
    async fn query_all<T>(&self, sql: &str, params: Vec<DbValue>) -> Result<Vec<T>, sqlx::Error>
    where
        T: Send + Unpin,
        T: for<'r> FromRow<'r, PgRow>,
        T: for<'r> FromRow<'r, SqliteRow>,
    {
        let (sql, args) = sqlite_args(sql, params)?;
        sqlx::query_as_with::<sqlx::Sqlite, T, _>(&sql, args)
            .fetch_all(self)
            .await
    }
    async fn query_scalar<T>(&self, sql: &str, params: Vec<DbValue>) -> Result<T, sqlx::Error>
    where
        T: Send + Unpin,
        for<'r> T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
        for<'r> T: sqlx::Decode<'r, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite>,
    {
        let (sql, args) = sqlite_args(sql, params)?;
        sqlx::query_scalar_with::<sqlx::Sqlite, T, _>(&sql, args)
            .fetch_one(self)
            .await
    }
}

/// The engine-dispatching pool. Cheap to clone (each arm is an `sqlx` pool
/// handle). At the MAIN-205 flip this becomes the workspace's `DbPool`; its [`Db`]
/// impl simply forwards to whichever concrete pool it holds.
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
    async fn exec(&self, sql: &str, params: Vec<DbValue>) -> Result<u64, sqlx::Error> {
        match &self.0 {
            Arm::Pg(p) => p.exec(sql, params).await,
            Arm::Sqlite(p) => p.exec(sql, params).await,
        }
    }
    async fn query_one<T>(&self, sql: &str, params: Vec<DbValue>) -> Result<T, sqlx::Error>
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
    async fn query_opt<T>(&self, sql: &str, params: Vec<DbValue>) -> Result<Option<T>, sqlx::Error>
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
    async fn query_all<T>(&self, sql: &str, params: Vec<DbValue>) -> Result<Vec<T>, sqlx::Error>
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
    async fn query_scalar<T>(&self, sql: &str, params: Vec<DbValue>) -> Result<T, sqlx::Error>
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
    pub async fn exec(&mut self, sql: &str, params: Vec<DbValue>) -> Result<u64, sqlx::Error> {
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
    pub async fn query_one<T>(&mut self, sql: &str, params: Vec<DbValue>) -> Result<T, sqlx::Error>
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
            }
            DbTx::Sqlite(tx) => {
                let (sql, args) = sqlite_args(sql, params)?;
                sqlx::query_as_with::<sqlx::Sqlite, T, _>(&sql, args)
                    .fetch_one(&mut **tx)
                    .await
            }
        }
    }

    /// Fetch at most one row inside the transaction, mapped via `FromRow`.
    pub async fn query_opt<T>(
        &mut self,
        sql: &str,
        params: Vec<DbValue>,
    ) -> Result<Option<T>, sqlx::Error>
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
            }
            DbTx::Sqlite(tx) => {
                let (sql, args) = sqlite_args(sql, params)?;
                sqlx::query_as_with::<sqlx::Sqlite, T, _>(&sql, args)
                    .fetch_optional(&mut **tx)
                    .await
            }
        }
    }

    /// Fetch a single scalar inside the transaction.
    pub async fn query_scalar<T>(
        &mut self,
        sql: &str,
        params: Vec<DbValue>,
    ) -> Result<T, sqlx::Error>
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
            }
            DbTx::Sqlite(tx) => {
                let (sql, args) = sqlite_args(sql, params)?;
                sqlx::query_scalar_with::<sqlx::Sqlite, T, _>(&sql, args)
                    .fetch_one(&mut **tx)
                    .await
            }
        }
    }

    /// Commit the transaction.
    pub async fn commit(self) -> Result<(), sqlx::Error> {
        match self {
            DbTx::Pg(tx) => tx.commit().await,
            DbTx::Sqlite(tx) => tx.commit().await,
        }
    }

    /// Roll the transaction back explicitly (dropping it rolls back too).
    pub async fn rollback(self) -> Result<(), sqlx::Error> {
        match self {
            DbTx::Pg(tx) => tx.rollback().await,
            DbTx::Sqlite(tx) => tx.rollback().await,
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
