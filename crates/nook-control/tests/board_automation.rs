//! Board automation engine (MAIN-73) against a live Postgres. Set `DATABASE_URL`.
//!
//! Exercises the engine through the real code paths: it fires on a column change
//! only (a same-column move is silent), from both the `/move` service and the
//! board-drag PATCH handler, applies label add/remove effects, and on a failure
//! records an event + a system comment while the move itself still sticks.

use nook_control::auth::{AuthCtx, Principal};
use nook_control::config::Config;
use nook_control::routes::boards;
use nook_control::services::taskwork;
use nook_control::services::triggers;
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
        queue_provider: "database".into(),
        redis_url: None,
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
        trusted_proxies: Vec::new(),
    }
}

struct Fixture {
    tenant: TenantId,
    board: BoardId,
    todo: ColumnId,
    review: ColumnId,
    done: ColumnId,
}

/// A tenant + a local board with the standard column types, plus an automation
/// config on Done and In Review.
async fn fixture(db: &PgPool, automation: serde_json::Value) -> Fixture {
    let tenant = TenantId(Uuid::now_v7());
    sqlx::query("INSERT INTO tenants (id, name, slug) VALUES ($1, $2, $2)")
        .bind(tenant)
        .bind(format!("t-{}", tenant.0.simple()))
        .execute(db)
        .await
        .unwrap();
    let board = BoardId(Uuid::now_v7());
    sqlx::query(
        "INSERT INTO boards (id, tenant_id, name, key, provider, automation)
         VALUES ($1, $2, 'b', $3, 'local', $4)",
    )
    .bind(board)
    .bind(tenant)
    .bind(format!("B{}", &board.0.simple().to_string()[26..]).to_uppercase())
    .bind(&automation)
    .execute(db)
    .await
    .unwrap();

    let (todo, review, done) = (ColumnId::new(), ColumnId::new(), ColumnId::new());
    sqlx::query(
        "INSERT INTO board_columns (id, board_id, name, position, type) VALUES
           ($1,$4,'Todo',0,'unstarted'),
           ($2,$4,'In Review',1,'review'),
           ($3,$4,'Done',2,'completed')",
    )
    .bind(todo)
    .bind(review)
    .bind(done)
    .bind(board)
    .execute(db)
    .await
    .unwrap();

    Fixture {
        tenant,
        board,
        todo,
        review,
        done,
    }
}

async fn task_in(db: &PgPool, f: &Fixture, col: ColumnId, number: i32) -> TaskId {
    let id = TaskId::new();
    sqlx::query(
        "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, number)
         VALUES ($1,$2,$3,$4,$5,$6)",
    )
    .bind(id)
    .bind(f.tenant)
    .bind(f.board)
    .bind(col)
    .bind(format!("task {number}"))
    .bind(number)
    .execute(db)
    .await
    .unwrap();
    id
}

async fn labels_of(db: &PgPool, task: TaskId) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT l.name FROM task_labels tl JOIN labels l ON l.id = tl.label_id
         WHERE tl.task_id = $1 ORDER BY l.name",
    )
    .bind(task)
    .fetch_all(db)
    .await
    .unwrap()
}

async fn column_of(db: &PgPool, task: TaskId) -> ColumnId {
    sqlx::query_scalar("SELECT column_id FROM tasks WHERE id = $1")
        .bind(task)
        .fetch_one(db)
        .await
        .unwrap()
}

fn state_and(pool: &PgPool) -> impl std::future::Future<Output = AppState> + '_ {
    AppState::new(pool.clone(), test_config(), None)
}

