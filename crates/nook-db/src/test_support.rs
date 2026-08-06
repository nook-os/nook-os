//! Database LIFECYCLE — creating and destroying whole databases (MAIN-429).
//!
//! Everywhere else in this crate manages CONNECTIONS to a database that already
//! exists. This manages the databases themselves, which is a different job with
//! a hard constraint: `CREATE DATABASE` cannot run against the database being
//! created, so it can never go through [`crate::pool::EnginePool`]. That is why
//! `nook-testkit` held raw `sqlx` for so long, and why the de-sqlx chain could
//! not finish while it did.
//!
//! **Behind the `test-support` feature, off by default** — the same convention
//! `nook-infra` uses for `Config::for_test`. A release build of `nook-control`
//! or `nook-chat` does not compile a line of this: database administration is
//! not something a shipped binary should be able to do by accident.
//!
//! What lives here is MECHANISM only. Every policy decision — which template to
//! reuse, how old is stale, what to name things, when to reap — stays in
//! `nook-testkit`, because policy needs `nook_control::MIGRATOR` and
//! `nook-control` depends on this crate. Moving it would be a dependency cycle,
//! not a refactor.

use std::path::{Path, PathBuf};

use sqlx::{Connection, PgConnection, PgPool};

use crate::pool::EnginePool;
use crate::DbError;

fn query(e: sqlx::Error) -> DbError {
    DbError::Query(e)
}

/// An admin connection to a Postgres server, held open against a database
/// OTHER than the ones it manages.
///
/// Opaque on purpose (AC-2): the `sqlx` connection is private and never appears
/// in a signature, so a caller cannot reach past this surface and run arbitrary
/// statements — which is what the sqlx-signature guard is for.
pub struct AdminConn {
    conn: PgConnection,
}

impl AdminConn {
    /// Connect to the base database named by `base_url`.
    pub async fn connect(base_url: &str) -> Result<Self, DbError> {
        let conn = PgConnection::connect(base_url)
            .await
            .map_err(DbError::Connect)?;
        Ok(Self { conn })
    }

    /// Close it. Best-effort: a failure to hang up cannot fail a test run.
    pub async fn close(self) {
        self.conn.close().await.ok();
    }

    /// Take the session-level advisory lock `key`, serialising every process
    /// that manages the shared template set.
    pub async fn advisory_lock(&mut self, key: i64) -> Result<(), DbError> {
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(key)
            .execute(&mut self.conn)
            .await
            .map_err(query)?;
        Ok(())
    }

    /// Release it. Best-effort, exactly as the code this replaces was: the
    /// connection closing drops the lock anyway.
    pub async fn advisory_unlock(&mut self, key: i64) {
        sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(key)
            .execute(&mut self.conn)
            .await
            .ok();
    }

    /// The highest-sorting database name matching `pattern` (a SQL `LIKE`).
    ///
    /// `ORDER BY datname DESC LIMIT 1` — with the epoch as the name's suffix
    /// that is the most recently built one, which is the template a caller
    /// wants to reuse.
    pub async fn latest_database_like(&mut self, pattern: &str) -> Result<Option<String>, DbError> {
        sqlx::query_scalar(
            "SELECT datname FROM pg_database
              WHERE datname LIKE $1 ORDER BY datname DESC LIMIT 1",
        )
        .bind(pattern)
        .fetch_optional(&mut self.conn)
        .await
        .map_err(query)
    }

    pub async fn create_database(&mut self, name: &str) -> Result<(), DbError> {
        sqlx::query(&format!("CREATE DATABASE \"{name}\""))
            .execute(&mut self.conn)
            .await
            .map_err(query)?;
        Ok(())
    }

    /// `CREATE DATABASE … TEMPLATE …` — a file copy, so the new database
    /// arrives already carrying the template's schema and rows.
    pub async fn create_database_from_template(
        &mut self,
        name: &str,
        template: &str,
    ) -> Result<(), DbError> {
        sqlx::query(&format!(
            "CREATE DATABASE \"{name}\" TEMPLATE \"{template}\""
        ))
        .execute(&mut self.conn)
        .await
        .map_err(query)?;
        Ok(())
    }

