//! The harness works on both engines (MAIN-242 AC-5/AC-6).
//!
//! **The same file, run twice**, is the deliverable — not two files that happen
//! to agree. Run it under `DATABASE_URL=postgres://…` (what `./test.sh` does)
//! and under `DATABASE_URL=sqlite:///tmp/t.db`; nothing here is engine-specific
//! except the assertions *about* the engine.
//!
//! ```text
//! DATABASE_URL=sqlite:///tmp/nook-pilot.db \
//!   cargo test -p nook-testkit --test dual_engine
//! ```
//!
//! What that proves, and why each part is here: the scheme picks the engine, the
//! matching migration track ran (so the schema is real, not plausible), the seed
//! ran, the entity helpers work through the engine-agnostic surface, `AppState`
//! builds, and teardown removes what the bed created. A harness missing any one
//! of those cannot host the test-site sweep that comes next.

use nook_db::{params, Db, Engine};
use nook_testkit::TestBed;

/// AC-5, the pilot: one DB-backed test, both engines, through the harness.
#[tokio::test]
async fn the_pilot_runs_on_whichever_engine_database_url_names() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };

    // AC-3 — the seed ran. Roles are seeded on every install, so their presence
    // means the migration track produced a real schema and `seed::run` executed
    // against it. Checked through the engine-agnostic surface, which is the
    // point: this line is identical on both engines.
    let roles: i64 = bed
        .db()
        .query_scalar("SELECT count(*) FROM roles", params![])
        .await
        .expect("count seeded roles");
    assert!(roles > 0, "the seed ran on {:?}", bed.engine());

    // AC-4 — the entity helpers are engine-agnostic now (they used to bind raw
    // `sqlx` against a `PgPool`).
    let tenant = bed.tenant("pilot").await;
    let (user, person) = bed.user(tenant, "admin").await;
    let node = bed.node(tenant, person).await;
    let workspace = bed.workspace(tenant).await;

    // …and what they wrote is readable back, per engine, with the ids intact.
    // Counting rows would pass against a helper that wrote the wrong tenant;
    // matching on the id is what makes this an assertion about the write.
    for (table, id) in [
        ("users", user.0),
        ("nodes", node.0),
        ("workspaces", workspace.0),
    ] {
        let found: i64 = bed
            .db()
            .query_scalar(
                &format!("SELECT count(*) FROM {table} WHERE id = $1 AND tenant_id = $2"),
                params![id, tenant],
            )
            .await
            .unwrap_or_else(|e| panic!("read back {table}: {e}"));
        assert_eq!(found, 1, "{table} row is there, in the right tenant");
    }

    // The bed can build the real application state on either engine — the thing
    // most integration tests do first.
    let _state = bed.app_state().await;

    bed.teardown().await;
}

/// AC-6 — the scheme really did select the engine. Without this the pilot above
/// would pass just as happily against Postgres twice.
#[tokio::test]
async fn the_bed_is_on_the_engine_the_url_named() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").expect("a bed implies a URL");
    let expected = if url.starts_with("sqlite") {
        Engine::Sqlite
    } else {
        Engine::Postgres
    };
    assert_eq!(
        bed.engine(),
        expected,
        "URL {url} selected the wrong engine"
    );
    bed.teardown().await;
}

/// AC-6 — a `sqlite://` bed never opens a Postgres connection.
///
/// The `pool` field still exists on a SQLite bed because ~650 Postgres-leg call
/// sites need it to (NG-1). This pins the two things that makes safe: it has
/// opened nothing, and it is not secretly pointed at the real server — where it
/// would *work*, and quietly run an allegedly-isolated test against the shared
/// dev database.
#[tokio::test]
async fn a_sqlite_bed_opens_no_postgres_connection() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    if bed.is_postgres() {
        bed.teardown().await;
        return;
    }

    assert_eq!(
        bed.pool.size(),
        0,
        "the escape-hatch pool must never have connected"
    );
    // Using it fails rather than succeeding. That is the whole guarantee, and
    // it is the one that matters: a pool aimed at the real server would have
    // *worked*, running this supposedly-isolated test against the shared dev
    // database. The error text is deliberately NOT asserted — sqlx omits the
    // connection target, so there is nothing engine-specific in it to pin, and
    // a test asserting the generic phrasing would only break on a sqlx upgrade.
    // The explanation lives on `inert_pg_pool`'s doc comment instead.
    sqlx::query("SELECT 1")
        .execute(&bed.pool)
        .await
        .expect_err("bed.pool must not work on a SQLite bed");
    assert_eq!(
        bed.pool.size(),
        0,
        "and it still has not connected after the attempt"
    );

    bed.teardown().await;
}

/// AC-2/AC-6 — teardown removes the SQLite artifact, and `keep` preserves it.
/// The Postgres half of this contract is `tests/teardown.rs`; this is its twin,
/// asserted against the filesystem rather than `pg_database`.
#[tokio::test]
async fn sqlite_teardown_removes_the_file_and_keep_preserves_it() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    if bed.is_postgres() {
        bed.teardown().await;
        return;
    }

    let path = std::path::PathBuf::from(bed.db_name());
    // Say which half is under test rather than inheriting it from the
    // environment: with `NOOK_KEEP_TEST_DATA=1` exported — the documented way to
    // keep artifacts for debugging — every bed comes back with keep already on,
    // and this half would assert removal against a bed told to preserve.
    bed.set_keep(false);
    // Write something, so "gone" means the database and not an empty file that
    // was never used.
    let _tenant = bed.tenant("teardown").await;
    assert!(path.exists(), "the private SQLite file exists while live");

    bed.teardown().await;
    assert!(!path.exists(), "teardown removes the private SQLite file");
    for ext in ["-wal", "-shm"] {
        let sidecar = format!("{}{ext}", path.display());
        assert!(
            !std::path::Path::new(&sidecar).exists(),
            "teardown removes {ext} too — a stray sidecar is half a database"
        );
    }

    // …and under keep, it survives for inspection (the `NOOK_KEEP_TEST_DATA`
    // contract). Set directly rather than via the env var, so this cannot race
    // its siblings through process-global state.
    let Some(mut kept) = TestBed::new().await else {
        return;
    };
    kept.set_keep(true);
    let kept_path = std::path::PathBuf::from(kept.db_name());
    kept.teardown().await;
    assert!(kept_path.exists(), "keep leaves the file for debugging");
    drop(kept);
    // Do not leak it: Drop honours keep as well, so remove it by hand.
    let _ = std::fs::remove_file(&kept_path);
    for ext in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{ext}", kept_path.display()));
    }
}