#[tokio::test]
async fn move_fires_label_actions_and_same_column_is_silent() {
    let Some(pool) = test_pool().await else {
        eprintln!("skipping automation move test — no DATABASE_URL");
        return;
    };
    let state = state_and(&pool).await;
    // Done removes `agent-ready`; In Review adds `in-review`.
    let f = fixture(
        &pool,
        serde_json::json!({
            "completed": [{ "kind": "remove_board_label", "label": "agent-ready" }],
            "review": [{ "kind": "add_board_label", "label": "in-review" }],
        }),
    )
    .await;
    let task = task_in(&pool, &f, f.todo, 1).await;

    // Pre-attach agent-ready so the removal has something to remove. The label
    // must exist in the tenant vocabulary first.
    let label_id: Uuid = sqlx::query_scalar(
        "INSERT INTO labels (id, tenant_id, name, color) VALUES ($1,$2,'agent-ready','#f0a000')
         ON CONFLICT (tenant_id, name) DO UPDATE SET name = EXCLUDED.name RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(f.tenant)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO task_labels (task_id, label_id) VALUES ($1,$2)")
        .bind(task)
        .bind(label_id)
        .execute(&pool)
        .await
        .unwrap();

    // Todo → In Review via the /move path: `in-review` is added (created).
    taskwork::move_task(&state, f.tenant, task, "In Review")
        .await
        .unwrap();
    assert_eq!(
        labels_of(&pool, task).await,
        vec!["agent-ready".to_string(), "in-review".to_string()]
    );

    // In Review → In Review (same column): the engine no-ops — no duplicate work,
    // and nothing changes.
    let before = labels_of(&pool, task).await;
    taskwork::move_task(&state, f.tenant, task, "In Review")
        .await
        .unwrap();
    assert_eq!(
        labels_of(&pool, task).await,
        before,
        "same-column is silent"
    );

    // In Review → Done: `agent-ready` is stripped.
    taskwork::move_task(&state, f.tenant, task, "Done")
        .await
        .unwrap();
    assert_eq!(labels_of(&pool, task).await, vec!["in-review".to_string()]);

    cleanup(&pool, f.tenant).await;
}

#[tokio::test]
async fn drag_patch_path_also_fires() {
    let Some(pool) = test_pool().await else {
        eprintln!("skipping automation PATCH test — no DATABASE_URL");
        return;
    };
    let state = state_and(&pool).await;
    let f = fixture(
        &pool,
        serde_json::json!({ "review": [{ "kind": "add_board_label", "label": "in-review" }] }),
    )
    .await;
    let task = task_in(&pool, &f, f.todo, 1).await;
    let user = user(&pool, f.tenant).await;

    // The board-drag path: PATCH /tasks/{id} with a new column_id.
    let req: UpdateTaskRequest =
        serde_json::from_value(serde_json::json!({ "column_id": f.review })).unwrap();
    let ctx = AuthCtx {
        session_id: AuthSessionId::new(),
        user_id: user,
        tenant_id: f.tenant,
        principal: Principal::User,
        cookie_session: false,
    };
    let _ = boards::update_task(
        axum::extract::State(state.clone()),
        ctx,
        axum::extract::Path(task.to_string()),
        axum::Json(req),
    )
    .await
    .expect("drag move");

    assert_eq!(labels_of(&pool, task).await, vec!["in-review".to_string()]);
    cleanup(&pool, f.tenant).await;
}

#[tokio::test]
async fn a_failed_action_records_event_and_comment_but_the_move_sticks() {
    let Some(pool) = test_pool().await else {
        eprintln!("skipping automation failure test — no DATABASE_URL");
        return;
    };
    let state = state_and(&pool).await;
    // A blank label is invalid at runtime — validation is bypassed by writing the
    // config straight to the column (simulating a config that slipped through).
    let f = fixture(
        &pool,
        serde_json::json!({ "completed": [{ "kind": "add_board_label", "label": "   " }] }),
    )
    .await;
    let task = task_in(&pool, &f, f.todo, 1).await;

    taskwork::move_task(&state, f.tenant, task, "Done")
        .await
        .expect("the move itself succeeds even though the action fails");

    // The move stuck.
    assert_eq!(column_of(&pool, task).await, f.done);

    // A task.automation_failed event was recorded.
    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM events
         WHERE tenant_id = $1 AND kind = 'task.automation_failed'
           AND payload->>'task_id' = $2",
    )
    .bind(f.tenant)
    .bind(task.0.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(events, 1, "a failure event is recorded");

    // A system-authored comment names the action and error.
    let comment: Option<(String, String)> = sqlx::query_as(
        "SELECT author_type, body_md FROM task_comments
         WHERE task_id = $1 AND author_type = 'system'",
    )
    .bind(task)
    .fetch_optional(&pool)
    .await
    .unwrap();
    let (author, body) = comment.expect("a system comment");
    assert_eq!(author, "system");
    assert!(body.contains("add_board_label"), "names the action: {body}");

    cleanup(&pool, f.tenant).await;
}

#[tokio::test]
async fn notify_action_raises_a_notification_with_a_deep_link() {
    let Some(pool) = test_pool().await else {
        eprintln!("skipping automation notify test — no DATABASE_URL");
        return;
    };
    let state = state_and(&pool).await;
    let f = fixture(
        &pool,
        serde_json::json!({ "review": [{ "kind": "notify", "title": "{key} needs review" }] }),
    )
    .await;
    let task = task_in(&pool, &f, f.todo, 7).await;

    // Fire the engine directly for the review column.
    triggers::on_column_change(&state, f.tenant, task, f.board, f.todo, f.review).await;

    let notif: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT title, link FROM notifications
         WHERE tenant_id = $1 AND kind = 'board.automation'
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(f.tenant)
    .fetch_optional(&pool)
    .await
    .unwrap();
    let (title, link) = notif.expect("a notification was raised");
    assert!(title.ends_with("needs review"), "token expanded: {title}");
    assert!(
        link.unwrap_or_default()
            .contains(&format!("/board?task={task}")),
        "deep link to the task"
    );

    cleanup(&pool, f.tenant).await;
}

#[tokio::test]
async fn notify_is_skipped_for_a_private_card_but_labels_still_apply() {
    // MAIN-76 leak guard (review of #60): the notify action is a tenant-wide
    // broadcast (toast + phone + channels), so it must NOT fire for a private
    // card — its title (via `{title}` or the default body) would reach the whole
    // tenant. Label actions broadcast nothing, so they still apply on a private
    // card.
    let Some(pool) = test_pool().await else {
        eprintln!("skipping private-card notify test — no DATABASE_URL");
        return;
    };
    let state = state_and(&pool).await;
    let f = fixture(
        &pool,
        serde_json::json!({
            "review": [
                { "kind": "notify", "title": "{title}" },
                { "kind": "add_board_label", "label": "in-review" },
            ]
        }),
    )
    .await;
    let task = task_in(&pool, &f, f.todo, 1).await;
    sqlx::query(
        "UPDATE tasks SET visibility = 'private', title = 'SUPER SECRET TITLE' WHERE id = $1",
    )
    .bind(task)
    .execute(&pool)
    .await
    .unwrap();

    triggers::on_column_change(&state, f.tenant, task, f.board, f.todo, f.review).await;

    // No tenant-wide automation notification was raised at all…
    let notifs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM notifications WHERE tenant_id = $1 AND kind = 'board.automation'",
    )
    .bind(f.tenant)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(notifs, 0, "no tenant-wide notify fires for a private card");
    // …and the private title never reached any notification field.
    let leaked: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM notifications
         WHERE tenant_id = $1 AND (title LIKE '%SUPER SECRET%' OR body LIKE '%SUPER SECRET%')",
    )
    .bind(f.tenant)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        leaked, 0,
        "the private title must not leak into a notification"
    );

    // The label action still applied — it does not broadcast, so it is not gated.
    assert!(
        labels_of(&pool, task)
            .await
            .contains(&"in-review".to_string()),
        "a non-broadcasting action still runs on a private card"
    );

    cleanup(&pool, f.tenant).await;
}

