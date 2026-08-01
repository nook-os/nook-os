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

/// Create the `chat` schema, tolerating the concurrent-creation race (MAIN-93):
/// `CREATE SCHEMA IF NOT EXISTS` is not atomic, so the loser of a `pg_namespace`
/// insert race between the chat test binaries gets `23505` though the schema now
/// exists — a duplicate is success. Mirrors the service's `ensure_chat_schema`,
/// which is unreachable from an integration test.
async fn ensure_chat_schema(db: &PgPool) {
    match sqlx::query("CREATE SCHEMA IF NOT EXISTS chat")
        .execute(db)
        .await
    {
        Ok(_) => {}
        Err(e) if e.as_database_error().and_then(|d| d.code()).as_deref() == Some("23505") => {}
        Err(e) => panic!("create chat schema: {e}"),
    }
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
    // A PRIVATE database (MAIN-165), not the shared dev one: this test seeds
    // tenants/users/sessions into `public`, and doing that on the shared database
    // is what the whole card exists to stop. The bed arrives with `public`
    // already migrated by the control track, which is exactly what this test
    // used to run the control MIGRATOR by hand to get.
    let Some(mut bed) = nook_testkit::TestBed::new().await else {
        return;
    };
    // Postgres semantics are the whole subject here — `search_path` resolution
    // and `information_schema` isolation have no SQLite equivalent — so this
    // skips rather than pretending to assert something (MAIN-294).
    if !bed.is_postgres() {
        eprintln!("skipping the chat search_path regression tests — Postgres-only behaviour");
        bed.teardown().await;
        return;
    }
    let url = bed.database_url().expect("a Postgres bed exposes its URL");

    // The `chat` schema must exist for a `chat,public` pool to connect.
    let bootstrap = pool(&url, "public").await;
    ensure_chat_schema(&bootstrap).await;
    let (tenant, session) = seed(&bootstrap).await;

    // EXACTLY the service's pool config (main.rs): `chat` first so chat's own
    // tables and ledger resolve there, `public` as the fallback for the shared
    // auth tables. This must resolve the session.
    let chat_pool = pool(&url, "chat,public").await;
    let resolved =
        nook_auth::resolve_session(&nook_db::EnginePool::from_pg(chat_pool.clone()), session)
            .await
            .expect("resolve_session must resolve the public auth tables through chat,public");
    assert_eq!(resolved.tenant_id, tenant);

    // The guard bites: a `chat`-only search_path — the bug — cannot resolve
    // `public.sessions_auth`, so this errors. If a future change narrows the
    // service's search_path back to `chat`, the assertion above starts failing.
    let chat_only = pool(&url, "chat").await;
    assert!(
        nook_auth::resolve_session(&nook_db::EnginePool::from_pg(chat_only.clone()), session)
            .await
            .is_err(),
        "a chat-only search_path must NOT resolve the public auth tables — \
         that regression is exactly what shipped and 500'd in prod"
    );

    // No row deletion: the bed takes the whole database, seeded rows and all.
    // Close this test's own pools first — the bed only knows about its own.
    for p in [bootstrap, chat_pool, chat_only] {
        p.close().await;
    }
    bed.teardown().await;
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

    // The search_path used to be an inline literal at the pool construction, and
    // this guard grepped for it. MAIN-294 made it an ARGUMENT, so the value lives
    // in a named constant and the grep follows it there. The protection is
    // unchanged: MAIN-87 is that dropping `public` makes every authenticated
    // request 500 on `sessions_auth`.
    let values: Vec<(&str, &str)> = src
        .match_indices("_SEARCH_PATH: &str = \"")
        .filter_map(|(i, _)| {
            // Back up over the constant's name, for the failure message.
            let name_end = src[..i].len();
            let name_start = src[..name_end].rfind(' ')? + 1;
            let rest = &src[i..];
            let open = rest.find('"')?;
            let close = rest[open + 1..].find('"')? + open + 1;
            Some((&src[name_start..name_end], &rest[open + 1..close]))
        })
        .collect();

    assert!(
        !values.is_empty(),
        "expected main.rs to declare its search_path as a `*_SEARCH_PATH` \
         constant — if that shape changed, this guard is no longer reading \
         anything and MAIN-87 is unprotected"
    );
    for (name, v) in &values {
        assert!(
            v.split(',').any(|s| s.trim() == "public"),
            "the search_path {name}_SEARCH_PATH = {v:?} must include `public`, or \
             the shared nook-auth tables (sessions_auth/tenant_members/\
             user_tokens) 500 — this is MAIN-87. Keep `public` in the search_path."
        );
    }

    // …and the constants must be the ONLY way one is set, or the next edit can
    // pass a raw literal straight past the check above.
    assert!(
        !src.contains("(\"search_path\", \""),
        "main.rs sets a search_path from an inline literal; route it through a \
         `*_SEARCH_PATH` constant so the guard above can see it"
    );
}
