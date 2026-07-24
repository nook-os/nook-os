//! MAIN-48 AC-5: chat's migrations apply into the `chat` schema, and its ledger
//! does NOT collide with the control plane's `public._sqlx_migrations`.
//!
//! DB-backed; no-ops without `NOOK_REQUIRE_DB=1`, matching the suite convention.

use std::str::FromStr;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

#[tokio::test]
async fn chat_migrations_apply_into_an_isolated_schema() {
    if std::env::var("NOOK_REQUIRE_DB").ok().as_deref() != Some("1") {
        eprintln!("skipping chat_migrations_apply_into_an_isolated_schema — no NOOK_REQUIRE_DB");
        return;
    }
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return;
    };

    // The same connection setup the service uses: search_path pinned to `chat`.
    let opts = PgConnectOptions::from_str(&url)
        .unwrap()
        .options([("search_path", "chat")]);
    let db = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(opts)
        .await
        .unwrap();

    sqlx::query("CREATE SCHEMA IF NOT EXISTS chat")
        .execute(&db)
        .await
        .unwrap();
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
}
