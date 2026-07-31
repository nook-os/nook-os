//! One place chat's DB-backed tests get a database, on either engine (MAIN-294).
//!
//! Every test module used to build its own pool with `PgConnectOptions` +
//! `PgPoolOptions` straight from `DATABASE_URL`. On `sqlite://` that waits for a
//! Postgres server that will never answer and fails as `PoolTimedOut` — which is
//! what 17 tests in `nook_chat:main`, plus the two integration binaries, were
//! actually doing. Four copies of the same construction is also four places for
//! the engines to drift apart, so they now share this.
//!
//! ## The two arms are deliberately different shapes
//!
//! **Postgres** keeps exactly what chat had: the *service's own* pool
//! configuration, against the shared `DATABASE_URL` database, isolated by random
//! uuids rather than by a private database. That is not incidental — chat pins
//! `search_path=chat,public` so its own tables and the control plane's auth
//! tables both resolve, and testing through anything else would stop testing the
//! thing that has broken before (MAIN-87).
//!
//! **SQLite** takes a [`nook_testkit::TestBed`]: a private file per test,
//! migrated with the control track and dropped on the way out. It has to be a
//! bed rather than a hand-built pool, because on SQLite the control track IS
//! chat's track — one file is one namespace and one `_sqlx_migrations`, so
//! chat's `chat_*` tables live in that `0001` and nothing else may write the
//! ledger. Reusing the bed also inherits its pool sizing, which is a fix in its
//! own right (MAIN-295) and not one worth re-deriving here.

use std::ops::Deref;
use std::sync::Arc;

use nook_db::DbPool;

use crate::AppState;

/// A chat test's [`AppState`], plus whatever has to stay alive around it.
///
/// Derefs to the state, so a test still reads `st.db` / `st.channels` and
/// `State(st.clone())` still hands a plain `AppState` to a handler — `ChatTest`
/// is deliberately NOT `Clone`, so that `clone()` resolves through the deref to
/// `AppState::clone` and every existing call site keeps working untouched.
pub(crate) struct ChatTest {
    state: AppState,
    /// SQLite only. The bed owns the database file and deletes it when dropped,
    /// so it must outlive the test — holding it here is the whole reason this
    /// type exists rather than returning a bare `AppState`.
    _bed: Option<nook_testkit::TestBed>,
}

impl Deref for ChatTest {
    type Target = AppState;
    fn deref(&self) -> &AppState {
        &self.state
    }
}

/// Wire the repositories and registry over a pool — the same `AppState` the
/// service builds, whichever engine produced the pool.
fn state_over(db: DbPool) -> AppState {
    AppState {
        channels: Arc::new(crate::repo::channels::DbChannelRepository::new(db.clone())),
        messages: Arc::new(crate::repo::messages::DbMessageRepository::new(db.clone())),
        dms: Arc::new(crate::repo::dms::DbDmRepository::new(db.clone())),
        db,
        registry: Arc::new(crate::registry::Registry::new()),
    }
}

/// A database and a wired `AppState`, or `None` when the suite runs without one.
///
/// `what` names the test family in the skip line, matching the suite convention
/// of saying which tests were skipped and why.
pub(crate) async fn chat_test(what: &str) -> Option<ChatTest> {
    if std::env::var("NOOK_REQUIRE_DB").ok().as_deref() != Some("1") {
        eprintln!("skipping {what} — no NOOK_REQUIRE_DB");
        return None;
    }
    let url = std::env::var("DATABASE_URL").ok()?;

    match nook_db::engine_from_url(&url).ok()? {
        nook_db::Engine::Postgres => {
            // Two pools, as before: the bootstrap one runs in `public` so the
            // control plane's migrator lands its tables there, then the service's
            // own `chat,public` pool runs chat's migrator into the `chat` schema.
            let bootstrap = crate::open_pool(&url, 2).await.ok()?;
            crate::ensure_chat_schema(&bootstrap).await.ok()?;
            nook_control::MIGRATOR.run(bootstrap.pg()).await.ok()?;
            let db = crate::open_pool(&url, 4).await.ok()?;
            crate::MIGRATOR.run(db.pg()).await.ok()?;
            Some(ChatTest {
                state: state_over(db),
                _bed: None,
            })
        }
        nook_db::Engine::Sqlite => {
            let bed = nook_testkit::TestBed::new().await?;
            Some(ChatTest {
                state: state_over(bed.db()),
                _bed: Some(bed),
            })
        }
    }
}

/// Skip a test that can only mean anything on Postgres.
///
/// Not everything here is engine-portable in principle: `bus.rs` drives
/// LISTEN/NOTIFY, and the two integration binaries assert `search_path`
/// resolution and `information_schema` isolation. Those are Postgres semantics,
/// not chat behaviour that happens to be written in Postgres — running them on
/// SQLite would prove nothing. Skipping says so out loud, in the same shape as
/// the suite's `NOOK_REQUIRE_DB` gate.
pub(crate) fn skip_unless_postgres(what: &str) -> bool {
    let pg = std::env::var("DATABASE_URL")
        .ok()
        .and_then(|u| nook_db::engine_from_url(&u).ok())
        .is_some_and(|e| e == nook_db::Engine::Postgres);
    if !pg {
        eprintln!("skipping {what} — Postgres-only behaviour");
    }
    !pg
}
