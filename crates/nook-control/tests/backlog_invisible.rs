//! Backlog + epics are invisible to the loop (MAIN-80): the pick query excludes
//! backlog-column and epic tasks by default, and `claim_inner` refuses them with
//! distinct 400s — all through the real code paths against a live Postgres. Set
//! `DATABASE_URL`.

use nook_control::config::Config;
use nook_control::error::ApiError;
use nook_control::routes::task_query::{claim_inner, query_rows, TaskFilter};
use nook_control::state::AppState;
use nook_types::*;
use sqlx::PgPool;
use uuid::Uuid;

mod common;
use common::test_pool;

fn test_config() -> Config {
    Config {
        app_env: "test".into(),
        bind: "127.0.0.1:0".into(),
        shutdown_grace_secs: 25,
        public_base_url: "http://localhost:8080".into(),
        web_origin: "http://localhost:5173".into(),
        database_url: std::env::var("DATABASE_URL").unwrap_or_default(),
        oidc_issuer_url: None,
        oidc_client_id: None,
        oidc_device_client_id: None,
        oidc_device_authorization_endpoint: None,
        oidc_client_secret: None,
        oidc_redirect_url: None,
        oidc_scopes: "openid profile email".into(),
        session_secret: "0".repeat(64),
        session_ttl_hours: 168,
        default_tenant_name: format!("test-{}", Uuid::now_v7().simple()),
        auth_dev_mode: true,
        mcp_token: None,
        dev_join_token: None,
        dist_dir: "/nonexistent".into(),
        agent_bind: "127.0.0.1:0".into(),
        agent_public_url: None,
        agent_tls_cert: None,
        agent_tls_key: None,
        releases_repo: "nook-os/nook-os".into(),
        artifact_store: "disk".into(),
        artifact_prefix: "nook".into(),
        artifact_redirect: false,
        s3_bucket: None,
        s3_endpoint: None,
        s3_region: None,
        s3_access_key_id: None,
        s3_secret_access_key: None,
        s3_path_style: true,
        cache_provider: "memory".into(),
        mail_provider: "capture".into(),
        smtp_host: None,
        smtp_port: 587,
        smtp_tls: "starttls".into(),
        smtp_from: "NookOS <no-reply@localhost>".into(),
        smtp_username: None,
        smtp_password: None,
        postmark_token: None,
        postmark_api_url: "https://api.postmarkapp.com/email".into(),
        mail_from: "NookOS <no-reply@localhost>".into(),
        mail_send_enabled: false,
        mail_notifications_enabled: false,
        mail_max_per_month: Some(100),
        mail_max_per_day: None,
    }
}

/// A tenant + board with a `backlog` column and an `unstarted` column.
async fn fixture(db: &PgPool) -> (TenantId, BoardId, ColumnId, ColumnId) {
    let tenant = TenantId(Uuid::now_v7());
    sqlx::query("INSERT INTO tenants (id, name, slug) VALUES ($1, $2, $2)")
        .bind(tenant)
        .bind(format!("t-{}", tenant.0.simple()))
        .execute(db)
        .await
        .expect("tenant");
    let board = BoardId(Uuid::now_v7());
    sqlx::query(
        "INSERT INTO boards (id, tenant_id, name, key, provider) VALUES ($1,$2,'b',$3,'local')",
    )
    .bind(board)
    .bind(tenant)
    // The random tail, not the shared v7 timestamp prefix, so keys don't collide.
    .bind(format!("B{}", &board.0.simple().to_string()[26..]).to_uppercase())
    .execute(db)
    .await
    .expect("board");
    let backlog = ColumnId(Uuid::now_v7());
    let todo = ColumnId(Uuid::now_v7());
    sqlx::query(
        "INSERT INTO board_columns (id, board_id, name, position, type)
         VALUES ($1,$2,'Triage',0,'backlog'), ($3,$2,'Todo',1,'unstarted')",
    )
    .bind(backlog)
    .bind(board)
    .bind(todo)
    .execute(db)
    .await
    .expect("columns");
    (tenant, board, backlog, todo)
}

