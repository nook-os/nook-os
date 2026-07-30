//! Managed content store (MAIN-78) against a live Postgres. Set `DATABASE_URL`.
//!
//! Two things are exercised through the real code paths: the seed rules (fresh
//! insert, unchanged-default no-op that preserves an operator edit, newer-default
//! refresh that bumps the version) and the node-management gate on the read
//! endpoints.

use nook_control::auth::{AuthCtx, Principal};
use nook_control::routes::managed;
use nook_testkit::TestBed;
use nook_types::*;
use sqlx::PgPool;
use uuid::Uuid;

/// The real repository over the test's pool — the seed rules under test live in
/// its impl since MAIN-258, and these stay DB-backed (NG-4).
fn repo(bed: &TestBed) -> nook_control::repo::admin::DbManagedContentRepository {
    nook_control::repo::admin::DbManagedContentRepository::new(bed.db())
}

/// A synthetic managed key, unique per test run, so the seed rules can be driven
/// without touching the real `nookos`/hooks rows other tests and the running
/// control plane rely on.
fn synthetic_name() -> String {
    format!("__test_{}", Uuid::now_v7().simple())
}

async fn row(db: &PgPool, name: &str) -> Option<(String, String, i64, String)> {
    sqlx::query_as(
        "SELECT content, sha256, version, default_sha256
         FROM managed_content WHERE kind = 'skill' AND name = $1",
    )
    .bind(name)
    .fetch_optional(db)
    .await
    .unwrap()
}

#[tokio::test]
async fn seed_rules_insert_noop_and_refresh() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping managed seed test — no DATABASE_URL");
        return;
    };
    let name = synthetic_name();

    // ── Fresh: inserts at version 1, content == default ─────────────────────
    managed::upsert_default(&repo(&bed), "skill", &name, "DEFAULT-V1")
        .await
        .unwrap();
    let (content, sha, version, default_sha) = row(&bed.pool, &name).await.expect("seeded");
    assert_eq!(content, "DEFAULT-V1");
    assert_eq!(version, 1);
    assert_eq!(sha, default_sha, "a fresh row's content is its default");

    // ── Unchanged default: a re-seed is a no-op ─────────────────────────────
    managed::upsert_default(&repo(&bed), "skill", &name, "DEFAULT-V1")
        .await
        .unwrap();
    assert_eq!(row(&bed.pool, &name).await.unwrap().2, 1, "no churn");

    // ── Operator edit + unchanged default: the edit is preserved ────────────
    // Simulate sub-ticket 5 editing the row (content + sha + version change; the
    // default it was seeded from does not).
    sqlx::query(
        "UPDATE managed_content SET content = 'OPERATOR-EDIT', sha256 = 'edited', version = 9
         WHERE kind = 'skill' AND name = $1",
    )
    .bind(&name)
    .execute(&bed.pool)
    .await
    .unwrap();
    managed::upsert_default(&repo(&bed), "skill", &name, "DEFAULT-V1")
        .await
        .unwrap();
    let (content, _, version, _) = row(&bed.pool, &name).await.unwrap();
    assert_eq!(
        content, "OPERATOR-EDIT",
        "unchanged default must not clobber"
    );
    assert_eq!(version, 9, "and must not bump the version");

    // ── Newer shipped default: refresh + bump, even over an operator edit ────
    managed::upsert_default(&repo(&bed), "skill", &name, "DEFAULT-V2")
        .await
        .unwrap();
    let (content, sha, version, default_sha) = row(&bed.pool, &name).await.unwrap();
    assert_eq!(content, "DEFAULT-V2", "a newer default wins");
    assert_eq!(version, 10, "and bumps the version monotonically");
    assert_eq!(sha, default_sha);

    bed.teardown().await;
}

