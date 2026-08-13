//! One place chat's DB-backed tests get a database, on either engine
//! (MAIN-294), and on **Postgres a private one** (MAIN-165).
//!
//! Every test module used to build its own pool with `PgConnectOptions` +
//! `PgPoolOptions` straight from `DATABASE_URL`. On `sqlite://` that waits for a
//! Postgres server that will never answer and fails as `PoolTimedOut` — which is
//! what 17 tests in `nook_chat:main`, plus the two integration binaries, were
//! actually doing. Four copies of the same construction is also four places for
//! the engines to drift apart, so they now share this.
//!
//! ## Why the Postgres arm stopped using the shared database
//!
//! It ran the control plane's migrator on the shared `public` AND chat's on the
//! shared `chat` schema. So any branch carrying a chat migration stamped the
//! SHARED chat ledger the moment its tests ran, and a sibling chat branch then
//! hit `VersionMismatch` — which serialised the whole chat migration chain to
//! one PR at a time. Isolating by random uuids never addressed that: the rows
//! were separable, the ledger was not.
//!
//! Both arms are now a private [`nook_testkit::TestBed`] database, dropped
//! whole on the way out. What differs is only what chat has to add to it:
//!
//! **Postgres** — the bed arrives with `public` already migrated (the testkit's
//! template runs the control track), so chat creates its schema and runs its own
//! migrator into it, over a pool pinned to `search_path=chat,public`. That pin
//! is the reason chat needs its own pool rather than the bed's: it is what makes
//! chat's tables and the control plane's auth tables both resolve, and testing
//! through anything else would stop testing the thing that has broken before
//! (MAIN-87).
//!
//! **SQLite** — the bed's own pool, unchanged. One file is one namespace and one
//! `_sqlx_migrations`, so chat's `chat_*` tables live in the control track's
//! `0001` and nothing else may write the ledger. There is no second pool to
//! open, which is why [`nook_testkit::TestBed::database_url`] hands back nothing
//! there.

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
    /// The same store `state.content` holds, kept concretely so a test can ask
    /// what a delete had forgotten (MAIN-535 AC-6).
    content: Arc<crate::content::RecordingContent>,
    /// The bed owns the private database and drops it — on Postgres from its own
    /// `Drop` safety net when a test ends or panics without calling
    /// [`ChatTest::teardown`]. It must outlive the test, so holding it here is
    /// the whole reason this type exists rather than returning a bare
    /// `AppState`.
    bed: nook_testkit::TestBed,
}

impl Deref for ChatTest {
    type Target = AppState;
    fn deref(&self) -> &AppState {
        &self.state
    }
}

impl ChatTest {
    /// The private database backing this test — its name on Postgres, its file
    /// on SQLite. For the teardown test, which is the only caller.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn db_name(&self) -> &str {
        self.bed.db_name()
    }

    /// The content ids this test's deletes asked the store to forget, in order.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn forgotten_content(&self) -> Vec<uuid::Uuid> {
        self.content.forgotten()
    }

    /// Drop the whole private database — both schemas with it.
    ///
    /// Every DB-backed chat test ends with this, as nook-control's do, rather
    /// than leaning on the bed's `Drop`. The net is real but it is a net: it
    /// runs while ~30 test futures are finishing at once, each holding chat's
    /// pool *and* the bed's, and its admin connection is the one that then
    /// cannot be opened — a swallowed failure that leaves the database behind.
    /// Measured: four survivors per run before this, none after.
    ///
    /// **Chat's own pool is deliberately not closed here.** `PgPool::close()`
    /// waits for every connection to come back, and `bus.rs` hands one to a
    /// `PgListener` that holds it for the life of the subscription — so closing
    /// it here hangs the test rather than tidying it. `DROP DATABASE … WITH
    /// (FORCE)` severs those connections server-side instead, which is the same
    /// reason `TestBed::Drop` never touches a pool either.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) async fn teardown(mut self) {
        self.bed.teardown().await;
    }
}

