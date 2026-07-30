//! An empty SQLite file boots (MAIN-196 AC-3/AC-4).
//!
//! The load-bearing claim of this ticket is not "the DDL parses" — MAIN-236's
//! scaffold test already proves that — but that the **real** boot path works on
//! SQLite: the engine is selected from the URL, the SQLite migration track runs
//! (not the Postgres one), the real `seed::run` executes against it, and the
//! app that comes out answers `/healthz` with 200.
//!
//! So this uses the production pieces end to end: `nook_db::connect`,
//! `migrate::run_boot_migrations_for`, `seed::run`, and the actual router.
//! Nothing is mocked; a mock schema would prove the translation is *plausible*
//! rather than *real*, which is the failure this test exists to prevent.
//!
//! Unswept request paths may still fail with dialect errors — that is NG-1 and
//! the sweep's job. Boot, health and seed may not.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use nook_db::Engine;
use tower::ServiceExt;

/// A scratch database file that removes itself. SQLite's whole appeal here is
/// "no infrastructure", so the test uses a real file rather than `:memory:` —
/// `create_if_missing` on a path that does not exist is precisely the
/// zero-setup case an operator hits.
struct ScratchDb(std::path::PathBuf);

impl ScratchDb {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "nook-sqlite-boot-{tag}-{}.db",
            uuid::Uuid::now_v7().simple()
        ));
        let _ = std::fs::remove_file(&p);
        ScratchDb(p)
    }
    fn url(&self) -> String {
        format!("sqlite://{}", self.0.display())
    }
}

impl Drop for ScratchDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        // sqlx may leave the WAL sidecars behind.
        for ext in ["-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{ext}", self.0.display()));
        }
    }
}

/// Connect + migrate + seed, exactly as `main.rs` does.
async fn boot(db_file: &ScratchDb) -> nook_db::DbPool {
    let db = nook_db::connect(&db_file.url(), 1)
        .await
        .expect("an empty sqlite path opens (create_if_missing)");
    assert_eq!(db.engine(), Engine::Sqlite, "the URL selected the engine");

    nook_db::migrate::run_boot_migrations_for(
        &db,
        false,
        &nook_control::MIGRATOR,
        &nook_control::MIGRATOR_SQLITE,
        nook_control::SQUASH_MANIFEST,
    )
    .await
    .expect("the SQLite track applies to an empty file");

    db
}

#[tokio::test]
async fn an_empty_sqlite_file_migrates_seeds_and_serves_healthz() {
    let file = ScratchDb::new("full");
    let db = boot(&file).await;

    // The REAL seed, not a fixture. This is the step that failed before the
    // dialect seams existed: it emitted `now()`, which SQLite has no such
    // function for.
    let cfg = nook_control::Config::for_test();
    nook_control::seed::run(&db, &cfg)
        .await
        .expect("seeds run on SQLite");

    // …and the app answers.
    let state = nook_control::AppState::new(db.clone(), cfg, None).await;
    let app = nook_control::routes::build_router(state);
    let res = app
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("healthz responds");
    assert_eq!(res.status(), StatusCode::OK, "/healthz on sqlite:// is 200");
}

/// AC-2: the engine picks the track. Running the Postgres DDL against SQLite
/// would fail loudly, so the fact that boot succeeds is itself evidence — but
/// assert the selection directly rather than inferring it.
#[tokio::test]
async fn the_engine_selects_the_migration_track() {
    assert!(std::ptr::eq(
        nook_control::migrator_for(Engine::Postgres),
        &nook_control::MIGRATOR
    ));
    assert!(std::ptr::eq(
        nook_control::migrator_for(Engine::Sqlite),
        &nook_control::MIGRATOR_SQLITE
    ));
}

