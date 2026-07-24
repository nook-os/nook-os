//! Assigning a task to a workspace after it has been filed.
//!
//! A task's workspace is what a confined `/loop-build` agent filters on, so an
//! unscoped ticket is one no loop will ever claim. Until this landed there was
//! no way to set it except at creation — `UpdateTaskRequest` had no such field
//! and the UPDATE never touched the column — which left a board of tickets
//! nothing could pick up and no way to fix them.
//!
//! The interesting part is that "leave it alone" and "clear it" are different
//! instructions that look identical in JSON if you are careless: both arrive as
//! an absent value. Every case below is one of those three.
//!
//! Needs a running Postgres (the dev stack's works): set `DATABASE_URL`.

use nook_control::services::kanban::{KanbanProvider, LocalBoardProvider};
use nook_types::{BoardId, CreateTaskRequest, TenantId, UpdateTaskRequest, WorkspaceId};
use sqlx::PgPool;
use uuid::Uuid;

mod common;
use common::test_pool;

/// An update that touches nothing, so each test states only what it changes.
fn no_change() -> UpdateTaskRequest {
    UpdateTaskRequest {
        title: None,
        description: None,
        column_id: None,
        column_type: None,
        position: None,
        assignee_user_id: None,
        priority: None,
        workspace_id: None,
    }
}

/// A tenant, a workspace and a board to hang tasks on.
async fn fixture(db: &PgPool) -> (TenantId, BoardId, WorkspaceId, WorkspaceId) {
    let tenant = TenantId(Uuid::now_v7());
    sqlx::query("INSERT INTO tenants (id, name, slug) VALUES ($1, $2, $3)")
        .bind(tenant)
        .bind(format!("t-{}", tenant.0.simple()))
        .bind(format!("t-{}", tenant.0.simple()))
        .execute(db)
        .await
        .expect("tenant");

    let mut ws = Vec::new();
    for n in 0..2 {
        let id = WorkspaceId(Uuid::now_v7());
        sqlx::query("INSERT INTO workspaces (id, tenant_id, name, slug) VALUES ($1, $2, $3, $4)")
            .bind(id)
            .bind(tenant)
            .bind(format!("ws-{n}-{}", id.0.simple()))
            .bind(format!("ws-{n}-{}", id.0.simple()))
            .execute(db)
            .await
            .expect("workspace");
        ws.push(id);
    }

    // Raw SQL: boards are created by the route, not by the provider trait, and
    // this test is about the provider. A board needs at least one column or
    // `create_task` has nowhere to put a card.
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

    (tenant, board, ws[0], ws[1])
}

async fn new_task(
    provider: &LocalBoardProvider,
    tenant: TenantId,
    board: BoardId,
    workspace: Option<WorkspaceId>,
) -> nook_types::TaskItem {
    provider
        .create_task(
            tenant,
            board,
            CreateTaskRequest {
                title: "t".into(),
                description: None,
                column_id: None,
                column_type: None,
                workspace_id: workspace,
                priority: None,
                labels: vec![],
            },
        )
        .await
        .expect("task")
}

#[tokio::test]
async fn workspace_can_be_set_changed_and_cleared() {
    let Some(db) = test_pool().await else { return };
    let (tenant, board, ws_a, ws_b) = fixture(&db).await;
    let provider = LocalBoardProvider { db: db.clone() };

    // Filed with no workspace — the state every ticket on the board was in.
    let task = new_task(&provider, tenant, board, None).await;
    assert_eq!(task.workspace_id, None);

    // Some(Some(id)) — assign it.
    let updated = provider
        .update_task(
            tenant,
            task.id,
            UpdateTaskRequest {
                workspace_id: Some(Some(ws_a)),
                ..no_change()
            },
        )
        .await
        .expect("assign");
    assert_eq!(updated.workspace_id, Some(ws_a));

    // Reassign, so this is not just "null → value" working by accident.
    let updated = provider
        .update_task(
            tenant,
            task.id,
            UpdateTaskRequest {
                workspace_id: Some(Some(ws_b)),
                ..no_change()
            },
        )
        .await
        .expect("reassign");
    assert_eq!(updated.workspace_id, Some(ws_b));

    // Some(None) — clear it. COALESCE cannot express this, which is why the
    // column is written through a CASE instead.
    let updated = provider
        .update_task(
            tenant,
            task.id,
            UpdateTaskRequest {
                workspace_id: Some(None),
                ..no_change()
            },
        )
        .await
        .expect("clear");
    assert_eq!(updated.workspace_id, None);
}

#[tokio::test]
async fn an_absent_workspace_leaves_the_existing_one_alone() {
    let Some(db) = test_pool().await else { return };
    let (tenant, board, ws_a, _) = fixture(&db).await;
    let provider = LocalBoardProvider { db: db.clone() };

    let task = new_task(&provider, tenant, board, Some(ws_a)).await;
    assert_eq!(task.workspace_id, Some(ws_a));

    // The common case by far: any other edit must not silently unscope a task
    // and strand it where no confined agent can claim it.
    let updated = provider
        .update_task(
            tenant,
            task.id,
            UpdateTaskRequest {
                title: Some("renamed".into()),
                ..no_change()
            },
        )
        .await
        .expect("retitle");
    assert_eq!(updated.title, "renamed");
    assert_eq!(updated.workspace_id, Some(ws_a));
}

/// The wire format, which is where the three cases are easiest to collapse
/// into two: serde applies a JSON `null` to the outer Option unless told not
/// to, making "clear it" indistinguishable from "leave it".
#[test]
fn the_three_json_cases_stay_distinct() {
    let absent: UpdateTaskRequest = serde_json::from_str(r#"{"title":"x"}"#).unwrap();
    assert_eq!(absent.workspace_id, None, "absent must mean leave alone");

    let null: UpdateTaskRequest = serde_json::from_str(r#"{"workspace_id":null}"#).unwrap();
    assert_eq!(null.workspace_id, Some(None), "null must mean clear");

    let id = Uuid::now_v7();
    let set: UpdateTaskRequest =
        serde_json::from_str(&format!(r#"{{"workspace_id":"{id}"}}"#)).unwrap();
    assert_eq!(set.workspace_id, Some(Some(WorkspaceId(id))));
}
