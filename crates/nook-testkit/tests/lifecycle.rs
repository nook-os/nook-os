//! MAIN-429: the database-lifecycle surface `nook-testkit` now provisions
//! through — `nook_db::test_support`.
//!
//! Here rather than in `nook-db` because these need a live server, and this
//! crate already owns the `DATABASE_URL` conventions (skip without one, hard
//! fail under `NOOK_REQUIRE_DB`). The harness's own suite is the regression
//! test for the move; what these add is the surface's own contract, including
//! the one property the harness cannot demonstrate about itself.

use nook_db::test_support::{self as lifecycle, AdminConn, Provisioned};
use uuid::Uuid;

/// A Postgres base URL, or `None` to skip. Mirrors `TestBed::new`'s rule so a
/// missing database in CI is a failure rather than a silent pass.
fn base_url() -> Option<String> {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        assert!(
            std::env::var("NOOK_REQUIRE_DB").is_err(),
            "NOOK_REQUIRE_DB is set but DATABASE_URL is not"
        );
        return None;
    };
    url.starts_with("postgres").then_some(url)
}

fn pg(base_url: &str, db_name: &str) -> Provisioned {
    Provisioned::Pg {
        base_url: base_url.to_string(),
        db_name: db_name.to_string(),
    }
}

fn a_name() -> String {
    format!("nook_lifecycle_{}", Uuid::now_v7().simple())
}

/// Provision → exists → destroy → gone.
#[tokio::test]
async fn a_database_round_trips_through_the_surface() {
    let Some(base) = base_url() else { return };
    let name = a_name();
    let what = pg(&base, &name);

    let mut admin = AdminConn::connect(&base).await.expect("connect");
    admin.create_database(&name).await.expect("create");
    admin.close().await;

    assert!(lifecycle::exists(&what).await, "created, so it exists");
    lifecycle::destroy(&what).await;
    assert!(!lifecycle::exists(&what).await, "destroyed, so it is gone");
}

/// AC-4's independence pair. `exists()` is what teardown is ASSERTED with, so
/// it must not share a code path with `destroy()` — a bug that made the drop a
/// no-op must not also make the check report absence and agree with it.
///
/// Both directions are driven WITHOUT `destroy`: the database is removed
/// out-of-band and `exists` must still notice, and created out-of-band and
/// `exists` must still see it.
#[tokio::test]
async fn exists_answers_from_the_catalogue_not_from_destroy() {
    let Some(base) = base_url() else { return };
    let name = a_name();
    let what = pg(&base, &name);
    let mut admin = AdminConn::connect(&base).await.expect("connect");

    admin.create_database(&name).await.expect("create");
    assert!(
        lifecycle::exists(&what).await,
        "created out-of-band — exists must see it without destroy involved"
    );

    // Dropped out-of-band, so nothing `destroy` does can be what makes this
    // report absence.
    admin.drop_database(&name).await.expect("drop");
    assert!(
        !lifecycle::exists(&what).await,
        "dropped out-of-band — exists must report absence on its own"
    );
    admin.close().await;
}

/// The guard that stops the reaper dropping a template another suite is
/// cloning from: a database with a live connection is not "unused".
#[tokio::test]
async fn a_database_in_use_is_not_reported_unused() {
    let Some(base) = base_url() else { return };
    let name = a_name();
    let what = pg(&base, &name);
    let mut admin = AdminConn::connect(&base).await.expect("connect");
    admin.create_database(&name).await.expect("create");

    // Nothing connected yet — it is a candidate.
    let idle = admin
        .unused_databases_like("nook_lifecycle_%", "")
        .await
        .expect("scan");
    assert!(idle.contains(&name), "idle, so it is reported unused");

    // Hold a connection open and it must drop out of the scan.
    let pool = lifecycle::open(&what).await.expect("open");
    let busy = admin
        .unused_databases_like("nook_lifecycle_%", "")
        .await
        .expect("scan");
    assert!(
        !busy.contains(&name),
        "something is connected — it must NOT be offered to the reaper"
    );

    // …and `except` excludes by name whatever the connection state.
    let excepted = admin
        .unused_databases_like("nook_lifecycle_%", &name)
        .await
        .expect("scan");
    assert!(!excepted.contains(&name));

    pool.pg().close().await;
    admin.drop_database(&name).await.ok();
    admin.close().await;
}

/// A template clone arrives carrying the template's schema — that is what makes
/// it a substitute for migrate-and-seed rather than an empty database.
#[tokio::test]
async fn a_template_clone_carries_the_templates_schema() {
    let Some(base) = base_url() else { return };
    let template = a_name();
    let clone = a_name();
    let mut admin = AdminConn::connect(&base).await.expect("connect");
    admin.create_database(&template).await.expect("create");

    // Put a table in the template, then release every connection so it can be
    // used as one (Postgres refuses to clone a database in use).
    let tpl = lifecycle::open(&pg(&base, &template)).await.expect("open");
    sqlx::query("CREATE TABLE marker (id int)")
        .execute(tpl.pg())
        .await
        .expect("create a table in the template");
    tpl.pg().close().await;

    admin
        .create_database_from_template(&clone, &template)
        .await
        .expect("clone");

    let cloned = lifecycle::open(&pg(&base, &clone)).await.expect("open");
    let found: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.tables WHERE table_name = 'marker'",
    )
    .fetch_one(cloned.pg())
    .await
    .expect("look for the marker table");
    assert_eq!(found, 1, "the clone carries the template's schema");
    cloned.pg().close().await;

    lifecycle::destroy(&pg(&base, &clone)).await;
    lifecycle::destroy(&pg(&base, &template)).await;
    admin.close().await;
}

/// The SQLite arm of the same contract, including the sidecars: "the file is
/// gone" and "the database is gone" are not the same claim.
#[tokio::test]
async fn the_sqlite_arm_round_trips_and_removes_its_sidecars() {
    let path = std::env::temp_dir().join(format!("nook_lifecycle_{}.db", Uuid::now_v7().simple()));
    let what = Provisioned::Sqlite { path: path.clone() };

    let pool = lifecycle::open_sqlite_bed(&path).await.expect("open");
    sqlx::query("CREATE TABLE marker (id integer)")
        .execute(pool.sqlite())
        .await
        .expect("write something so the file is real");
    assert!(lifecycle::exists(&what).await, "the file exists");
    pool.sqlite().close().await;

    lifecycle::destroy(&what).await;
    assert!(!lifecycle::exists(&what).await, "the file is gone");
    for ext in ["-wal", "-shm"] {
        assert!(
            !std::path::Path::new(&format!("{}{ext}", path.display())).exists(),
            "the {ext} sidecar goes with it"
        );
    }
}
