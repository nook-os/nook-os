//! Issue types on tasks (MAIN-59): create/patch/persist and the list filter.
//!
//! Needs a running Postgres (the dev stack's works): set `DATABASE_URL`.

use nook_control::routes::task_query::{query_rows, TaskFilter};
use nook_control::services::kanban::{KanbanProvider, LocalBoardProvider};
use nook_types::{BoardId, CreateTaskRequest, TenantId, UpdateTaskRequest};
use sqlx::PgPool;
use uuid::Uuid;

mod common;
use common::test_pool;

/// A tenant + board + one column to hang tasks on.
async fn fixture(db: &PgPool) -> (TenantId, BoardId) {
    let tenant = TenantId(Uuid::now_v7());
    sqlx::query("INSERT INTO tenants (id, name, slug) VALUES ($1, $2, $3)")
        .bind(tenant)
        .bind(format!("t-{}", tenant.0.simple()))
        .bind(format!("t-{}", tenant.0.simple()))
        .execute(db)
        .await
        .expect("tenant");

    let board = BoardId(Uuid::now_v7());
    sqlx::query(
        "INSERT INTO boards (id, tenant_id, name, key, provider) VALUES ($1,$2,$3,$4,'local')",
    )
    .bind(board)
    .bind(tenant)
    .bind("b")
    .bind(format!("B{}", &board.0.simple().to_string()[..6]).to_uppercase())
    .execute(db)
    .await
    .expect("board");
    sqlx::query(
        "INSERT INTO board_columns (id, board_id, name, position, type)
         VALUES ($1, $2, 'Triage', 0, 'unstarted')",
    )
    .bind(Uuid::now_v7())
    .bind(board)
    .execute(db)
    .await
    .expect("column");

    (tenant, board)
}

fn create(title: &str, type_: Option<&str>) -> CreateTaskRequest {
    CreateTaskRequest {
        title: title.into(),
        description: None,
        column_id: None,
        column_type: None,
        workspace_id: None,
        priority: None,
        type_: type_.map(str::to_string),
        visibility: None,
        parent: None,
        labels: vec![],
    }
}

fn patch_type(type_: &str) -> UpdateTaskRequest {
    UpdateTaskRequest {
        title: None,
        description: None,
        column_id: None,
        column_type: None,
        position: None,
        assignee_user_id: None,
        priority: None,
        type_: Some(type_.into()),
        visibility: None,
        parent: None,
        workspace_id: None,
        expected_updated_at: None,
    }
}

#[tokio::test]
async fn create_persists_type_and_defaults_to_task() {
    let Some(db) = test_pool().await else { return };
    let (tenant, board) = fixture(&db).await;
    let provider = LocalBoardProvider { db: db.clone() };

    // Explicit type is stored.
    let bug = provider
        .create_task(tenant, board, None, create("a bug", Some("bug")))
        .await
        .expect("create bug");
    assert_eq!(bug.type_, "bug");

    // Omitted → defaults to `task` (AC-2/AC-5).
    let plain = provider
        .create_task(tenant, board, None, create("a task", None))
        .await
        .expect("create task");
    assert_eq!(plain.type_, "task");
}

#[tokio::test]
async fn an_invalid_type_is_rejected_not_coerced() {
    let Some(db) = test_pool().await else { return };
    let (tenant, board) = fixture(&db).await;
    let provider = LocalBoardProvider { db: db.clone() };

    let err = provider
        .create_task(tenant, board, None, create("nope", Some("frobnicate")))
        .await
        .expect_err("an invalid type is refused, not silently coerced (AC-2)");
    // A provider error wrapping a 400 — not a database CHECK 500.
    assert!(
        format!("{err:?}")
            .to_lowercase()
            .contains("invalid task type"),
        "got {err:?}"
    );
}

#[tokio::test]
async fn patch_changes_the_type() {
    let Some(db) = test_pool().await else { return };
    let (tenant, board) = fixture(&db).await;
    let provider = LocalBoardProvider { db: db.clone() };

    let task = provider
        .create_task(tenant, board, None, create("t", None))
        .await
        .expect("create");
    assert_eq!(task.type_, "task");

    let updated = provider
        .update_task(tenant, None, task.id, patch_type("epic"))
        .await
        .expect("patch");
    assert_eq!(updated.type_, "epic");
}

#[tokio::test]
async fn the_list_filter_ors_within_types_and_is_off_by_default() {
    let Some(db) = test_pool().await else { return };
    let (tenant, board) = fixture(&db).await;
    let provider = LocalBoardProvider { db: db.clone() };

    for (title, ty) in [("e", "epic"), ("b", "bug"), ("c", "chore"), ("t", "task")] {
        provider
            .create_task(tenant, board, None, create(title, Some(ty)))
            .await
            .expect("create");
    }

    let filtered = |types: Vec<&str>| {
        let f = TaskFilter {
            type_: types.into_iter().map(str::to_string).collect(),
            ..Default::default()
        };
        let db = db.clone();
        async move {
            query_rows(&db, tenant, nook_types::UserId::new(), &f)
                .await
                .expect("query")
        }
    };

    // OR within types.
    let both = filtered(vec!["epic", "bug"]).await;
    let mut got: Vec<String> = both.iter().map(|t| t.type_.clone()).collect();
    got.sort();
    assert_eq!(got, vec!["bug", "epic"]);

    // A single type.
    let epics = filtered(vec!["epic"]).await;
    assert_eq!(epics.len(), 1);
    assert_eq!(epics[0].type_, "epic");

    // No type filter → all four (unchanged behaviour, AC-5).
    let all = filtered(vec![]).await;
    assert_eq!(all.len(), 4);
    assert!(all
        .iter()
        .all(|t| ["task", "bug", "epic", "chore"].contains(&t.type_.as_str())));
}
