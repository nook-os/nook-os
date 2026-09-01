//! MAIN-169: the auth-mode backfill migration, and the break-glass
//! has-local-credentials signal, exercised against a real database.
//!
//! The state-machine half (configured / usable / degraded, and on-demand
//! discovery) is unit-tested in `nook_control::auth`; what needs a database is
//! the migration's classification of pre-lock tenants and the exact predicate
//! the login page's break-glass form keys on. Each test creates its own tenants
//! and asserts only about them (a private `TestBed` database, so isolation is
//! total either way).

use nook_db::{params, Db, EnginePool};
use nook_testkit::TestBed;
use sqlx::PgPool;
use uuid::Uuid;

/// The real migration, run verbatim — no drift between what the test proves and
/// what ships.
///
/// It is TWO statements, and `Db` has no multi-statement path (MAIN-393), so
/// running it needs `sqlx::raw_sql` against a raw Postgres pool. Splitting it
/// here would be exactly the drift the line above exists to prevent.
const BACKFILL_SQL: &str = include_str!("../../migrations/0022_backfill_auth_mode.sql");

async fn tenant(db: &EnginePool) -> Uuid {
    let id = Uuid::now_v7();
    // auth_mode defaults to NULL — the pre-lock state the backfill targets.
    db.exec(
        "INSERT INTO tenants (id, slug, name) VALUES ($1, $2, $2)",
        params![id, format!("m169-{}", Uuid::now_v7().simple())],
    )
    .await
    .expect("tenant");
    id
}

/// A user with a local password set.
async fn user_with_password(db: &EnginePool, tenant: Uuid) -> Uuid {
    let id = Uuid::now_v7();
    db.exec(
        "INSERT INTO users (id, tenant_id, display_name, email, password_hash)
         VALUES ($1, $2, 'L', $3, 'argon2-hash')",
        params![id, tenant, format!("l-{}@example.test", id.simple())],
    )
    .await
    .expect("local user");
    id
}

/// A user with a federated identity and NO local password (an OIDC account).
async fn user_with_identity(db: &EnginePool, tenant: Uuid) -> Uuid {
    let id = Uuid::now_v7();
    db.exec(
        "INSERT INTO users (id, tenant_id, display_name, email)
         VALUES ($1, $2, 'O', $3)",
        params![id, tenant, format!("o-{}@example.test", id.simple())],
    )
    .await
    .expect("oidc user");
    db.exec(
        "INSERT INTO identities (id, user_id, issuer, subject)
         VALUES ($1, $2, 'https://idp.example.test', $3)",
        params![Uuid::now_v7(), id, id.simple().to_string()],
    )
    .await
    .expect("identity");
    id
}

async fn mode_of(db: &EnginePool, tenant: Uuid) -> Option<String> {
    let (m,): (Option<String>,) = db
        .query_one(
            "SELECT auth_mode FROM tenants WHERE id = $1",
            params![tenant],
        )
        .await
        .expect("read auth_mode");
    m
}

#[tokio::test]
async fn backfill_classifies_only_unambiguous_tenants() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let db = bed.db();
    // Its OWN pool, from the bed's URL, for `raw_sql` alone — see BACKFILL_SQL.
    // `bed.pool` used to hand this out; MAIN-268 removed it, and this is the one
    // consumer with a reason the engine-neutral API cannot serve.
    let Some(url) = bed.database_url() else {
        // SQLite: `database_url` is None, and a Postgres migration file is not
        // the thing to run there anyway.
        bed.teardown().await;
        return;
    };
    let pool = PgPool::connect(&url).await.expect("own pool for raw_sql");

    // OIDC-only: an identity, no local password → 'oidc'.
    let oidc_only = tenant(&db).await;
    user_with_identity(&db, oidc_only).await;

    // Local-only: a password, no identity → 'local'.
    let local_only = tenant(&db).await;
    user_with_password(&db, local_only).await;

    // Mixed: both signals → left NULL (a human must decide).
    let mixed = tenant(&db).await;
    user_with_identity(&db, mixed).await;
    user_with_password(&db, mixed).await;

    // Empty: neither → left NULL.
    let empty = tenant(&db).await;

    sqlx::raw_sql(BACKFILL_SQL)
        .execute(&pool)
        .await
        .expect("backfill runs");

    assert_eq!(mode_of(&db, oidc_only).await.as_deref(), Some("oidc"));
    assert_eq!(mode_of(&db, local_only).await.as_deref(), Some("local"));
    assert_eq!(mode_of(&db, mixed).await, None, "mixed stays undecided");
    assert_eq!(mode_of(&db, empty).await, None, "empty stays undecided");

    // Idempotent: a second run changes nothing.
    sqlx::raw_sql(BACKFILL_SQL)
        .execute(&pool)
        .await
        .expect("backfill re-runs");
    assert_eq!(mode_of(&db, oidc_only).await.as_deref(), Some("oidc"));
    assert_eq!(mode_of(&db, local_only).await.as_deref(), Some("local"));

    // And it never overwrites a mode that is already set.
    let already = tenant(&db).await;
    db.exec(
        "UPDATE tenants SET auth_mode = 'local' WHERE id = $1",
        params![already],
    )
    .await
    .unwrap();
    user_with_identity(&db, already).await; // would say 'oidc' if it re-ran
    sqlx::raw_sql(BACKFILL_SQL).execute(&pool).await.unwrap();
    assert_eq!(
        mode_of(&db, already).await.as_deref(),
        Some("local"),
        "an already-committed mode is never rewritten"
    );

    pool.close().await;
    bed.teardown().await;
}

/// The exact predicate `/auth/local/status` reports as `has_local_credentials`
/// — the gate the login page's break-glass form keys on (AC-5).
#[tokio::test]
async fn has_local_credentials_tracks_password_presence() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let db = bed.db();

    let has = tenant(&db).await;
    user_with_password(&db, has).await;

    let none = tenant(&db).await;
    user_with_identity(&db, none).await; // OIDC account: no password here

    async fn local_creds(db: &EnginePool, tenant: Uuid) -> i64 {
        let (n,): (i64,) = db
            .query_one(
                "SELECT count(*) FROM users WHERE tenant_id = $1 AND password_hash IS NOT NULL",
                params![tenant],
            )
            .await
            .unwrap();
        n
    }

    assert!(local_creds(&db, has).await > 0, "a password user counts");
    assert_eq!(
        local_creds(&db, none).await,
        0,
        "an OIDC-only tenant has no break-glass credential"
    );

    bed.teardown().await;
}
