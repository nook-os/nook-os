//! Dev tolerance for a migration ledger ahead of the checked-out set (MAIN-224):
//! a dev boot warns past an applied-but-missing version and proceeds, production
//! stays strictly fatal, a modified migration stays fatal everywhere, the orphan
//! query honours the pool's `search_path` (chat's ledger vs the control plane's),
//! and `scripts/dev-db-heal.sh` lists/deletes exactly the orphan rows while
//! refusing production and non-local URLs.
//!
//! Everything runs against a private `nook_testkit::TestBed` database; only the
//! synthetic ledger rows this test inserts are ever touched. Set `DATABASE_URL`.

use nook_control::MIGRATOR;
use nook_db::migrate::{orphan_versions, run_with_dev_tolerance};
use nook_testkit::TestBed;
use sqlx::migrate::MigrateError;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::PgPool;
use std::path::PathBuf;
use std::process::Command;
use std::str::FromStr;

/// Path to the repo root's copy of the heal script, absolute so the test's CWD
/// does not matter.
fn heal_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/dev-db-heal.sh")
}

/// Swap the database name in a `postgres://…/<db>` URL, mirroring the testkit
/// helper — used to point psql/a fresh pool at this bed's private database.
fn swap_db(url: &str, db: &str) -> String {
    let (prefix, _) = url.rsplit_once('/').expect("a database url with a path");
    format!("{prefix}/{db}")
}

/// Record a synthetic applied migration that has no matching resolved migration —
/// exactly the row an unmerged branch's migration leaves in a shared dev ledger.
async fn insert_orphan(pool: &PgPool, table: &str, version: i64) {
    sqlx::query(&format!(
        "INSERT INTO {table} (version, description, success, checksum, execution_time)
         VALUES ($1, 'synthetic orphan (MAIN-224 test)', true, '\\x00'::bytea, 0)"
    ))
    .bind(version)
    .execute(pool)
    .await
    .expect("insert synthetic ledger row");
}

#[tokio::test]
async fn dev_boot_proceeds_past_an_orphan_row() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    insert_orphan(&bed.pool, "_sqlx_migrations", 9999).await;

    // The orphan is detected…
    let orphans = orphan_versions(&MIGRATOR, &bed.pool)
        .await
        .expect("list orphans");
    assert_eq!(orphans, vec![9999], "the synthetic row is the only orphan");

    // …and a dev boot tolerates it (this is what unbricks branch switching).
    run_with_dev_tolerance(&MIGRATOR, &bed.pool, false)
        .await
        .expect("dev boot proceeds past the orphan row");

    bed.teardown().await;
}

