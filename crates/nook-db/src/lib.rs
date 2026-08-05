//! The one pool type the whole workspace routes through (MAIN-195 groundwork).
//!
//! Today `DbPool` is *exactly* `sqlx::PgPool`, so this crate changes no behavior
//! at all. Its only purpose is to be the SINGLE pivot for the engine-by-URL work
//! (MAIN-195): when the pool abstraction lands (AC-1), only this one definition
//! changes — the ~200 signatures that name `DbPool` across the DB-touching crates
//! do not move again, and neither does the query surface the sweep (item 4)
//! rewrites.
//!
//! It lives in its own base crate rather than in `nook-infra` (where the ticket
//! first placed it) because the crates that need the pool type — `nook-chat` and
//! `nook-auth` — do not, and should not, depend on nook-infra's redis / queue /
//! storage / mailer stack. A tiny leaf crate everyone can share is the only home
//! that yields a genuinely single pivot.
//!
//! Concrete-Postgres forms that are NOT the pool type — `PgPoolOptions` (pool
//! construction), `sqlx::Postgres` (the DB generic on transactions / `query_as`),
//! `PgConnection` — are deliberately left alone here; they are the engine
//! mechanism PR-B replaces, not this seam.

/// The workspace-wide database pool type (MAIN-205): the engine-dispatching
/// [`EnginePool`], which carries the [`Db`] query surface and hides whether it is
/// backed by Postgres or SQLite. This is the pivot the whole workspace routes
/// through — the ~656 executor sites call `db.query_*`/`exec`/`begin`, never a
/// concrete `PgPool`. Boot construction runs through [`connect`]; migrations and
/// the few raw-pool boot paths reach the Postgres arm via [`EnginePool::pg`].
pub type DbPool = pool::EnginePool;

/// The engine seams (MAIN-198): atomic-claim / json / type-mapping / event-bus
/// traits + the Postgres arm, for the coming dialect sweep to dispatch on.
pub mod dialect;
pub use dialect::{
    AtomicClaim, CiMatch, EventBus, Json, PgEventBus, Postgres, Sqlite, TimeMath, TypeMapping,
};

/// The engine-dispatching pool + parameter model (MAIN-205). Introduced
/// alongside the `DbPool` alias; it becomes the pool type at the call-site flip.
pub mod pool;
pub use pool::{Db, DbTx, DbValue, EnginePool, IntoDbValue};

/// Engine-neutral row mapping (MAIN-327) — what the `Db` fetchers bind on, in
/// place of `sqlx::FromRow` over both engines' row types.
pub mod row;
pub use nook_db_derive::FromDbRow;
pub use row::{DbRow, FromDbColumn, FromDbRow};

/// The pagination contract's server half (QOL sprint 2026-08): opaque cursors,
/// validated page arguments, and the one SQL skeleton every paginated list
/// shares. A repository adopts it by declaring a [`paging::ListSpec`]; fakes
/// inherit identical semantics from [`paging::page_vec`].
pub mod paging;

/// Boot-time migration runner with dev tolerance for a ledger ahead of the
/// checked-out migration set (MAIN-224). Both services' boot paths run through
/// [`migrate::run_with_dev_tolerance`], so they get identical treatment.
pub mod migrate;

/// Boot-time collapse of a pre-squash migration ledger (MAIN-235). The image
/// that ships a squash performs its own re-stamp, so the ordering that caused
/// the documented prod near-miss cannot be gotten wrong by an operator.
pub mod restamp;

/// One control plane per SQLite database file (MAIN-197). SQLite is
/// single-writer and the decided limit is one instance per file; this refuses a
/// second one at boot instead of letting the two race. Postgres takes no lock.
pub mod single_instance;
pub use single_instance::{acquire_for_url as acquire_single_instance_lock, InstanceLock};

use std::fmt;

/// The database engine, selected from the `DATABASE_URL` scheme (MAIN-195). This
/// is the *detected* engine; whether this build can actually run it is a separate
/// question ([`connect`] refuses SQLite until MAIN-196 lands the driver + track).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Postgres,
    Sqlite,
}

impl fmt::Display for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Engine::Postgres => "postgres",
            Engine::Sqlite => "sqlite",
        })
    }
}

/// Why the database could not be selected or opened at boot.
#[derive(Debug)]
pub enum DbError {
    /// The URL's scheme is not one we recognize (e.g. `mysql://`).
    UnsupportedScheme(String),
    /// A recognized engine this build cannot run yet — SQLite, until MAIN-196
    /// adds the driver and the migration track.
    NotYetSupported(Engine),
    /// The engine was fine, but opening the pool failed.
    Connect(sqlx::Error),
    /// A query failed (MAIN-269).
    ///
    /// This is what makes `DbError` the type the [`pool::Db`] trait returns, so
    /// callers stop naming `sqlx::Error` in their own signatures. The driver's
    /// error is kept inside rather than flattened: the two things callers
    /// actually branch on are exposed as predicates below, and everything else
    /// about a failed query is diagnostic text nobody matches on.
    Query(sqlx::Error),
}

