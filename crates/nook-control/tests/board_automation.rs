//! Board automation engine (MAIN-73) against a live Postgres. Set `DATABASE_URL`.
//!
//! Exercises the engine through the real code paths: it fires on a column change
//! only (a same-column move is silent), from both the `/move` service and the
//! board-drag PATCH handler, applies label add/remove effects, and on a failure
//! records an event + a system comment while the move itself still sticks.
//!
//! Setup + teardown run through `nook_testkit::TestBed` (MAIN-156).

use nook_control::auth::{AuthCtx, Principal};
use nook_control::routes::boards;
use nook_control::services::taskwork;
use nook_control::services::triggers;
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

struct Fixture {
    tenant: TenantId,
    board: BoardId,
    todo: ColumnId,
    review: ColumnId,
    done: ColumnId,
}

/// A tracked tenant + a local board with the standard column types, plus an
/// automation config on Done and In Review.
async fn fixture(bed: &mut TestBed, automation: serde_json::Value) -> Fixture {
    let tenant = bed.tenant("ba").await;
    let board = BoardId(Uuid::now_v7());
    bed.db()
        .exec(
            "INSERT INTO boards (id, tenant_id, name, key, provider, automation)
         VALUES ($1, $2, 'b', $3, 'local', $4)",
            params![
                board,
                tenant,
                format!("B{}", &board.0.simple().to_string()[26..]).to_uppercase(),
                automation.clone()
            ],
        )
        .await
        .unwrap();

    let (todo, review, done) = (ColumnId::new(), ColumnId::new(), ColumnId::new());
    bed.db()
        .exec(
            "INSERT INTO board_columns (id, board_id, name, position, type) VALUES
           ($1,$4,'Todo',0,'unstarted'),
           ($2,$4,'In Review',1,'review'),
           ($3,$4,'Done',2,'completed')",
            params![todo, review, done, board],
        )
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

async fn task_in(bed: &TestBed, f: &Fixture, col: ColumnId, number: i32) -> TaskId {
    let id = TaskId::new();
    bed.db()
        .exec(
            "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, number)
         VALUES ($1,$2,$3,$4,$5,$6)",
            params![id, f.tenant, f.board, col, format!("task {number}"), number],
        )
        .await
        .unwrap();
    id
}

async fn labels_of(bed: &TestBed, task: TaskId) -> Vec<String> {
    bed.db()
        .query_scalar_all(
            "SELECT l.name FROM task_labels tl JOIN labels l ON l.id = tl.label_id
         WHERE tl.task_id = $1 ORDER BY l.name",
            params![task],
        )
        .await
        .unwrap()
}

async fn column_of(bed: &TestBed, task: TaskId) -> ColumnId {
    bed.db()
        .query_scalar("SELECT column_id FROM tasks WHERE id = $1", params![task])
        .await
        .unwrap()
}

async fn a_user(bed: &TestBed, tenant: TenantId) -> UserId {
    let user = UserId::new();
    bed.db()
        .exec(
            "INSERT INTO users (id, tenant_id, person_id, display_name, email)
         VALUES ($1, $2, gen_random_uuid(), 'U', $3)",
            params![user, tenant, format!("u-{}@example.test", user.0.simple())],
        )
        .await
        .unwrap();
    user
}

#[tokio::test]
async fn move_fires_label_actions_and_same_column_is_silent() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping automation move test — no DATABASE_URL");
        return;
    };
    let state = bed.app_state().await;
    // Done removes `agent-ready`; In Review adds `in-review`.
    let f = fixture(
        &mut bed,
        serde_json::json!({
            "completed": [{ "kind": "remove_board_label", "label": "agent-ready" }],
            "review": [{ "kind": "add_board_label", "label": "in-review" }],
        }),
    )
    .await;
    let task = task_in(&bed, &f, f.todo, 1).await;

    // Pre-attach agent-ready so the removal has something to remove. The label
    // must exist in the tenant vocabulary first.
    let label_id: Uuid = bed
        .db()
        .query_scalar(
            "INSERT INTO labels (id, tenant_id, name, color) VALUES ($1,$2,'agent-ready','#f0a000')
         ON CONFLICT (tenant_id, name) DO UPDATE SET name = EXCLUDED.name RETURNING id",
            params![Uuid::now_v7(), f.tenant],
        )
        .await
        .unwrap();
    bed.db()
        .exec(
            "INSERT INTO task_labels (task_id, label_id) VALUES ($1,$2)",
            params![task, label_id],
        )
        .await
        .unwrap();

    // Todo → In Review via the /move path: `in-review` is added (created).
    taskwork::move_task(&state, f.tenant, task, "In Review")
        .await
        .unwrap();
    assert_eq!(
        labels_of(&bed, task).await,
        vec!["agent-ready".to_string(), "in-review".to_string()]
    );

    // In Review → In Review (same column): the engine no-ops — no duplicate work,
    // and nothing changes.
    let before = labels_of(&bed, task).await;
    taskwork::move_task(&state, f.tenant, task, "In Review")
        .await
        .unwrap();
    assert_eq!(labels_of(&bed, task).await, before, "same-column is silent");

    // In Review → Done: `agent-ready` is stripped.
    taskwork::move_task(&state, f.tenant, task, "Done")
        .await
        .unwrap();
    assert_eq!(labels_of(&bed, task).await, vec!["in-review".to_string()]);

    bed.teardown().await;
}