#[tokio::test]
async fn patch_board_rejects_a_bogus_automation_config() {
    let Some(pool) = test_pool().await else {
        eprintln!("skipping automation validation test — no DATABASE_URL");
        return;
    };
    let state = state_and(&pool).await;
    let f = fixture(&pool, serde_json::json!({})).await;
    let user = user(&pool, f.tenant).await;
    let ctx = AuthCtx {
        session_id: AuthSessionId::new(),
        user_id: user,
        tenant_id: f.tenant,
        principal: Principal::User,
        cookie_session: false,
    };

    let req: UpdateBoardRequest = serde_json::from_value(serde_json::json!({
        "name": "b",
        "automation": { "review": [{ "kind": "merge_pr" }] }
    }))
    .unwrap();
    let out = boards::update_board(
        axum::extract::State(state),
        ctx,
        axum::extract::Path(f.board),
        axum::Json(req),
    )
    .await;
    assert!(out.is_err(), "an unknown action kind is a 400");

    cleanup(&pool, f.tenant).await;
}

async fn user(pool: &PgPool, tenant: TenantId) -> UserId {
    let user = UserId::new();
    sqlx::query(
        "INSERT INTO users (id, tenant_id, person_id, display_name, email)
         VALUES ($1, $2, gen_random_uuid(), 'U', $3)",
    )
    .bind(user)
    .bind(tenant)
    .bind(format!("u-{}@example.test", user.0.simple()))
    .execute(pool)
    .await
    .unwrap();
    user
}

async fn cleanup(pool: &PgPool, tenant: TenantId) {
    let _ = sqlx::query("DELETE FROM tenants WHERE id = $1 AND slug <> 'dev'")
        .bind(tenant)
        .execute(pool)
        .await;
}