/// Wire the repositories and registry over a pool — the same `AppState` the
/// service builds, whichever engine produced the pool.
fn state_over(db: DbPool, content: Arc<crate::content::RecordingContent>) -> AppState {
    AppState {
        channels: Arc::new(crate::repo::channels::DbChannelRepository::new(db.clone())),
        messages: Arc::new(crate::repo::messages::DbMessageRepository::new(db.clone())),
        dms: Arc::new(crate::repo::dms::DbDmRepository::new(db.clone())),
        // A test has no control plane to ask, and asking one would make the
        // suite depend on a second service being up. What matters here is that
        // chat asks at all, and for which ids — so the store records.
        content,
        db,
        registry: Arc::new(crate::registry::Registry::new()),
    }
}

/// A private database and a wired `AppState`, or `None` when the suite runs
/// without one.
///
/// `what` names the test family in the skip line, matching the suite convention
/// of saying which tests were skipped and why.
pub(crate) async fn chat_test(what: &str) -> Option<ChatTest> {
    if std::env::var("NOOK_REQUIRE_DB").ok().as_deref() != Some("1") {
        eprintln!("skipping {what} — no NOOK_REQUIRE_DB");
        return None;
    }
    let bed = nook_testkit::TestBed::new().await?;
    let content = Arc::new(crate::content::RecordingContent::default());
    Some(ChatTest {
        state: state_over(chat_pool(&bed).await?, content.clone()),
        content,
        bed,
    })
}

/// Chat's pool over a bed's private database: the service's own
/// `search_path=chat,public` on Postgres, the bed's single pool on SQLite.
///
/// The two integration binaries do the same three steps against their own bed.
/// They cannot call this — chat is a `[[bin]]` with no library target, so
/// nothing under `tests/` can reach into `crate::` — which is also why each of
/// them already carries its own `ensure_chat_schema`.
async fn chat_pool(bed: &nook_testkit::TestBed) -> Option<DbPool> {
    let Some(url) = bed.database_url() else {
        // SQLite: the bed's pool IS the database, control track and all.
        return Some(bed.db());
    };
    let db = crate::open_pool(&url, 4, crate::CHAT_SEARCH_PATH)
        .await
        .ok()?;
    // The schema must exist before the migrator creates `chat._sqlx_migrations`.
    crate::ensure_chat_schema(&db).await.ok()?;
    // Chat's OWN track only. `public` arrived already migrated with the bed's
    // template, so running the control migrator here would be both redundant and
    // wrong — over a chat-first search_path it lands the control plane's tables
    // and ledger inside `chat`, where they collide with chat's.
    crate::MIGRATOR.run(db.pg()).await.ok()?;
    Some(db)
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

#[cfg(test)]
mod tests {
    use nook_db::Db;

    /// The isolation claim, checked rather than asserted in a doc comment: the
    /// database a chat test ran against is GONE afterwards — chat schema, chat
    /// ledger and all — so nothing a chat migration stamps can outlive its test.
    #[tokio::test]
    async fn teardown_takes_the_whole_private_database_with_it() {
        let Some(st) = super::chat_test("the chat bed teardown test").await else {
            return;
        };
        if super::skip_unless_postgres("the chat bed teardown test") {
            return;
        }
        let name = st.db_name().to_string();

        // Chat's own schema really is in there while the test runs.
        let has_chat: i64 = st
            .db
            .query_scalar(
                "SELECT count(*) FROM information_schema.tables
                  WHERE table_schema = 'chat' AND table_name = '_sqlx_migrations'",
                nook_db::params![],
            )
            .await
            .expect("chat ledger");
        assert_eq!(has_chat, 1, "the bed carries chat's own ledger");

        st.teardown().await;

        // Asked from the base database, the only place that can see the drop.
        let base = std::env::var("DATABASE_URL").expect("checked by the bed");
        let admin = nook_db::connect(&base, 1).await.expect("admin pool");
        let still_there: i64 = admin
            .query_scalar(
                "SELECT count(*) FROM pg_database WHERE datname = $1",
                nook_db::params![name.clone()],
            )
            .await
            .expect("pg_database");
        assert_eq!(still_there, 0, "{name} outlived its test");
    }
}