/// Insert a task in a column with a type + number, returning its id.
async fn task(
    db: &PgPool,
    tenant: TenantId,
    board: BoardId,
    col: ColumnId,
    number: i32,
    type_: &str,
) -> TaskId {
    let id = TaskId::new();
    sqlx::query(
        "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, number, type)
         VALUES ($1,$2,$3,$4,$5,$6,$7)",
    )
    .bind(id)
    .bind(tenant)
    .bind(board)
    .bind(col)
    .bind(format!("task {number}"))
    .bind(number)
    .bind(type_)
    .execute(db)
    .await
    .expect("task");
    id
}

async fn pick_ids(db: &PgPool, tenant: TenantId, f: &TaskFilter) -> Vec<TaskId> {
    // These fixtures set no visibility (default non-private), so any viewer sees
    // them; the pick's MAIN-76 predicate is exercised by task_visibility.rs.
    query_rows(db, tenant, UserId::new(), f)
        .await
        .expect("pick")
        .into_iter()
        .map(|t| t.id)
        .collect()
}

#[tokio::test]
async fn pick_excludes_backlog_and_epics_by_default() {
    let Some(pool) = test_pool().await else {
        eprintln!("skipping backlog test — no DATABASE_URL");
        return;
    };
    let (tenant, board, backlog, todo) = fixture(&pool).await;

    // A normal task on the board, a normal task in the backlog, and an epic on
    // the board.
    let normal = task(&pool, tenant, board, todo, 1, "task").await;
    let in_backlog = task(&pool, tenant, board, backlog, 2, "task").await;
    let epic = task(&pool, tenant, board, todo, 3, "epic").await;

    let base = TaskFilter {
        board: Some(board.to_string()),
        limit: Some(200),
        ..Default::default()
    };

    // ── Default: backlog + epic excluded (AC-1/AC-2) ────────────────────────
    let def = pick_ids(&pool, tenant, &base).await;
    assert!(def.contains(&normal), "a board task is picked");
    assert!(
        !def.contains(&in_backlog),
        "a backlog task is NOT in the default pick"
    );
    assert!(
        !def.contains(&epic),
        "an epic is NOT in the default pick, regardless of column"
    );

    // ── backlog=true includes backlog tasks (AC-1) ──────────────────────────
    let with_backlog = pick_ids(
        &pool,
        tenant,
        &TaskFilter {
            backlog: Some(true),
            ..base.clone()
        },
    )
    .await;
    assert!(
        with_backlog.contains(&in_backlog),
        "backlog=true reveals it"
    );
    assert!(
        !with_backlog.contains(&epic),
        "backlog=true does not un-hide an epic — that is a separate exclusion"
    );

    // ── type=epic surfaces epics on purpose (AC-2) ──────────────────────────
    let with_epic = pick_ids(
        &pool,
        tenant,
        &TaskFilter {
            type_: vec!["epic".into()],
            ..base.clone()
        },
    )
    .await;
    assert!(
        with_epic.contains(&epic),
        "an explicit type=epic filter shows epics"
    );
    assert!(
        !with_epic.contains(&normal),
        "and only epics — the type filter still restricts"
    );

    let _ = sqlx::query("DELETE FROM tenants WHERE id = $1 AND slug <> 'dev'")
        .bind(tenant)
        .execute(&pool)
        .await;
}

