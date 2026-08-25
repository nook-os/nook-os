//! `POST /tasks/{id}/move` naming its destination by column TYPE (MAIN-138).
//!
//! The type form is what a skill writes, because it survives a board that
//! renamed its columns — so what is pinned here is that it resolves against
//! the task's OWN board, that the name form is untouched beside it, and that
//! the two together (or neither) are REFUSED rather than resolved by some
//! precedence rule: a silent winner is how a card lands in the wrong column.
//!
//! Every row is one this test created, in its own `TestBed` database.

use axum::response::IntoResponse;
use nook_control::auth::{AuthCtx, Principal};
use nook_control::routes::taskwork as routes;
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

struct Fixture {
    tenant: TenantId,
    board: BoardId,
    todo: ColumnId,
    started: ColumnId,
    review: ColumnId,
    done: ColumnId,
}

/// A board whose `started` column is NOT called "In Progress" — the whole point
/// of moving by type is that a renamed column still receives the move, and a
/// fixture using the stock names could not tell the two paths apart.
async fn fixture(bed: &mut TestBed) -> Fixture {
    let tenant = bed.tenant("mbt").await;
    let board = BoardId(Uuid::now_v7());
    bed.db()
        .exec(
            "INSERT INTO boards (id, tenant_id, name, key, provider)
         VALUES ($1, $2, 'b', $3, 'local')",
            params![
                board,
                tenant,
                format!("B{}", &board.0.simple().to_string()[26..]).to_uppercase()
            ],
        )
        .await
        .unwrap();

    let (todo, started, review, done) = (
        ColumnId::new(),
        ColumnId::new(),
        ColumnId::new(),
        ColumnId::new(),
    );
    bed.db()
        .exec(
            "INSERT INTO board_columns (id, board_id, name, position, type) VALUES
           ($1,$5,'Todo',0,'unstarted'),
           ($2,$5,'Cooking',1,'started'),
           ($3,$5,'In Review',2,'review'),
           ($4,$5,'Done',3,'completed')",
            params![todo, started, review, done, board],
        )
        .await
        .unwrap();

    Fixture {
        tenant,
        board,
        todo,
        started,
        review,
        done,
    }
}

async fn task_in(bed: &TestBed, f: &Fixture, col: ColumnId) -> TaskId {
    let id = TaskId::new();
    bed.db()
        .exec(
            "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, number)
         VALUES ($1,$2,$3,$4,'move me',1)",
            params![id, f.tenant, f.board, col],
        )
        .await
        .unwrap();
    id
}

async fn column_of(bed: &TestBed, task: TaskId) -> ColumnId {
    bed.db()
        .query_scalar("SELECT column_id FROM tasks WHERE id = $1", params![task])
        .await
        .unwrap()
}

fn ctx(tenant: TenantId, user: UserId) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId::new(),
        user_id: user,
        tenant_id: tenant,
        principal: Principal::User,
        cookie_session: false,
    }
}

async fn a_user(bed: &TestBed, tenant: TenantId) -> UserId {
    let user = UserId::new();
    bed.db()
        .exec(
            // The person id is BOUND rather than `gen_random_uuid()`: that
            // function is Postgres-only and this test also runs on SQLite
            // (MAIN-435).
            "INSERT INTO users (id, tenant_id, person_id, display_name, email)
         VALUES ($1, $2, $4, 'U', $3)",
            params![
                user,
                tenant,
                format!("u-{}@example.test", user.0.simple()),
                Uuid::now_v7()
            ],
        )
        .await
        .unwrap();
    user
}

async fn move_with(
    state: &nook_control::state::AppState,
    tenant: TenantId,
    user: UserId,
    task: TaskId,
    body: serde_json::Value,
) -> Result<(), (axum::http::StatusCode, String)> {
    let req: MoveTaskRequest = serde_json::from_value(body).expect("a well-formed body");
    match routes::move_task(
        axum::extract::State(state.clone()),
        ctx(tenant, user),
        axum::extract::Path(task.to_string()),
        axum::Json(req),
    )
    .await
    {
        Ok(_) => Ok(()),
        Err(e) => {
            let res = e.into_response();
            let status = res.status();
            let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024)
                .await
                .unwrap();
            Err((status, String::from_utf8(bytes.to_vec()).unwrap()))
        }
    }
}

#[tokio::test]
async fn column_type_lands_the_card_and_the_name_form_still_works() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping move-by-type test — no DATABASE_URL");
        return;
    };
    let state = bed.app_state().await;
    let f = fixture(&mut bed).await;
    let user = a_user(&bed, f.tenant).await;
    let task = task_in(&bed, &f, f.todo).await;

    // `started` resolves to "Cooking", which no name-based caller would find.
    move_with(
        &state,
        f.tenant,
        user,
        task,
        serde_json::json!({ "column_type": "started" }),
    )
    .await
    .expect("move by type");
    assert_eq!(column_of(&bed, task).await, f.started);

    move_with(
        &state,
        f.tenant,
        user,
        task,
        serde_json::json!({ "column_type": "completed" }),
    )
    .await
    .expect("move by type");
    assert_eq!(column_of(&bed, task).await, f.done);

    // The pre-existing `{"column": "<name>"}` caller is unchanged (AC-2).
    move_with(
        &state,
        f.tenant,
        user,
        task,
        serde_json::json!({ "column": "In Review" }),
    )
    .await
    .expect("move by name");
    assert_eq!(column_of(&bed, task).await, f.review);

    bed.teardown().await;
}

#[tokio::test]
async fn both_or_neither_is_a_422_naming_the_rule() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping move-by-type rejection test — no DATABASE_URL");
        return;
    };
    let state = bed.app_state().await;
    let f = fixture(&mut bed).await;
    let user = a_user(&bed, f.tenant).await;
    let task = task_in(&bed, &f, f.todo).await;

    for body in [
        serde_json::json!({ "column": "Done", "column_type": "completed" }),
        serde_json::json!({}),
    ] {
        let (status, text) = move_with(&state, f.tenant, user, task, body.clone())
            .await
            .expect_err("refused");
        assert_eq!(
            status,
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            "{body} → {text}"
        );
        assert!(
            text.contains("exactly one") && text.contains("column_type"),
            "the message names the rule: {text}"
        );
        // Refused means refused: nothing moved.
        assert_eq!(column_of(&bed, task).await, f.todo);
    }

    bed.teardown().await;
}

/// A type the board has no column for is a 409 that names it, not a guess at a
/// position — the caller can add the column or pick another type, and neither
/// is possible from a card that silently went somewhere.
#[tokio::test]
async fn a_type_the_board_has_no_column_for_is_refused() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping missing-column test — no DATABASE_URL");
        return;
    };
    let state = bed.app_state().await;
    let f = fixture(&mut bed).await;
    let user = a_user(&bed, f.tenant).await;
    let task = task_in(&bed, &f, f.todo).await;

    let (status, text) = move_with(
        &state,
        f.tenant,
        user,
        task,
        serde_json::json!({ "column_type": "canceled" }),
    )
    .await
    .expect_err("refused");
    assert_eq!(status, axum::http::StatusCode::CONFLICT, "{text}");
    assert!(text.contains("canceled"), "names the missing type: {text}");
    assert_eq!(column_of(&bed, task).await, f.todo);

    bed.teardown().await;
}
