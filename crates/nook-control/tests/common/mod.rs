//! Shared test-database setup.
//!
//! Both integration suites need a Postgres with the schema on it. They used to
//! each connect and hope one was already prepared, which meant they only ever
//! ran on a laptop that happened to have the dev stack up — in CI they skipped
//! themselves and reported success, for every build until the pipeline started
//! providing a database.
//!
//! So: connect, migrate, and refuse to skip anywhere that says it is CI.

use sqlx::PgPool;

/// Connect to `DATABASE_URL` and bring the schema up to date.
///
/// `None` means "no database configured, skip this test" — legitimate on a
/// developer's machine, and a hard failure when `NOOK_REQUIRE_DB` is set,
/// because a silent skip in CI is indistinguishable from a passing test.
pub async fn test_pool() -> Option<PgPool> {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        assert!(
            std::env::var("NOOK_REQUIRE_DB").is_err(),
            "NOOK_REQUIRE_DB is set but DATABASE_URL is not - these tests would \
             have skipped silently and reported success"
        );
        return None;
    };
    let pool = PgPool::connect(&url).await.ok()?;
    // Test binaries are separate processes and cargo may run them at the same
    // time; sqlx takes an advisory lock, so racing migrations are safe and the
    // loser simply finds the work already done.
    nook_control::MIGRATOR
        .run(&pool)
        .await
        .expect("migrations must apply to the test database");
    Some(pool)
}