impl From<sqlx::Error> for DbError {
    fn from(e: sqlx::Error) -> Self {
        DbError::Query(e)
    }
}

/// Is this driver error the transport, or the database's own answer?
///
/// One function so [`DbError::is_transient`]'s two arms cannot drift: the rule
/// is the same whether the failure happened while connecting or while querying,
/// and the first cut of that predicate got it wrong precisely by having two
/// rules.
fn driver_error_is_transient(e: &sqlx::Error) -> bool {
    match e {
        // Nobody answered, or the pool could not hand out a connection in time.
        // Availability, not intent.
        sqlx::Error::Io(_) | sqlx::Error::PoolTimedOut => true,
        // The peer closed the connection or spoke unexpectedly — a failover or
        // a restart, from the client's point of view.
        sqlx::Error::Protocol(_) => true,
        // The database ITSELF answered. It knows what it does not have, so this
        // is about the query, the schema or the credentials — never the link.
        // `57P01` (admin_shutdown) and `57P02` (crash_shutdown) are the
        // exceptions: the server is going away, and it said so.
        sqlx::Error::Database(d) => {
            matches!(d.code().as_deref(), Some("57P01") | Some("57P02"))
        }
        _ => false,
    }
}

impl DbError {
    /// Did a write collide with a unique constraint?
    ///
    /// Callers branch on this to turn a duplicate into a 409 rather than a 500
    /// — the one query failure that is the caller's business. Exposed here so
    /// they do not have to reach through to `sqlx::Error::Database` and the
    /// driver's SQLSTATE to ask.
    pub fn is_unique_violation(&self) -> bool {
        matches!(self, DbError::Query(sqlx::Error::Database(d)) if d.is_unique_violation())
    }

    /// Which constraint a violation names, when the driver says.
    ///
    /// A caller that inserts against two unique constraints has to know which
    /// one failed to answer "username taken" versus "email taken". Exposing
    /// the name here keeps that decision in the caller while keeping the
    /// driver's error type out of its signature.
    pub fn constraint(&self) -> Option<&str> {
        match self {
            DbError::Query(sqlx::Error::Database(d)) => d.constraint(),
            _ => None,
        }
    }

    /// Is this worth RETRYING, or is it the database telling us we are wrong?
    ///
    /// The distinction a background worker needs (MAIN-409): a connection that
    /// blipped is a reason to back off and try again, while a missing database
    /// or a table that does not exist is a reason to stop and say so — retrying
    /// forever against those is not resilience, it is a silent outage.
    ///
    /// Lives here rather than at the call site because answering it means
    /// reaching into `sqlx::Error` and the driver's SQLSTATE, which is exactly
    /// what this type exists to keep out of callers' signatures.
    ///
    /// Deliberately CONSERVATIVE: only the cases that are unambiguously a
    /// transport or availability blip say `true`. Anything unrecognised is
    /// treated as fatal by the caller, which is no worse than the behaviour
    /// this replaced (the worker exited on every error) and never spins on a
    /// bug.
    pub fn is_transient(&self) -> bool {
        match self {
            // CONNECT and QUERY are classified by the same rule, deliberately.
            // The first cut said `Connect(_) => true` unconditionally, and that
            // is wrong for the case AC-3 names by name: a server that answers
            // the startup packet with `3D000 database does not exist` — or
            // `28P01 invalid password` — has ANSWERED, and no amount of
            // retrying changes what it said. Only the failures that never got
            // an answer are worth trying again.
            DbError::Connect(e) | DbError::Query(e) => driver_error_is_transient(e),
            // A scheme we cannot run is a configuration error and will be one
            // forever.
            DbError::UnsupportedScheme(_) | DbError::NotYetSupported(_) => false,
        }
    }

    /// Did a `query_one` find nothing?
    ///
    /// The other branch worth a caller's attention: it is a 404, not a 500.
    pub fn is_row_not_found(&self) -> bool {
        matches!(self, DbError::Query(sqlx::Error::RowNotFound))
    }
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DbError::UnsupportedScheme(s) => write!(
                f,
                "unsupported DATABASE_URL scheme `{s}` — use postgres:// or sqlite://"
            ),
            DbError::NotYetSupported(Engine::Sqlite) => write!(
                f,
                "sqlite:// is recognized but not supported yet — the SQLite engine \
                 lands in MAIN-196; use postgres:// for now"
            ),
            DbError::NotYetSupported(e) => write!(f, "{e} is not supported yet"),
            DbError::Connect(e) => write!(f, "could not connect to the database: {e}"),
            DbError::Query(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for DbError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DbError::Connect(e) | DbError::Query(e) => Some(e),
            _ => None,
        }
    }
}

