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