    /// Databases matching `pattern` that nothing is connected to, excluding
    /// `except`.
    ///
    /// The `pg_stat_activity` half is the direct test for "in use", and it is
    /// the guard that stops a reaper dropping a template another suite is
    /// cloning from. Without it this scan turns cleanup into the failure.
    pub async fn unused_databases_like(
        &mut self,
        pattern: &str,
        except: &str,
    ) -> Result<Vec<String>, DbError> {
        sqlx::query_scalar::<_, String>(
            "SELECT d.datname FROM pg_database d
              WHERE d.datname LIKE $1
                AND d.datname <> $2
                AND NOT EXISTS (
                      SELECT 1 FROM pg_stat_activity a WHERE a.datname = d.datname)",
        )
        .bind(pattern)
        .bind(except)
        .fetch_all(&mut self.conn)
        .await
        .map_err(query)
    }

    /// `WITH (FORCE)` terminates any stragglers (Postgres 13+; the dev/CI
    /// Postgres is 16). That is what lets a bed drop its database without first
    /// closing a pool whose connections belong to a frozen runtime.
    pub async fn drop_database(&mut self, name: &str) -> Result<(), DbError> {
        sqlx::query(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
            .execute(&mut self.conn)
            .await
            .map_err(query)?;
        Ok(())
    }

    /// Straight from the catalogue.
    ///
    /// Deliberately its own statement rather than anything shared with
    /// [`AdminConn::drop_database`] (AC-4): this is what teardown is ASSERTED
    /// with, so a bug that made the drop a no-op must not also make the check
    /// report absence and agree with it.
    pub async fn database_exists(&mut self, name: &str) -> Result<bool, DbError> {
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_database WHERE datname = $1")
            .bind(name)
            .fetch_one(&mut self.conn)
            .await
            .map_err(query)?;
        Ok(n > 0)
    }
}

/// What a bed created, and therefore what teardown has to undo.
///
/// The two engines have nothing in common here: Postgres owns a database on a
/// server reachable only through an admin connection, SQLite owns a file.
pub enum Provisioned {
    Pg {
        /// `DATABASE_URL` — the server + base database, used for the admin
        /// `CREATE`/`DROP DATABASE` statements (which cannot run against the
        /// target).
        base_url: String,
        /// The unique database that was created (e.g. `nook_test_<uuid>`).
        db_name: String,
    },
    Sqlite {
        /// The unique file that was created.
        path: PathBuf,
    },
}

/// Destroy it. Best-effort and infallible by design — a failure to tidy up must
/// never fail a test run.
pub async fn destroy(what: &Provisioned) {
    match what {
        Provisioned::Pg { base_url, db_name } => {
            let Ok(mut admin) = AdminConn::connect(base_url).await else {
                return;
            };
            admin.drop_database(db_name).await.ok();
            admin.close().await;
        }
        Provisioned::Sqlite { path } => remove_sqlite_files(path),
    }
}

/// Is it still there?
///
/// Shares no code path with [`destroy`] (AC-4) — Postgres reads the catalogue,
/// SQLite stats the file. That independence is the point: this is the assertion
/// teardown is proven with.
pub async fn exists(what: &Provisioned) -> bool {
    match what {
        Provisioned::Pg { base_url, db_name } => {
            let Ok(mut admin) = AdminConn::connect(base_url).await else {
                return false;
            };
            let found = admin.database_exists(db_name).await.unwrap_or(false);
            admin.close().await;
            found
        }
        Provisioned::Sqlite { path } => path.exists(),
    }
}

/// Open a pool onto it.
pub async fn open(what: &Provisioned) -> Result<EnginePool, DbError> {
    match what {
        Provisioned::Pg { base_url, db_name } => {
            let pool = PgPool::connect(&swap_db(base_url, db_name))
                .await
                .map_err(DbError::Connect)?;
            Ok(EnginePool::from_pg(pool))
        }
        Provisioned::Sqlite { path } => open_sqlite_bed(path).await,
    }
}

/// The SQLite bed's pool (MAIN-295).
///
/// Built here rather than through [`crate::connect`] because that function pins
/// SQLite to **one** connection deliberately — and, being production's entry
/// point, it should keep doing so until someone decides otherwise. (It ignores
/// its own `max_connections` argument on the SQLite arm, so passing a bigger
/// number there would silently do nothing; noted, not changed here.)
///
/// A pool of one is a deadlock waiting for a caller: hold a connection — an
/// open transaction, a `fetch` still streaming — and ask for a second, and the
/// second waits for the first, which waits for the caller, until the acquire
/// gives up as `PoolTimedOut`. The Postgres bed has never had this shape, so a
/// test that passes there fails here for a reason that has nothing to do with
/// the test.
///
/// ## Why more than one connection is safe here
///
/// The worry with a wider SQLite pool is trading `PoolTimedOut` for `database
/// is locked`. Two things stop that, and a third was tried and dropped:
///
/// * **`busy_timeout`** — SQLite serialises WRITERS. Without a timeout the loser
///   of a write race fails immediately with `database is locked`; with one it
///   waits. This is what turns a pool of writers from an error generator into a
///   queue, and it is why the bump is safe rather than merely bigger.
/// * **A bounded size** — five, not fifty. A test needing a sixth concurrent
///   connection is doing something a test should not, and a small ceiling keeps
///   that visible instead of hiding it behind a large pool.
/// * **WAL was tried and is NOT set.** The card offers it for the case where a
///   pool bump alone is insufficient; measured here, the bump is sufficient. No
///   test in this file distinguishes WAL from the default journal — under the
///   rollback journal an uncommitted writer holds only a RESERVED lock, so
///   readers proceed anyway, and the window WAL actually removes (a reader
///   during COMMIT) is a race with no deterministic test. Rather than ship a
///   setting behind a justification nothing checks, it is left off; if a real
///   contention problem shows up, it is one line and it should arrive with the
///   failing case that motivated it.
///
/// `foreign_keys` is carried over from [`crate::connect`] deliberately: the beds
/// enforce foreign keys today, and quietly dropping that here would change what
/// every existing test proves.
pub async fn open_sqlite_bed(path: &Path) -> Result<EnginePool, DbError> {
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .foreign_keys(true)
        .busy_timeout(std::time::Duration::from_secs(10));

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(opts)
        .await
        .map_err(DbError::Connect)?;
    Ok(EnginePool::from_sqlite(pool))
}

/// Remove a SQLite database and the sidecars sqlx leaves beside it. The `-wal`
/// and `-shm` files are not optional housekeeping: leaving them next to a
/// deleted database is how a later run finds a half-state, and they are the
/// difference between "the file is gone" and "the database is gone".
pub fn remove_sqlite_files(path: &Path) {
    let _ = std::fs::remove_file(path);
    for ext in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{ext}", path.display()));
    }
}

