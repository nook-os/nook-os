//! MAIN-169: the auth-mode backfill migration, and the break-glass
//! has-local-credentials signal, exercised against a real database.
//!
//! The state-machine half (configured / usable / degraded, and on-demand
//! discovery) is unit-tested in `nook_control::auth`; what needs a database is
//! the migration's classification of pre-lock tenants and the exact predicate
//! the login page's break-glass form keys on. Each test creates its own tenants
//! and asserts only about them (a private `TestBed` database, so isolation is
//! total either way).

use nook_testkit::TestBed;
use sqlx::PgPool;
use uuid::Uuid;

/// The real migration, run verbatim — no drift between what the test proves and
/// what ships.
const BACKFILL_SQL: &str = include_str!("../migrations/0022_backfill_auth_mode.sql");

async fn tenant(pool: &PgPool) -> Uuid {
    let id = Uuid::now_v7();
    // auth_mode defaults to NULL — the pre-lock state the backfill targets.
    sqlx::query("INSERT INTO tenants (id, slug, name) VALUES ($1, $2, $2)")
        .bind(id)
        .bind(format!("m169-{}", Uuid::now_v7().simple()))
        .execute(pool)
        .await
        .expect("tenant");
    id
}

/// A user with a local password set.
async fn user_with_password(pool: &PgPool, tenant: Uuid) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO users (id, tenant_id, display_name, email, password_hash)
         VALUES ($1, $2, 'L', $3, 'argon2-hash')",
    )
    .bind(id)
    .bind(tenant)
    .bind(format!("l-{}@example.test", id.simple()))
    .execute(pool)
    .await
    .expect("local user");
    id
}

/// A user with a federated identity and NO local password (an OIDC account).
async fn user_with_identity(pool: &PgPool, tenant: Uuid) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO users (id, tenant_id, display_name, email)
         VALUES ($1, $2, 'O', $3)",
    )
    .bind(id)
    .bind(tenant)
    .bind(format!("o-{}@example.test", id.simple()))
    .execute(pool)
    .await
    .expect("oidc user");
    sqlx::query(
        "INSERT INTO identities (id, user_id, issuer, subject)
         VALUES ($1, $2, 'https://idp.example.test', $3)",
    )
    .bind(Uuid::now_v7())
    .bind(id)
    .bind(id.simple().to_string())
    .execute(pool)
    .await
    .expect("identity");
    id
}

async fn mode_of(pool: &PgPool, tenant: Uuid) -> Option<String> {
    let (m,): (Option<String>,) = sqlx::query_as("SELECT auth_mode FROM tenants WHERE id = $1")
        .bind(tenant)
        .fetch_one(pool)
        .await
        .expect("read auth_mode");
    m
}

#[tokio::test]
async fn backfill_classifies_only_unambiguous_tenants() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let pool = bed.pool.clone();

    // OIDC-only: an identity, no local password → 'oidc'.
    let oidc_only = tenant(&pool).await;
    user_with_identity(&pool, oidc_only).await;

    // Local-only: a password, no identity → 'local'.
    let local_only = tenant(&pool).await;
    user_with_password(&pool, local_only).await;

    // Mixed: both signals → left NULL (a human must decide).
    let mixed = tenant(&pool).await;
    user_with_identity(&pool, mixed).await;
    user_with_password(&pool, mixed).await;

    // Empty: neither → left NULL.
    let empty = tenant(&pool).await;

    sqlx::raw_sql(BACKFILL_SQL)
        .execute(&pool)
        .await
        .expect("backfill runs");

    assert_eq!(mode_of(&pool, oidc_only).await.as_deref(), Some("oidc"));
    assert_eq!(mode_of(&pool, local_only).await.as_deref(), Some("local"));
    assert_eq!(mode_of(&pool, mixed).await, None, "mixed stays undecided");
    assert_eq!(mode_of(&pool, empty).await, None, "empty stays undecided");

    // Idempotent: a second run changes nothing.
    sqlx::raw_sql(BACKFILL_SQL)
        .execute(&pool)
        .await
        .expect("backfill re-runs");
    assert_eq!(mode_of(&pool, oidc_only).await.as_deref(), Some("oidc"));
    assert_eq!(mode_of(&pool, local_only).await.as_deref(), Some("local"));

    // And it never overwrites a mode that is already set.
    let already = tenant(&pool).await;
    sqlx::query("UPDATE tenants SET auth_mode = 'local' WHERE id = $1")
        .bind(already)
        .execute(&pool)
        .await
        .unwrap();
    user_with_identity(&pool, already).await; // would say 'oidc' if it re-ran
    sqlx::raw_sql(BACKFILL_SQL).execute(&pool).await.unwrap();
    assert_eq!(
        mode_of(&pool, already).await.as_deref(),
        Some("local"),
        "an already-committed mode is never rewritten"
    );

    bed.teardown().await;
}

/// The exact predicate `/auth/local/status` reports as `has_local_credentials`
/// — the gate the login page's break-glass form keys on (AC-5).
#[tokio::test]
async fn has_local_credentials_tracks_password_presence() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let pool = bed.pool.clone();

    let has = tenant(&pool).await;
    user_with_password(&pool, has).await;

    let none = tenant(&pool).await;
    user_with_identity(&pool, none).await; // OIDC account: no password here

    async fn local_creds(pool: &PgPool, tenant: Uuid) -> i64 {
        let (n,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM users WHERE tenant_id = $1 AND password_hash IS NOT NULL",
        )
        .bind(tenant)
        .fetch_one(pool)
        .await
        .unwrap();
        n
    }

    assert!(local_creds(&pool, has).await > 0, "a password user counts");
    assert_eq!(
        local_creds(&pool, none).await,
        0,
        "an OIDC-only tenant has no break-glass credential"
    );

    bed.teardown().await;
}
