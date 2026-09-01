//! Issue types on tasks (MAIN-59): create/patch/persist and the list filter.
//!
//! Engine-neutral (MAIN-264): nothing here names a `sqlx` type, so the same
//! file runs on whichever engine `DATABASE_URL` selects.

use nook_control::routes::task_query::{query_rows, TaskFilter};
use nook_control::services::kanban::{KanbanProvider, LocalBoardProvider};
use nook_db::{params, Db, DbPool};
use nook_testkit::TestBed;
use nook_types::{BoardId, CreateTaskRequest, TenantId, UpdateTaskRequest};
use uuid::Uuid;

/// A tenant + board + one column to hang tasks on.
async fn fixture(db: &DbPool) -> (TenantId, BoardId) {
    let tenant = TenantId(Uuid::now_v7());
    db.exec(
        "INSERT INTO tenants (id, name, slug) VALUES ($1, $2, $3)",
        params![
            tenant,
            format!("t-{}", tenant.0.simple()),
            format!("t-{}", tenant.0.simple())
        ],
    )
    .await
    .expect("tenant");

    let board = BoardId(Uuid::now_v7());
    db.exec(
        "INSERT INTO boards (id, tenant_id, name, key, provider) VALUES ($1,$2,$3,$4,'local')",
        params![
            board,
            tenant,
            "b",
            format!("B{}", &board.0.simple().to_string()[..6]).to_uppercase()
        ],
    )
    .await
    .expect("board");
    db.exec(
        "INSERT INTO board_columns (id, board_id, name, position, type)
         VALUES ($1, $2, 'Triage', 0, 'unstarted')",
        params![Uuid::now_v7(), board],
    )
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
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (tenant, board) = fixture(&bed.db()).await;
    let provider = LocalBoardProvider {
        repo: std::sync::Arc::new(nook_control::repo::tasks::DbTaskRepository::new(bed.db())),
    };

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

    bed.teardown().await;
}

#[tokio::test]
async fn an_invalid_type_is_rejected_not_coerced() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (tenant, board) = fixture(&bed.db()).await;
    let provider = LocalBoardProvider {
        repo: std::sync::Arc::new(nook_control::repo::tasks::DbTaskRepository::new(bed.db())),
    };

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

    bed.teardown().await;
}

#[tokio::test]
async fn patch_changes_the_type() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (tenant, board) = fixture(&bed.db()).await;
    let provider = LocalBoardProvider {
        repo: std::sync::Arc::new(nook_control::repo::tasks::DbTaskRepository::new(bed.db())),
    };

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

    bed.teardown().await;
}

#[tokio::test]
async fn the_list_filter_ors_within_types_and_is_off_by_default() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (tenant, board) = fixture(&bed.db()).await;
    let provider = LocalBoardProvider {
        repo: std::sync::Arc::new(nook_control::repo::tasks::DbTaskRepository::new(bed.db())),
    };

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
        let db = bed.db();
        let repo = nook_control::repo::tasks::DbTaskRepository::new(db.clone());
        async move {
            query_rows(&repo, tenant, nook_types::UserId::new(), &f)
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

    // No type filter → the three buildable types; the epic is excluded by
    // default now (MAIN-80 AC-2), and returns only with an explicit type=epic.
    let all = filtered(vec![]).await;
    assert_eq!(all.len(), 3, "epic is excluded from the default pick");
    assert!(
        all.iter()
            .all(|t| ["task", "bug", "chore"].contains(&t.type_.as_str())),
        "no epic in the default pick"
    );
    assert!(
        !all.iter().any(|t| t.type_ == "epic"),
        "the epic is only returned when explicitly filtered for"
    );

    bed.teardown().await;
}