/// Detect the engine from a `DATABASE_URL` by its scheme, without opening a
/// connection. `postgres://` and `postgresql://` → Postgres; `sqlite://` (and
/// `sqlite::memory:`) → Sqlite; anything else is [`DbError::UnsupportedScheme`].
/// The scheme match is case-insensitive.
pub fn engine_from_url(url: &str) -> Result<Engine, DbError> {
    let scheme = url
        .split_once(':')
        .map(|(s, _)| s)
        .unwrap_or("")
        .to_ascii_lowercase();
    match scheme.as_str() {
        "postgres" | "postgresql" => Ok(Engine::Postgres),
        "sqlite" => Ok(Engine::Sqlite),
        _ => Err(DbError::UnsupportedScheme(scheme)),
    }
}

/// Open the database named by `url`, selecting the engine from its scheme
/// (MAIN-195). This is the single boot entry point — every binary's startup
/// routes pool construction through here, so scheme validation and the
/// unknown-scheme refusal happen in exactly one place.
///
/// Postgres connects exactly as before (the pool type is still `PgPool`, so
/// behavior is bit-identical). An unknown scheme is refused with a pointed
/// message; `sqlite://` is *recognized* but refused until MAIN-196 supplies the
/// SQLite driver, migration track, and the query adaptation the engine wrapper
/// needs (that work is inseparable from the query surface, so it lives with the
/// engine that requires it, not here).
pub async fn connect(url: &str, max_connections: u32) -> Result<DbPool, DbError> {
    use std::str::FromStr;

    match engine_from_url(url)? {
        Engine::Postgres => {
            let pg = sqlx::postgres::PgPoolOptions::new()
                .max_connections(max_connections)
                .connect(url)
                .await
                .map_err(DbError::Connect)?;
            Ok(EnginePool::from_pg(pg))
        }
        Engine::Sqlite => {
            // `create_if_missing` is what makes "point it at an empty file and
            // boot" true (MAIN-196): without it sqlx refuses a path that does
            // not exist yet, which is exactly the zero-infrastructure case.
            //
            // One connection, deliberately. SQLite serialises writers anyway,
            // and a pool of them turns a serialised write into a `database is
            // locked` error instead of a wait. The single-instance file lock is
            // its own card (MAIN-195 AC-5); this is just not pretending.
            let opts = sqlx::sqlite::SqliteConnectOptions::from_str(url)
                .map_err(DbError::Connect)?
                .create_if_missing(true)
                .foreign_keys(true)
                .busy_timeout(std::time::Duration::from_secs(10));
            let sqlite = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(opts)
                .await
                .map_err(DbError::Connect)?;
            Ok(EnginePool::from_sqlite(sqlite))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// MAIN-409 AC-3: what a background worker may retry, and what it must not.
    ///
    /// The case that matters is the one the first cut got wrong — `Connect`
    /// was transient unconditionally, so a URL naming a database that does not
    /// exist retried at the ceiling forever, which is the outage AC-3 names
    /// with the alarm switched off.
    mod transience {
        use super::*;

        /// A `Database` error carrying one SQLSTATE.
        ///
        /// `PgDatabaseError` has no public constructor, so this implements the
        /// public `DatabaseError` trait instead. That is not a weaker test: the
        /// predicate reads `code()` and nothing else, so this exercises the
        /// exact path the driver's own error takes through it.
        #[derive(Debug)]
        struct Coded(String);

        impl std::fmt::Display for Coded {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "SQLSTATE {}", self.0)
            }
        }
        impl std::error::Error for Coded {}
        impl sqlx::error::DatabaseError for Coded {
            fn message(&self) -> &str {
                "test"
            }
            fn code(&self) -> Option<std::borrow::Cow<'_, str>> {
                Some(std::borrow::Cow::Borrowed(&self.0))
            }
            fn kind(&self) -> sqlx::error::ErrorKind {
                sqlx::error::ErrorKind::Other
            }
            fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
                self
            }
            fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
                self
            }
            fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
                self
            }
        }

        fn db_err(code: &str) -> sqlx::Error {
            sqlx::Error::Database(Box::new(Coded(code.to_string())))
        }

        #[test]
        fn a_server_that_answered_is_never_retried_however_it_failed() {
            // 3D000 invalid_catalog_name — "database does not exist", AC-3's
            // own example. 28P01 invalid_password — the same shape: answered,
            // and the answer will not change.
            for code in ["3D000", "28P01", "42P01"] {
                assert!(
                    !DbError::Connect(db_err(code)).is_transient(),
                    "{code} on connect must be fatal"
                );
                assert!(
                    !DbError::Query(db_err(code)).is_transient(),
                    "{code} on query must be fatal"
                );
            }
        }

        #[test]
        fn a_server_going_away_is_retried_from_either_arm() {
            // The carve-out: the server SAID it is shutting down, so the next
            // attempt reaches a different process rather than the same refusal.
            for code in ["57P01", "57P02"] {
                assert!(DbError::Connect(db_err(code)).is_transient(), "{code}");
                assert!(DbError::Query(db_err(code)).is_transient(), "{code}");
            }
        }

        #[test]
        fn nobody_answering_is_retried() {
            let io = || sqlx::Error::Io(std::io::Error::other("connection refused"));
            assert!(DbError::Connect(io()).is_transient());
            assert!(DbError::Query(io()).is_transient());
            assert!(DbError::Connect(sqlx::Error::PoolTimedOut).is_transient());
        }

        #[test]
        fn the_two_arms_answer_identically() {
            // The property the shared classifier exists to hold. Two rules is
            // how the first cut ended up retrying a missing database forever.
            for e in [
                sqlx::Error::PoolTimedOut,
                sqlx::Error::Io(std::io::Error::other("x")),
                db_err("3D000"),
                db_err("57P01"),
            ] {
                assert_eq!(
                    DbError::Connect(clone_err(&e)).is_transient(),
                    DbError::Query(clone_err(&e)).is_transient(),
                    "connect and query disagree about {e:?}"
                );
            }
        }

        /// `sqlx::Error` is not `Clone`; rebuild the same shape for the second
        /// arm rather than asserting on two different errors.
        fn clone_err(e: &sqlx::Error) -> sqlx::Error {
            match e {
                sqlx::Error::PoolTimedOut => sqlx::Error::PoolTimedOut,
                sqlx::Error::Io(_) => sqlx::Error::Io(std::io::Error::other("x")),
                sqlx::Error::Database(d) => db_err(d.code().as_deref().unwrap_or("")),
                other => panic!("unhandled in this test helper: {other:?}"),
            }
        }

        #[test]
        fn a_scheme_we_cannot_run_is_forever() {
            assert!(!DbError::UnsupportedScheme("mysql".into()).is_transient());
            assert!(!DbError::NotYetSupported(Engine::Sqlite).is_transient());
        }
    }

    #[test]
    fn detects_postgres_variants() {
        assert_eq!(
            engine_from_url("postgres://u@h/db").unwrap(),
            Engine::Postgres
        );
        assert_eq!(
            engine_from_url("postgresql://u@h/db").unwrap(),
            Engine::Postgres
        );
        // Scheme match is case-insensitive.
        assert_eq!(
            engine_from_url("POSTGRES://u@h/db").unwrap(),
            Engine::Postgres
        );
    }

    #[test]
    fn detects_sqlite_forms() {
        assert_eq!(
            engine_from_url("sqlite:///tmp/nook.db").unwrap(),
            Engine::Sqlite
        );
        assert_eq!(engine_from_url("sqlite::memory:").unwrap(), Engine::Sqlite);
    }

    #[test]
    fn refuses_unknown_schemes_with_a_pointed_message() {
        let err = engine_from_url("mysql://u@h/db").unwrap_err();
        assert!(matches!(&err, DbError::UnsupportedScheme(s) if s == "mysql"));
        let msg = err.to_string();
        assert!(msg.contains("mysql"), "names the bad scheme: {msg}");
        assert!(
            msg.contains("postgres://") && msg.contains("sqlite://"),
            "names the supported ones: {msg}"
        );
        // A URL with no scheme is unsupported, not a panic.
        assert!(matches!(
            engine_from_url("not-a-url"),
            Err(DbError::UnsupportedScheme(_))
        ));
    }

    /// MAIN-196 is what this test used to wait for: `connect` now OPENS a
    /// SQLite pool instead of refusing one. Creating the file when it is
    /// missing is the point — "point it at a path and boot" is the whole
    /// zero-infrastructure promise.
    #[tokio::test]
    async fn connect_opens_sqlite_and_creates_the_file() {
        let path =
            std::env::temp_dir().join(format!("nook-connect-{}.db", uuid::Uuid::now_v7().simple()));
        let _ = std::fs::remove_file(&path);

        let pool = connect(&format!("sqlite://{}", path.display()), 1)
            .await
            .expect("an absent sqlite path is created, not refused");
        assert_eq!(pool.engine(), Engine::Sqlite);
        assert!(path.exists(), "the file was created");

        drop(pool);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn connect_refuses_unknown_scheme_before_opening() {
        // mysql:// never touches the network — it fails at scheme detection.
        let err = connect("mysql://u@h/db", 1).await.unwrap_err();
        assert!(matches!(err, DbError::UnsupportedScheme(_)));
    }
}