/// A connection URL aimed at what was provisioned, so a caller can open an
/// ADDITIONAL pool against it with its own options.
///
/// `None` on SQLite: one file is one namespace, so there is no second
/// configuration anything could ask for. Here rather than in the harness
/// because it is [`swap_db`]'s only caller outside [`open`], and duplicating
/// that rewrite is how the two would drift.
pub fn database_url(what: &Provisioned) -> Option<String> {
    match what {
        Provisioned::Pg { base_url, db_name } => Some(swap_db(base_url, db_name)),
        Provisioned::Sqlite { .. } => None,
    }
}

/// Rewrite the database segment of a Postgres URL, preserving any query params.
fn swap_db(base: &str, db: &str) -> String {
    let (scheme, rest) = base.split_once("://").unwrap_or(("postgres", base));
    let (authority_and_db, params) = match rest.split_once('?') {
        Some((a, p)) => (a, Some(p)),
        None => (rest, None),
    };
    let authority = authority_and_db
        .rsplit_once('/')
        .map(|(a, _)| a)
        .unwrap_or(authority_and_db);
    let mut out = format!("{scheme}://{authority}/{db}");
    if let Some(p) = params {
        out.push('?');
        out.push_str(p);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swap_db_rewrites_only_the_database_segment() {
        assert_eq!(
            swap_db("postgres://nook:nook@postgres:5432/nook", "nook_test_1"),
            "postgres://nook:nook@postgres:5432/nook_test_1"
        );
    }

    #[test]
    fn swap_db_preserves_query_params() {
        assert_eq!(
            swap_db("postgres://u:p@h:5432/nook?sslmode=require", "nook_test_1"),
            "postgres://u:p@h:5432/nook_test_1?sslmode=require"
        );
    }
}
