//! MAIN-48 AC-5: chat's migrations apply into the `chat` schema, and its ledger
//! does NOT collide with the control plane's `public._sqlx_migrations`.
//!
//! DB-backed; no-ops without `NOOK_REQUIRE_DB=1`, matching the suite convention.

use std::str::FromStr;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Create the `chat` schema. Mirrors the service's own `ensure_chat_schema`,
/// which is `pub(crate)` in a bin crate and so unreachable from here — including
/// its `23505` tolerance: the database is this test's own now (MAIN-165), so the
/// `pg_namespace` race with a sibling binary cannot happen, but a helper that
/// quietly differs from the one it mirrors is worse than one extra arm.
async fn ensure_chat_schema(db: &sqlx::PgPool) {
    match sqlx::query("CREATE SCHEMA IF NOT EXISTS chat")
        .execute(db)
        .await
    {
        Ok(_) => {}
        Err(e) if e.as_database_error().and_then(|d| d.code()).as_deref() == Some("23505") => {}
        Err(e) => panic!("create chat schema: {e}"),
    }
}

#[tokio::test]
async fn chat_migrations_apply_into_an_isolated_schema() {
    // A PRIVATE database (MAIN-165). Running chat's MIGRATOR is the whole point
    // of this test, and doing it against the shared dev database stamped the
    // shared `chat._sqlx_migrations` — which is precisely how one branch's chat
    // migration used to give the next branch a `VersionMismatch`.
    let Some(mut bed) = nook_testkit::TestBed::new().await else {
        return;
    };
    // Postgres semantics are the whole subject here — `search_path` resolution
    // and `information_schema` isolation have no SQLite equivalent — so this
    // skips rather than pretending to assert something (MAIN-294).
    if !bed.is_postgres() {
        eprintln!(
            "skipping chat_migrations_apply_into_an_isolated_schema — Postgres-only behaviour"
        );
        bed.teardown().await;
        return;
    }
    let url = bed.database_url().expect("a Postgres bed exposes its URL");

    // The same connection setup the service uses: search_path pinned to `chat`.
    let opts = PgConnectOptions::from_str(&url)
        .unwrap()
        .options([("search_path", "chat")]);
    let db = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(opts)
        .await
        .unwrap();

    ensure_chat_schema(&db).await;
    MIGRATOR.run(&db).await.unwrap();

    // chat_channels lives in the `chat` schema, with the owner_type CHECK.
    let (in_chat,): (bool,) = sqlx::query_as(
        "SELECT EXISTS(SELECT 1 FROM information_schema.tables
                       WHERE table_schema = 'chat' AND table_name = 'chat_channels')",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert!(in_chat, "chat_channels is created in the chat schema");

    // The ledger is chat's own — a separate table from public._sqlx_migrations.
    let (ledger_in_chat,): (bool,) = sqlx::query_as(
        "SELECT EXISTS(SELECT 1 FROM information_schema.tables
                       WHERE table_schema = 'chat' AND table_name = '_sqlx_migrations')",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert!(
        ledger_in_chat,
        "chat's migration ledger lives in the chat schema, not public"
    );

    // The owner_type CHECK constraint is present (('org','tenant')).
    let bad = sqlx::query(
        "INSERT INTO chat.chat_channels (id, owner_type, owner_id, name, slug)
         VALUES (gen_random_uuid(), 'nope', gen_random_uuid(), 'x', 'x')",
    )
    .execute(&db)
    .await;
    assert!(bad.is_err(), "owner_type is constrained to org|tenant");

    // Re-running is idempotent (no error on a second apply).
    MIGRATOR.run(&db).await.unwrap();

    db.close().await;
    bed.teardown().await;
}
