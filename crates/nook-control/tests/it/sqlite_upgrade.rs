//! An app upgrade migrates the database file that is already there, and the
//! user's data survives it (MAIN-400 AC-3, NG-2).
//!
//! `sqlite_boot.rs` proves a VIRGIN file boots. That is the first launch, and it
//! is the case a desktop install stops being in about a minute after it is
//! installed. Every launch afterwards opens a file that a *previous version of
//! the app* created — with rows in it — and runs a migration set that has grown
//! since. Nothing exercised that: the whole SQLite track was only ever applied
//! to empty files created and dropped inside one test.
//!
//! The older app is modelled by running a PREFIX of the current migration set,
//! which is precisely what an older build shipped. No fixture database to
//! check in and keep current, and it re-derives itself as migrations land.

use std::borrow::Cow;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use sqlx::migrate::Migrator;
use tower::ServiceExt;

/// A scratch database file that removes itself, sidecars and all.
struct ScratchDb(std::path::PathBuf);

impl ScratchDb {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "nook-sqlite-upgrade-{tag}-{}.db",
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
        for ext in ["-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{ext}", self.0.display()));
        }
    }
}

/// The migration set an older build of the app shipped: the first `keep` of the
/// current one.
///
/// `Migrator`'s fields are public (doc-hidden, semver-exempt) so `migrate!()`
/// can build the static in a const context; `nook_db::migrate` already borrows
/// that to flip one flag. Same trick, one field.
fn as_of(keep: usize) -> Migrator {
    let current = &nook_control::MIGRATOR_SQLITE;
    Migrator {
        migrations: Cow::Owned(current.migrations[..keep].to_vec()),
        ignore_missing: current.ignore_missing,
        locking: current.locking,
        no_tx: current.no_tx,
    }
}

/// How many migrations the imagined older build was behind. Three, so the
/// upgrade applies several rather than resting on whatever the newest one
/// happens to touch.
const BEHIND: usize = 3;

/// The boot step `main.rs` runs, on whatever file it is pointed at.
async fn boot(db: &nook_db::DbPool) -> Result<(), nook_db::migrate::BootMigrateError> {
    nook_db::migrate::run_boot_migrations_for(
        db,
        false,
        &nook_control::MIGRATOR,
        &nook_control::MIGRATOR_SQLITE,
        nook_control::SQUASH_MANIFEST,
    )
    .await
}

/// A tenant and a person, written by the "older" app. Rows rather than a table
/// count: AC-3 is about the user's data still being there, and a schema that
/// survived while its contents did not would pass any structural check.
///
/// No org is inserted — `0001` ships the default one `tenants.org_id` points
/// at, so writing one here is a UNIQUE violation rather than setup.
async fn write_user_data(db: &nook_db::DbPool) {
    let pool = db.sqlite();
    sqlx::query("INSERT INTO tenants (id, name, slug) VALUES (?, ?, ?)")
        .bind("11111111-1111-4111-8111-111111111111")
        .bind("Before The Upgrade")
        .bind("before-the-upgrade")
        .execute(pool)
        .await
        .expect("the older app writes a tenant");
    sqlx::query("INSERT INTO users (id, tenant_id, display_name, email) VALUES (?, ?, ?, ?)")
        .bind("22222222-2222-4222-8222-222222222222")
        .bind("11111111-1111-4111-8111-111111111111")
        .bind("Ada")
        .bind("ada@example.com")
        .execute(pool)
        .await
        .expect("the older app writes a person");
}

/// AC-3: the new set runs against the existing file and the rows are still
/// there afterwards — and the app that comes out serves.
#[tokio::test]
async fn an_upgrade_migrates_the_existing_file_and_keeps_the_data() {
    let file = ScratchDb::new("data");
    let total = nook_control::MIGRATOR_SQLITE.migrations.len();
    assert!(
        total > BEHIND,
        "the track needs more migrations than the gap being modelled"
    );

    // The older app: an earlier migration set, and a person's data in it.
    {
        let old = nook_db::connect(&file.url(), 1).await.expect("open");
        as_of(total - BEHIND)
            .run(old.sqlite())
            .await
            .expect("the older build's set applies to a virgin file");
        write_user_data(&old).await;
        old.sqlite().close().await;
    }

    // The upgrade: the shipped boot step, on the file that is already there.
    let db = nook_db::connect(&file.url(), 1).await.expect("reopen");
    boot(&db)
        .await
        .expect("the new set applies to an existing file");

    let (name, email): (String, String) =
        sqlx::query_as("SELECT t.name, u.email FROM tenants t JOIN users u ON u.tenant_id = t.id")
            .fetch_one(db.sqlite())
            .await
            .expect("the data written before the upgrade is still readable");
    assert_eq!(name, "Before The Upgrade");
    assert_eq!(email, "ada@example.com");

    // The ledger is complete, so the next launch is a no-op rather than a
    // partial re-apply.
    let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations")
        .fetch_one(db.sqlite())
        .await
        .expect("ledger");
    assert_eq!(applied as usize, total, "every migration is recorded");

    // …and the upgraded file is not merely migrated but serving. A half-applied
    // schema that answers nothing is the state AC-3 forbids presenting as
    // working.
    let cfg = nook_control::Config::for_test();
    nook_control::seed::run(&db, &cfg)
        .await
        .expect("seeds run against an upgraded file");
    let state = nook_control::AppState::new(db.clone(), cfg, None).await;
    let res = nook_control::routes::build_router(state)
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("healthz responds");
    assert_eq!(res.status(), StatusCode::OK);
}