#[tokio::test]
async fn drag_patch_path_also_fires() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping automation PATCH test — no DATABASE_URL");
        return;
    };
    let state = bed.app_state().await;
    let f = fixture(
        &mut bed,
        serde_json::json!({ "review": [{ "kind": "add_board_label", "label": "in-review" }] }),
    )
    .await;
    let task = task_in(&bed, &f, f.todo, 1).await;
    let user = a_user(&bed, f.tenant).await;

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

    assert_eq!(labels_of(&bed, task).await, vec!["in-review".to_string()]);
    bed.teardown().await;
}

#[tokio::test]
async fn a_failed_action_records_event_and_comment_but_the_move_sticks() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping automation failure test — no DATABASE_URL");
        return;
    };
    let state = bed.app_state().await;
    // A blank label is invalid at runtime — validation is bypassed by writing the
    // config straight to the column (simulating a config that slipped through).
    let f = fixture(
        &mut bed,
        serde_json::json!({ "completed": [{ "kind": "add_board_label", "label": "   " }] }),
    )
    .await;
    let task = task_in(&bed, &f, f.todo, 1).await;

    taskwork::move_task(&state, f.tenant, task, "Done")
        .await
        .expect("the move itself succeeds even though the action fails");

    // The move stuck.
    assert_eq!(column_of(&bed, task).await, f.done);

    // A task.automation_failed event was recorded.
    let events: i64 = bed
        .db()
        .query_scalar(
            "SELECT count(*) FROM events
         WHERE tenant_id = $1 AND kind = 'task.automation_failed'
           AND payload->>'task_id' = $2",
            params![f.tenant, task.0.to_string()],
        )
        .await
        .unwrap();
    assert_eq!(events, 1, "a failure event is recorded");

    // A system-authored comment names the action and error.
    let comment: Option<(String, String)> = bed
        .db()
        .query_opt(
            "SELECT author_type, body_md FROM task_comments
         WHERE task_id = $1 AND author_type = 'system'",
            params![task],
        )
        .await
        .unwrap();
    let (author, body) = comment.expect("a system comment");
    assert_eq!(author, "system");
    assert!(body.contains("add_board_label"), "names the action: {body}");

    bed.teardown().await;
}

#[tokio::test]
async fn notify_action_raises_a_notification_with_a_deep_link() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping automation notify test — no DATABASE_URL");
        return;
    };
    let state = bed.app_state().await;
    let f = fixture(
        &mut bed,
        serde_json::json!({ "review": [{ "kind": "notify", "title": "{key} needs review" }] }),
    )
    .await;
    let task = task_in(&bed, &f, f.todo, 7).await;

    // Fire the engine directly for the review column.
    triggers::on_column_change(&state, f.tenant, task, f.board, f.todo, f.review).await;

    let notif: Option<(String, Option<String>)> = bed
        .db()
        .query_opt(
            "SELECT title, link FROM notifications
         WHERE tenant_id = $1 AND kind = 'board.automation'
         ORDER BY created_at DESC LIMIT 1",
            params![f.tenant],
        )
        .await
        .unwrap();
    let (title, link) = notif.expect("a notification was raised");
    assert!(title.ends_with("needs review"), "token expanded: {title}");
    assert!(
        link.unwrap_or_default()
            .contains(&format!("/board?task={task}")),
        "deep link to the task"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn notify_is_skipped_for_a_private_card_but_labels_still_apply() {
    // MAIN-76 leak guard (review of #60): the notify action is a tenant-wide
    // broadcast (toast + phone + channels), so it must NOT fire for a private
    // card — its title (via `{title}` or the default body) would reach the whole
    // tenant. Label actions broadcast nothing, so they still apply on a private
    // card.
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping private-card notify test — no DATABASE_URL");
        return;
    };
    let state = bed.app_state().await;
    let f = fixture(
        &mut bed,
        serde_json::json!({
            "review": [
                { "kind": "notify", "title": "{title}" },
                { "kind": "add_board_label", "label": "in-review" },
            ]
        }),
    )
    .await;
    let task = task_in(&bed, &f, f.todo, 1).await;
    bed.db()
        .exec(
            "UPDATE tasks SET visibility = 'private', title = 'SUPER SECRET TITLE' WHERE id = $1",
            params![task],
        )
        .await
        .unwrap();

    triggers::on_column_change(&state, f.tenant, task, f.board, f.todo, f.review).await;

    // No tenant-wide automation notification was raised at all…
    let notifs: i64 = bed
        .db()
        .query_scalar(
            "SELECT count(*) FROM notifications WHERE tenant_id = $1 AND kind = 'board.automation'",
            params![f.tenant],
        )
        .await
        .unwrap();
    assert_eq!(notifs, 0, "no tenant-wide notify fires for a private card");
    // …and the private title never reached any notification field.
    let leaked: i64 = bed
        .db()
        .query_scalar(
            "SELECT count(*) FROM notifications
         WHERE tenant_id = $1 AND (title LIKE '%SUPER SECRET%' OR body LIKE '%SUPER SECRET%')",
            params![f.tenant],
        )
        .await
        .unwrap();
    assert_eq!(
        leaked, 0,
        "the private title must not leak into a notification"
    );

    // The label action still applied — it does not broadcast, so it is not gated.
    assert!(
        labels_of(&bed, task)
            .await
            .contains(&"in-review".to_string()),
        "a non-broadcasting action still runs on a private card"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn patch_board_rejects_a_bogus_automation_config() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping automation validation test — no DATABASE_URL");
        return;
    };
    let state = bed.app_state().await;
    let f = fixture(&mut bed, serde_json::json!({})).await;
    let user = a_user(&bed, f.tenant).await;
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

    bed.teardown().await;
}