#[tokio::test]
async fn production_stays_fatal_on_an_orphan_row() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    insert_orphan(&bed.pool, "_sqlx_migrations", 9999).await;

    // Production keeps today's strict behaviour, verbatim: the same VersionMissing
    // that would abort the boot — a tolerance leaking here would mask schema drift.
    let err = run_with_dev_tolerance(&MIGRATOR, &bed.pool, true)
        .await
        .expect_err("production must refuse an applied-but-missing version");
    assert!(
        matches!(err, MigrateError::VersionMissing(9999)),
        "expected VersionMissing(9999), got {err:?}"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn dev_tolerance_does_not_mask_a_modified_migration() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    // Corrupt an *applied* migration's checksum — a modified migration, not a
    // missing one. Dev tolerates missing versions; it must never tolerate this
    // (NG-1: this survives a stray row, it never rewrites the ledger).
    sqlx::query("UPDATE _sqlx_migrations SET checksum = '\\xdeadbeef'::bytea WHERE version = 1")
        .execute(&bed.pool)
        .await
        .expect("corrupt an applied checksum");

    let err = run_with_dev_tolerance(&MIGRATOR, &bed.pool, false)
        .await
        .expect_err("a modified migration stays fatal even in dev");
    assert!(
        matches!(err, MigrateError::VersionMismatch(1)),
        "expected VersionMismatch(1), got {err:?}"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn orphan_query_reads_the_ledger_on_the_pools_search_path() {
    // The chat migrator runs on a pool pinned to `search_path=chat,public`, so its
    // ledger is `chat._sqlx_migrations`. Prove the orphan query follows that path —
    // it must read the chat ledger, not the control plane's `public` one.
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let bed_url = swap_db(&base_url, bed.db_name());

    // A chat ledger with one orphan, plus a DIFFERENT orphan in public: if the
    // query ignored search_path it would surface 7777 instead of 8888.
    sqlx::query("CREATE SCHEMA IF NOT EXISTS chat")
        .execute(&bed.pool)
        .await
        .expect("create chat schema");
    sqlx::query(
        "CREATE TABLE chat._sqlx_migrations (
             version bigint primary key, description text not null,
             installed_on timestamptz not null default now(), success boolean not null,
             checksum bytea not null, execution_time bigint not null)",
    )
    .execute(&bed.pool)
    .await
    .expect("create chat ledger");
    insert_orphan(&bed.pool, "chat._sqlx_migrations", 8888).await;
    insert_orphan(&bed.pool, "public._sqlx_migrations", 7777).await;

    let opts = PgConnectOptions::from_str(&bed_url)
        .expect("parse bed url")
        .options([("search_path", "chat,public")]);
    let chat_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .expect("chat-search_path pool");

    let orphans = orphan_versions(&MIGRATOR, &chat_pool)
        .await
        .expect("list chat orphans");
    assert_eq!(
        orphans,
        vec![8888],
        "the chat ledger's orphan, not public's — search_path was honoured"
    );

    chat_pool.close().await;
    bed.teardown().await;
}

#[test]
fn heal_script_refuses_production_and_remote_urls() {
    // These guards fire before any DB access, so they run without psql or a DB —
    // the load-bearing safety property (AC-3): never touch a production ledger.
    let prod = Command::new("bash")
        .arg(heal_script())
        .arg("--fix")
        .env("APP_ENV", "production")
        .env("DATABASE_URL", "postgres://nook:nook@localhost:5432/nook")
        .output()
        .expect("run heal script");
    assert!(!prod.status.success(), "APP_ENV=production must be refused");

    let remote = Command::new("bash")
        .arg(heal_script())
        .env_remove("APP_ENV")
        .env(
            "DATABASE_URL",
            "postgres://u:p@prod.db.example.com:5432/nook",
        )
        .output()
        .expect("run heal script");
    assert!(
        !remote.status.success(),
        "a non-local DATABASE_URL host must be refused"
    );
}

#[tokio::test]
async fn heal_script_lists_and_deletes_only_the_orphan() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    // The script drives psql; where it is absent (the dev container, some hosts)
    // skip the end-to-end run — the guard test above still covers the safety path.
    if Command::new("psql").arg("--version").output().is_err() {
        eprintln!("skipping heal-script --fix — psql not on PATH");
        bed.teardown().await;
        return;
    }
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        bed.teardown().await;
        return;
    };
    let bed_url = swap_db(&base_url, bed.db_name());
    insert_orphan(&bed.pool, "public._sqlx_migrations", 9999).await;

    // Dry run lists the orphan without deleting it.
    let dry = Command::new("bash")
        .arg(heal_script())
        .env_remove("APP_ENV")
        .env("DATABASE_URL", &bed_url)
        .output()
        .expect("run heal script (dry)");
    assert!(dry.status.success(), "dry run should succeed");
    assert!(
        String::from_utf8_lossy(&dry.stdout).contains("9999"),
        "dry run should list the orphan version"
    );
    let still_there: i64 =
        sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE version = 9999")
            .fetch_one(&bed.pool)
            .await
            .unwrap();
    assert_eq!(still_there, 1, "dry run must not delete anything");

    // --fix --yes deletes exactly the orphan.
    let fixed = Command::new("bash")
        .arg(heal_script())
        .arg("--fix")
        .arg("--yes")
        .env_remove("APP_ENV")
        .env("DATABASE_URL", &bed_url)
        .output()
        .expect("run heal script (fix)");
    assert!(
        fixed.status.success(),
        "fix run failed: {}",
        String::from_utf8_lossy(&fixed.stderr)
    );
    let orphan_gone: i64 =
        sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE version = 9999")
            .fetch_one(&bed.pool)
            .await
            .unwrap();
    assert_eq!(orphan_gone, 0, "the orphan row must be deleted");
    // A real applied migration must be untouched.
    let real_kept: i64 =
        sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE version = 1")
            .fetch_one(&bed.pool)
            .await
            .unwrap();
    assert_eq!(real_kept, 1, "a genuine migration row must remain");

    bed.teardown().await;
}
