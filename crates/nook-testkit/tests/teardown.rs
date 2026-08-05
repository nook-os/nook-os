//! MAIN-156: `TestBed` gives each test a private database and drops the whole
//! thing at teardown; `NOOK_KEEP_TEST_DATA` keeps it for debugging. Proven here
//! against `pg_database` — the database itself vanishes, not just its rows.
//! Needs a **Postgres** `DATABASE_URL`; skips cleanly without one, and skips on
//! a SQLite bed because `pg_database` and a raw `PgPool` are both Postgres-only
//! (MAIN-242 AC-4). The SQLite half of the same contract — teardown removes the
//! file, `keep` preserves it — lives in `tests/dual_engine.rs`.

use nook_testkit::TestBed;
use sqlx::PgPool;

/// Does a database of this name exist on the server?
async fn db_exists(name: &str) -> bool {
    let base = std::env::var("DATABASE_URL").expect("DATABASE_URL (TestBed::new gave a bed)");
    let pool = PgPool::connect(&base).await.expect("connect to base");
    let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM pg_database WHERE datname = $1")
        .bind(name)
        .fetch_one(&pool)
        .await
        .expect("query pg_database");
    pool.close().await;
    n > 0
}

#[tokio::test]
async fn teardown_drops_the_private_database() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping teardown test — no DATABASE_URL");
        return;
    };
    if !bed.is_postgres() {
        bed.teardown().await;
        return; // dual_engine.rs owns the SQLite twin of this test
    }
    let name = bed.db_name().to_string();
    // Put a row in it, so "gone" means the whole database, not an empty one.
    let _tenant = bed.tenant("teardown").await;
    assert!(
        db_exists(&name).await,
        "the private database exists while the bed is live"
    );

    bed.teardown().await;
    assert!(
        !db_exists(&name).await,
        "teardown drops the whole private database — created data vanishes with it"
    );
}

#[tokio::test]
async fn keep_preserves_the_database() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    if !bed.is_postgres() {
        bed.teardown().await;
        return; // dual_engine.rs owns the SQLite twin of this test
    }
    // The flag normally comes from NOOK_KEEP_TEST_DATA; set it directly here so
    // this test doesn't race the process-global env var with its sibling.
    bed.set_keep(true);
    let name = bed.db_name().to_string();
    let _tenant = bed.tenant("keep").await;

    bed.teardown().await; // a no-op under keep
    assert!(
        db_exists(&name).await,
        "keep leaves the database in place for debugging"
    );

    // Don't leak the kept database: the Drop guard also honours keep and won't
    // remove it, so drop it by hand. The bed's own pool is no longer reachable
    // from here (MAIN-268 removed `bed.pool`), which costs nothing: `WITH
    // (FORCE)` below terminates its connections server-side, which is the same
    // mechanism the Drop guard has always relied on.
    let base = std::env::var("DATABASE_URL").unwrap();
    let admin = PgPool::connect(&base).await.unwrap();
    sqlx::query(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
        .execute(&admin)
        .await
        .expect("hand-drop the kept database");
    admin.close().await;
}