/// AC-4's spot-check: representative tables exist, with the audit's mapped
/// column types rather than Postgres ones.
#[tokio::test]
async fn the_booted_schema_has_the_mapped_types() {
    let file = ScratchDb::new("types");
    let db = boot(&file).await;
    let pool = db.sqlite();

    let tables: Vec<String> =
        sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .fetch_all(pool)
            .await
            .expect("list tables");
    for t in ["tenants", "users", "tasks", "nodes", "sessions"] {
        assert!(
            tables.contains(&t.to_string()),
            "missing {t}; got {tables:?}"
        );
    }

    // uuid → TEXT, timestamptz → TEXT, bigint → INTEGER.
    let cols: Vec<(String, String)> =
        sqlx::query_as("SELECT name, type FROM pragma_table_info('tasks')")
            .fetch_all(pool)
            .await
            .expect("tasks columns");
    let ty = |n: &str| {
        cols.iter()
            .find(|(c, _)| c == n)
            .unwrap_or_else(|| panic!("tasks.{n} missing; got {cols:?}"))
            .1
            .clone()
    };
    assert_eq!(ty("id"), "TEXT", "uuid maps to TEXT");
    assert_eq!(ty("created_at"), "TEXT", "timestamptz maps to TEXT");
    assert_eq!(ty("number"), "INTEGER");

    // The ledger records the SQLite track, so a second boot is a no-op rather
    // than a re-apply.
    let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations")
        .fetch_one(pool)
        .await
        .expect("ledger");
    assert!(applied >= 1, "the SQLite track recorded itself");
}

/// Booting twice over the same file must be a no-op, not a re-apply — the
/// ordinary case of restarting a single-machine deployment.
#[tokio::test]
async fn a_second_boot_over_the_same_file_is_clean() {
    let file = ScratchDb::new("twice");
    {
        let db = boot(&file).await;
        let cfg = nook_control::Config::for_test();
        nook_control::seed::run(&db, &cfg)
            .await
            .expect("first seed");
    }
    let db = boot(&file).await;
    let cfg = nook_control::Config::for_test();
    nook_control::seed::run(&db, &cfg)
        .await
        .expect("seeding is idempotent across a restart");

    let roles: i64 = sqlx::query_scalar("SELECT count(*) FROM roles")
        .fetch_one(db.sqlite())
        .await
        .expect("roles");
    assert!(roles >= 1, "the seeded role model survived the restart");
}

/// The branch a fresh boot never takes (review of 8a34a63).
///
/// `managed::upsert_default` has three paths: insert (fresh), update (the
/// shipped default's sha moved — i.e. an UPGRADE), and no-op. Only the first
/// runs on a virgin file, which is exactly why the rest of this suite — and a
/// manual boot — missed a `Postgres.now()` splice living in the second. On
/// SQLite that is a syntax error, so the *second* boot of a single-machine
/// deployment, after any change to a shipped managed skill, would have died in
/// seeding.
///
/// This drives the update branch directly, which is the only way a fresh-boot
/// suite can see this class of conditional-branch Postgres-ism.
#[tokio::test]
async fn the_managed_upgrade_branch_survives_on_sqlite() {
    let file = ScratchDb::new("managed");
    let db = boot(&file).await;
    // The seeder speaks to the repository now (MAIN-258); the engine-selected
    // `now()` this test exists for moved into its impl with it.
    let repo = nook_control::repo::admin::DbManagedContentRepository::new(db.clone());

    // Install at version 1 (the insert branch).
    nook_control::routes::managed::upsert_default(&repo, "skill", "sqlite-probe", "v1 body")
        .await
        .expect("insert branch");

    // Now ship a DIFFERENT default: the sha moves, so the UPDATE branch runs.
    // This is the call that failed before the fix.
    nook_control::routes::managed::upsert_default(&repo, "skill", "sqlite-probe", "v2 body")
        .await
        .expect("the upgrade branch must not be Postgres-only");

    let (version, content, updated): (i64, String, Option<String>) = sqlx::query_as(
        "SELECT version, content, updated_at FROM managed_content
          WHERE kind = 'skill' AND name = 'sqlite-probe'",
    )
    .fetch_one(db.sqlite())
    .await
    .expect("the managed row");

    assert_eq!(version, 2, "the version bumped");
    assert_eq!(content, "v2 body", "the newer default won");
    assert!(
        updated.is_some_and(|u| !u.is_empty()),
        "updated_at was stamped by CURRENT_TIMESTAMP, not left null"
    );

    // Re-running with unchanged content takes the no-op branch and must also
    // survive — this is the ordinary restart.
    nook_control::routes::managed::upsert_default(&repo, "skill", "sqlite-probe", "v2 body")
        .await
        .expect("no-op branch");
    let version: i64 = sqlx::query_scalar(
        "SELECT version FROM managed_content WHERE kind = 'skill' AND name = 'sqlite-probe'",
    )
    .fetch_one(db.sqlite())
    .await
    .expect("version");
    assert_eq!(version, 2, "an unchanged default does not churn the row");
}