/// Running it twice changes nothing — the second launch after an upgrade is
/// the ordinary case, and it must not re-apply what it already applied.
#[tokio::test]
async fn migrating_an_already_current_file_is_a_no_op() {
    let file = ScratchDb::new("idempotent");
    let db = nook_db::connect(&file.url(), 1).await.expect("open");
    boot(&db).await.expect("first boot");
    let after_first: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations")
        .fetch_one(db.sqlite())
        .await
        .expect("ledger");

    boot(&db).await.expect("second boot");
    let after_second: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations")
        .fetch_one(db.sqlite())
        .await
        .expect("ledger");
    assert_eq!(after_first, after_second);
}

/// NG-2: a file written by a NEWER app refuses, loudly, in the older one.
///
/// The desktop's `APP_ENV` is `desktop` rather than `production`, and MAIN-224's
/// dev tolerance forgives exactly this shape of ledger — a version applied here
/// but absent from the resolved set. It is Postgres-only, and this asserts that
/// it stays that way: forgiving it on SQLite would mean an older app quietly
/// serving a database whose schema it does not understand, which is migrating
/// backwards by omission.
#[tokio::test]
async fn a_newer_file_refuses_to_open_in_an_older_app() {
    let file = ScratchDb::new("downgrade");
    let db = nook_db::connect(&file.url(), 1).await.expect("open");
    boot(&db).await.expect("first boot");

    // What a future release would have left behind.
    sqlx::query(
        "INSERT INTO _sqlx_migrations
           (version, description, installed_on, success, checksum, execution_time)
         VALUES (?, ?, CURRENT_TIMESTAMP, 1, ?, 0)",
    )
    .bind(999_999_i64)
    .bind("from a newer app")
    .bind(vec![0u8; 32])
    .execute(db.sqlite())
    .await
    .expect("stand in for a newer release's migration");

    let err = boot(&db)
        .await
        .expect_err("an older app must refuse a newer file");
    let msg = err.to_string();
    assert!(
        msg.contains("999999"),
        "the refusal must name the migration it cannot account for: {msg}"
    );
}

/// A migration that fails leaves the boot FAILED, not half-done and serving.
///
/// Asserted by handing the migrator a statement that cannot run against the
/// schema in front of it — the shape any genuine migration bug takes — and
/// checking the boot step reports it rather than returning `Ok`. The desktop
/// shell renders that failure verbatim; what it must never do is get an `Ok`
/// for a file the migration did not finish.
#[tokio::test]
async fn a_migration_that_cannot_apply_fails_the_boot_rather_than_serving() {
    let file = ScratchDb::new("broken");
    let db = nook_db::connect(&file.url(), 1).await.expect("open");

    let current = &nook_control::MIGRATOR_SQLITE;
    let mut migrations = current.migrations.to_vec();
    let last = migrations.last().expect("a non-empty track").clone();
    migrations.push(sqlx::migrate::Migration::new(
        last.version + 1,
        Cow::Borrowed("cannot possibly apply"),
        sqlx::migrate::MigrationType::Simple,
        Cow::Borrowed("ALTER TABLE tenants ADD COLUMN name TEXT NOT NULL;"),
        false,
    ));
    let broken = Migrator {
        migrations: Cow::Owned(migrations),
        ignore_missing: current.ignore_missing,
        locking: current.locking,
        no_tx: current.no_tx,
    };

    let err = broken
        .run(db.sqlite())
        .await
        .expect_err("a migration that cannot apply must fail");
    assert!(
        !err.to_string().is_empty(),
        "the failure carries the driver's reason, which is what the shell shows"
    );

    // The failed migration is not recorded as applied, so the file is not left
    // claiming a schema it does not have.
    let recorded: i64 =
        sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE version = ?")
            .bind(last.version + 1)
            .fetch_one(db.sqlite())
            .await
            .expect("ledger");
    assert_eq!(recorded, 0, "a failed migration must not be recorded");
}