#[tokio::test]
async fn seed_populates_the_real_managed_set() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping managed seed-real test — no DATABASE_URL");
        return;
    };
    managed::seed(&repo(&bed)).await.unwrap();

    // The nookos skill and the hook set are present with a sha and a version.
    let skill: Option<(String, String, i64)> = sqlx::query_as(
        "SELECT content, sha256, version FROM managed_content
         WHERE kind = 'skill' AND name = 'nookos'",
    )
    .fetch_optional(&bed.pool)
    .await
    .unwrap();
    let (content, sha, version) = skill.expect("the nookos skill is seeded");
    assert!(!content.is_empty());
    assert_eq!(sha.len(), 64);
    assert!(version >= 1);

    let hooks: Option<(String,)> = sqlx::query_as(
        "SELECT content FROM managed_content WHERE kind = 'hooks' AND name = 'default'",
    )
    .fetch_optional(&bed.pool)
    .await
    .unwrap();
    let hooks = hooks.expect("the hook set is seeded").0;
    let parsed: serde_json::Value = serde_json::from_str(&hooks).expect("hooks are JSON");
    assert!(
        parsed.get("Stop").is_some(),
        "the settings fragment has events"
    );

    // The stored skills express as the InstallSkill payload (AC-4).
    let payloads = managed::managed_skills_as_install(&repo(&bed))
        .await
        .unwrap();
    assert!(payloads.iter().any(
        |m| matches!(m, nook_proto::ControlToNode::InstallSkill { name, .. } if name == "nookos")
    ));

    bed.teardown().await;
}

// ── Read-endpoint permission gate (AC-3) ─────────────────────────────────────

/// Insert a tenant + user; return an authenticated user context.
async fn tenant_user(pool: &PgPool) -> (TenantId, UserId, AuthCtx) {
    let tenant = TenantId(Uuid::now_v7());
    sqlx::query("INSERT INTO tenants (id, name, slug) VALUES ($1, $2, $2)")
        .bind(tenant)
        .bind(format!("t-{}", tenant.0.simple()))
        .execute(pool)
        .await
        .unwrap();
    let user = UserId::new();
    sqlx::query(
        "INSERT INTO users (id, tenant_id, person_id, display_name, email)
         VALUES ($1, $2, gen_random_uuid(), 'Op', $3)",
    )
    .bind(user)
    .bind(tenant)
    .bind(format!("op-{}@example.test", user.0.simple()))
    .execute(pool)
    .await
    .unwrap();
    let ctx = AuthCtx {
        session_id: AuthSessionId::new(),
        user_id: user,
        tenant_id: tenant,
        principal: Principal::User,
        cookie_session: false,
    };
    (tenant, user, ctx)
}

#[tokio::test]
async fn read_endpoints_require_node_manage() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping managed perm test — no DATABASE_URL");
        return;
    };
    let state = bed.app_state().await;
    managed::seed(&repo(&bed)).await.unwrap();
    let (_tenant, user, ctx) = tenant_user(&bed.pool).await;

    // Without a grant: both reads are refused (not a 500, a forbidden). AuthCtx
    // is Copy, so it is passed by copy rather than cloned.
    let denied = managed::list_skills(axum::extract::State(state.clone()), ctx).await;
    assert!(denied.is_err(), "a plain user cannot read managed content");
    let denied_hooks = managed::get_hooks(axum::extract::State(state.clone()), ctx).await;
    assert!(denied_hooks.is_err());

    // Grant the caller `operator` at deployment scope — the role the seed gives
    // `node.manage`. `require` then resolves it exactly as `nodes::update` does.
    sqlx::query(
        "INSERT INTO role_bindings (id, subject_type, subject_id, role_key, scope_type, scope_id)
         VALUES (gen_random_uuid(), 'user', $1, 'operator', 'deployment', NULL)",
    )
    .bind(user.0)
    .execute(&bed.pool)
    .await
    .unwrap();

    let ok = managed::list_skills(axum::extract::State(state.clone()), ctx).await;
    assert!(ok.is_ok(), "an operator can read managed skills");
    let ok_hooks = managed::get_hooks(axum::extract::State(state.clone()), ctx).await;
    assert!(ok_hooks.is_ok(), "an operator can read managed hooks");

    bed.teardown().await;
}
