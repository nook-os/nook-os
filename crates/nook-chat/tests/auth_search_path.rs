//! MAIN-87 regression: nook-chat pins its pool to `search_path=chat,public`, so
//! the shared `nook-auth` queries — which run **unqualified** `sessions_auth` /
//! `tenant_members` / `user_tokens`, tables that live only in `public` — still
//! resolve. When chat was pinned to `chat` alone, every authenticated request
//! 500'd with `relation "sessions_auth" does not exist`.
//!
//! This test drives `resolve_session` over a pool configured EXACTLY as the
//! service configures it (`crates/nook-chat/src/main.rs`), and fails if the
//! search_path ever again excludes `public`.
//!
//! DB-backed; no-ops without `NOOK_REQUIRE_DB=1`, matching the suite convention.

use std::str::FromStr;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::PgPool;
use uuid::Uuid;

async fn pool(url: &str, search_path: &str) -> PgPool {
    let opts = PgConnectOptions::from_str(url)
        .unwrap()
        .options([("search_path", search_path)]);
    PgPoolOptions::new()
        .max_connections(2)
        .connect_with(opts)
        .await
        .unwrap()
}

/// Seed a tenant + user + live session + membership in `public`, returning the
/// session id. Explicitly schema-qualified so setup does not itself depend on the
/// search_path under test.
async fn seed(db: &PgPool) -> (Uuid, Uuid) {
    let tenant = Uuid::now_v7();
    let user = Uuid::now_v7();
    let session = Uuid::now_v7();
    sqlx::query("INSERT INTO public.tenants (id, name, slug) VALUES ($1, $2, $2)")
        .bind(tenant)
        .bind(format!("t-{}", tenant.simple()))
        .execute(db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO public.users (id, tenant_id, person_id, display_name, email)
         VALUES ($1, $2, gen_random_uuid(), 'U', $3)",
    )
    .bind(user)
    .bind(tenant)
    .bind(format!("u-{}@example.test", user.simple()))
    .execute(db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO public.sessions_auth (id, user_id, tenant_id, expires_at)
         VALUES ($1, $2, $3, now() + interval '1 hour')",
    )
    .bind(session)
    .bind(user)
    .bind(tenant)
    .execute(db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO public.tenant_members (id, tenant_id, principal_type, principal_id)
         VALUES (gen_random_uuid(), $1, 'user', $2)",
    )
    .bind(tenant)
    .bind(user)
    .execute(db)
    .await
    .unwrap();
    (tenant, session)
}

#[tokio::test]
async fn resolve_session_resolves_public_auth_tables_on_the_chat_first_pool() {
    if std::env::var("NOOK_REQUIRE_DB").ok().as_deref() != Some("1") {
        eprintln!("skipping chat search_path regression — no NOOK_REQUIRE_DB");
        return;
    }
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return;
    };

    // Provision the control-plane `public` schema this test depends on. The
    // nook-chat crate ships only its own `chat` migrations, so a fresh CI
    // database has no `public.tenants`/`users`/`sessions_auth`/`tenant_members` —
    // seeding would then fail on `relation "public.tenants" does not exist`.
    // Running the control plane's own MIGRATOR (idempotent) creates them exactly
    // as production has them, rather than hand-maintaining table DDL that could
    // drift. The `chat` schema must also exist for a `chat,public` pool to connect.
    let bootstrap = pool(&url, "public").await;
    sqlx::query("CREATE SCHEMA IF NOT EXISTS chat")
        .execute(&bootstrap)
        .await
        .unwrap();
    nook_control::MIGRATOR
        .run(&bootstrap)
        .await
        .expect("control-plane migrations must provision the public auth tables");
    let (tenant, session) = seed(&bootstrap).await;

    // EXACTLY the service's pool config (main.rs): `chat` first so chat's own
    // tables and ledger resolve there, `public` as the fallback for the shared
    // auth tables. This must resolve the session.
    let chat_pool = pool(&url, "chat,public").await;
    let resolved = nook_auth::resolve_session(&chat_pool, session)
        .await
        .expect("resolve_session must resolve the public auth tables through chat,public");
    assert_eq!(resolved.tenant_id, tenant);

    // The guard bites: a `chat`-only search_path — the bug — cannot resolve
    // `public.sessions_auth`, so this errors. If a future change narrows the
    // service's search_path back to `chat`, the assertion above starts failing.
    let chat_only = pool(&url, "chat").await;
    assert!(
        nook_auth::resolve_session(&chat_only, session)
            .await
            .is_err(),
        "a chat-only search_path must NOT resolve the public auth tables — \
         that regression is exactly what shipped and 500'd in prod"
    );

    // Cascades to users / sessions_auth / tenant_members via the tenant FK.
    sqlx::query("DELETE FROM public.tenants WHERE id = $1")
        .bind(tenant)
        .execute(&bootstrap)
        .await
        .unwrap();
}

/// The runtime test above uses a `chat,public` pool it builds itself, so it
/// cannot see a change to how the *service* pins its pool (a bin crate's
/// `main.rs` is not importable from an integration test). This ties the guard to
/// the real config: the service's `search_path` must keep `public`. Narrowing it
/// back to `chat` — the exact prod regression — fails here. Source-parsing to
/// enforce an invariant follows the `operator_writes.rs` precedent in the tree.
#[test]
fn the_service_pool_search_path_keeps_public() {
    let src = include_str!("../src/main.rs");
    // Grab every `("search_path", "…")` value the service configures.
    let values: Vec<&str> = src
        .match_indices("\"search_path\"")
        .filter_map(|(i, _)| {
            let rest = &src[i..];
            // …, "<value>")  — the second quoted string after the key.
            let mut quotes = rest.match_indices('"').map(|(q, _)| q);
            let open = quotes.nth(2)?; // 0,1 = the key; 2 = value open
            let close = quotes.next()?; // 3 = value close
            Some(&rest[open + 1..close])
        })
        .collect();

    assert!(
        !values.is_empty(),
        "expected the service to pin a search_path in main.rs"
    );
    for v in values {
        assert!(
            v.split(',').any(|s| s.trim() == "public"),
            "the service search_path {v:?} must include `public`, or the shared \
             nook-auth tables (sessions_auth/tenant_members/user_tokens) 500 — \
             this is MAIN-87. Keep `public` in the search_path."
        );
    }
}
