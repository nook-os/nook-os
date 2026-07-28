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

/// The workspace-wide database pool type. See the crate docs: this is the single
/// pivot the engine-by-URL abstraction (MAIN-195) will redefine.
pub type DbPool = sqlx::PgPool;

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
        }
    }
}

impl std::error::Error for DbError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DbError::Connect(e) => Some(e),
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
    match engine_from_url(url)? {
        Engine::Postgres => sqlx::postgres::PgPoolOptions::new()
            .max_connections(max_connections)
            .connect(url)
            .await
            .map_err(DbError::Connect),
        Engine::Sqlite => Err(DbError::NotYetSupported(Engine::Sqlite)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[tokio::test]
    async fn connect_refuses_sqlite_until_main_196() {
        let err = connect("sqlite:///tmp/x.db", 1).await.unwrap_err();
        assert!(matches!(err, DbError::NotYetSupported(Engine::Sqlite)));
        assert!(err.to_string().contains("MAIN-196"));
    }

    #[tokio::test]
    async fn connect_refuses_unknown_scheme_before_opening() {
        // mysql:// never touches the network — it fails at scheme detection.
        let err = connect("mysql://u@h/db", 1).await.unwrap_err();
        assert!(matches!(err, DbError::UnsupportedScheme(_)));
    }
}