#[tokio::test]
async fn parent_filter_lifts_the_backlog_exclusion() {
    // MAIN-80 AC-3, now live because the epics-backend `parent` filter (MAIN-81)
    // has merged: an epic's tickets span triage and the board, so a `parent=`
    // query must return the children still in the backlog rather than silently
    // dropping them. Without the parent filter, those same tickets stay hidden.
    let Some(pool) = test_pool().await else {
        eprintln!("skipping parent-lift test — no DATABASE_URL");
        return;
    };
    let (tenant, board, backlog, todo) = fixture(&pool).await;

    let epic = task(&pool, tenant, board, todo, 1, "epic").await;
    let child_backlog = task(&pool, tenant, board, backlog, 2, "task").await;
    let child_board = task(&pool, tenant, board, todo, 3, "task").await;
    for child in [child_backlog, child_board] {
        sqlx::query("UPDATE tasks SET parent_task_id = $1 WHERE id = $2")
            .bind(epic)
            .bind(child)
            .execute(&pool)
            .await
            .expect("set parent");
    }

    let base = TaskFilter {
        board: Some(board.to_string()),
        limit: Some(200),
        ..Default::default()
    };

    // ?parent=<epic> returns BOTH children — the backlog exclusion is lifted.
    let children = pick_ids(
        &pool,
        tenant,
        &TaskFilter {
            parent: Some(epic.to_string()),
            ..base.clone()
        },
    )
    .await;
    assert!(
        children.contains(&child_backlog),
        "a parent= query returns the child still in the backlog (AC-3 lift)"
    );
    assert!(
        children.contains(&child_board),
        "and the child on the board"
    );

    // Without a parent filter the backlog child stays excluded by default.
    let default = pick_ids(&pool, tenant, &base).await;
    assert!(
        !default.contains(&child_backlog),
        "the backlog child is still hidden without parent="
    );

    let _ = sqlx::query("DELETE FROM tenants WHERE id = $1 AND slug <> 'dev'")
        .bind(tenant)
        .execute(&pool)
        .await;
}

#[tokio::test]
async fn claim_refuses_backlog_and_epic_with_distinct_messages() {
    let Some(pool) = test_pool().await else {
        eprintln!("skipping claim-refusal test — no DATABASE_URL");
        return;
    };
    let state = AppState::new(pool.clone(), test_config(), None).await;
    let (tenant, board, backlog, todo) = fixture(&pool).await;
    // A real user row — claiming sets assignee_user_id, which has an FK.
    let claimant = UserId::new();
    sqlx::query(
        "INSERT INTO users (id, tenant_id, person_id, display_name, email)
         VALUES ($1, $2, gen_random_uuid(), 'Claimant', $3)",
    )
    .bind(claimant)
    .bind(tenant)
    .bind(format!("claimant-{}@example.test", claimant.0.simple()))
    .execute(&pool)
    .await
    .expect("claimant user");

    let in_backlog = task(&pool, tenant, board, backlog, 1, "task").await;
    let epic = task(&pool, tenant, board, todo, 2, "epic").await;
    let normal = task(&pool, tenant, board, todo, 3, "task").await;

    // Backlog: a 400 naming the backlog, NOT the 409 lost-claim message (AC-4).
    let backlog_err = claim_inner(&state, tenant, claimant, &in_backlog.to_string(), None)
        .await
        .expect_err("backlog task is not claimable");
    match backlog_err {
        ApiError::BadRequest(m) => assert!(
            m.contains("backlog"),
            "backlog claim refused with the backlog message, got {m:?}"
        ),
        other => panic!("expected a 400 BadRequest, got {other:?}"),
    }

    // Epic: a distinct 400 about containers.
    let epic_err = claim_inner(&state, tenant, claimant, &epic.to_string(), None)
        .await
        .expect_err("an epic is not claimable");
    match epic_err {
        ApiError::BadRequest(m) => assert!(
            m.contains("epic") || m.contains("container"),
            "epic claim refused with the epic message, got {m:?}"
        ),
        other => panic!("expected a 400 BadRequest, got {other:?}"),
    }

    // A normal board task still claims fine — the guard is narrow.
    claim_inner(&state, tenant, claimant, &normal.to_string(), None)
        .await
        .expect("a normal board task claims");

    // resolve_id still finds a backlog task by key (AC-8): it stays readable /
    // commentable / labelable even though it is unpickable and unclaimable.
    let key: String = sqlx::query_scalar(
        "SELECT b.key || '-' || t.number::text FROM tasks t
         JOIN boards b ON b.id = t.board_id WHERE t.id = $1",
    )
    .bind(in_backlog)
    .fetch_one(&pool)
    .await
    .expect("key");
    let resolved = nook_control::services::tasks::resolve_id(&pool, tenant, &key)
        .await
        .expect("backlog task resolves by key");
    assert_eq!(
        resolved, in_backlog,
        "the backlog task is still addressable"
    );

    let _ = sqlx::query("DELETE FROM tenants WHERE id = $1 AND slug <> 'dev'")
        .bind(tenant)
        .execute(&pool)
        .await;
}
