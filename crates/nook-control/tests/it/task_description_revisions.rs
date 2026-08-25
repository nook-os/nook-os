//! Description replaces keep what they overwrote (MAIN-470 AC-3).
//!
//! The task PATCH is a whole-body replace with no history, so one bad payload
//! (the literal `-` of 2026-08-08) destroyed a contract with no undo. The
//! service now records the prior body on every real replace; these prove the
//! row is written, retrievable newest-first, and NOT written for edits that
//! destroy nothing.
//!
//! Engine-neutral (MAIN-264): nothing here names a `sqlx` type, so the same
//! file runs on whichever engine `DATABASE_URL` selects.

use axum::extract::{Path, State};
use nook_control::auth::{AuthCtx, Principal};
use nook_control::error::ApiError;
use nook_control::repo::tasks::{DbTaskRepository, TaskRepository};
use nook_control::services::kanban::{KanbanProvider, LocalBoardProvider};
use nook_db::{params, Db, DbPool};
use nook_testkit::TestBed;
use nook_types::{AuthSessionId, BoardId, CreateTaskRequest, TenantId, UpdateTaskRequest, UserId};
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

fn create(title: &str, description: &str) -> CreateTaskRequest {
    CreateTaskRequest {
        title: title.into(),
        description: Some(description.into()),
        column_id: None,
        column_type: None,
        workspace_id: None,
        priority: None,
        type_: None,
        visibility: None,
        parent: None,
        labels: vec![],
    }
}

fn patch(description: Option<&str>, title: Option<&str>) -> UpdateTaskRequest {
    UpdateTaskRequest {
        title: title.map(str::to_string),
        description: description.map(str::to_string),
        column_id: None,
        column_type: None,
        position: None,
        assignee_user_id: None,
        priority: None,
        type_: None,
        visibility: None,
        parent: None,
        workspace_id: None,
        expected_updated_at: None,
    }
}

#[tokio::test]
async fn a_replace_keeps_the_prior_body_and_lists_newest_first() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (tenant, board) = fixture(&bed.db()).await;
    let repo = std::sync::Arc::new(DbTaskRepository::new(bed.db()));
    let provider = LocalBoardProvider { repo: repo.clone() };

    // A body worth protecting — the size the CLI's sanity floor guards too.
    let original = "the whole contract, painstakingly written. ".repeat(8);
    let t = provider
        .create_task(tenant, board, None, create("card", &original))
        .await
        .expect("create");

    provider
        .update_task(tenant, None, t.id, patch(Some("second body"), None))
        .await
        .expect("first replace");
    let revs = repo
        .description_revisions_of(tenant, t.id)
        .await
        .expect("list");
    assert_eq!(revs.len(), 1, "one replace, one revision");
    assert_eq!(revs[0].body, original, "the revision is the PRIOR body");

    provider
        .update_task(tenant, None, t.id, patch(Some("third body"), None))
        .await
        .expect("second replace");
    let revs = repo
        .description_revisions_of(tenant, t.id)
        .await
        .expect("list");
    assert_eq!(revs.len(), 2);
    assert_eq!(
        revs[0].body, "second body",
        "newest first — the reader is undoing the most recent clobber"
    );
    assert_eq!(revs[1].body, original);

    bed.teardown().await;
}

#[tokio::test]
async fn edits_that_destroy_nothing_record_nothing() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (tenant, board) = fixture(&bed.db()).await;
    let repo = std::sync::Arc::new(DbTaskRepository::new(bed.db()));
    let provider = LocalBoardProvider { repo: repo.clone() };

    let t = provider
        .create_task(tenant, board, None, create("card", "the body"))
        .await
        .expect("create");

    // A title-only patch replaces no description.
    provider
        .update_task(tenant, None, t.id, patch(None, Some("new title")))
        .await
        .expect("title patch");
    // A same-body replace destroys nothing either — recording it would bury
    // the real clobber under noise.
    provider
        .update_task(tenant, None, t.id, patch(Some("the body"), None))
        .await
        .expect("no-op replace");

    let revs = repo
        .description_revisions_of(tenant, t.id)
        .await
        .expect("list");
    assert!(revs.is_empty(), "nothing was lost, so nothing was kept");

    bed.teardown().await;
}

fn auth(user: UserId, tenant: TenantId) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: user,
        tenant_id: tenant,
        principal: Principal::User,
        cookie_session: false,
    }
}

/// A second real member of the tenant, the way task_visibility.rs makes one.
async fn add_member(db: &DbPool, tenant: TenantId, name: &str) -> UserId {
    let id = UserId::new();
    db.exec(
        // person_id BOUND, not gen_random_uuid(): that function is
        // Postgres-only and this file also runs on SQLite.
        "INSERT INTO users (id, tenant_id, person_id, display_name, email, role)
         VALUES ($1, $2, $6, $3, $4, $5)",
        params![
            id,
            tenant,
            name,
            format!("{}-{}@example.test", name, id.0.simple()),
            "member",
            Uuid::now_v7()
        ],
    )
    .await
    .expect("member");
    id
}

/// The review's must-fix (MAIN-76): the stored bodies ARE the descriptions,
/// so the history route must 404 a private card exactly where the detail read
/// does — and still serve the owner their undo.
#[tokio::test]
async fn a_private_cards_history_is_not_found_to_a_non_owner() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (tenant, board) = fixture(&bed.db()).await;
    let state = bed.app_state().await;
    let alice = add_member(&bed.db(), tenant, "alice").await;
    let bob = add_member(&bed.db(), tenant, "bob").await;

    let repo = std::sync::Arc::new(DbTaskRepository::new(bed.db()));
    let provider = LocalBoardProvider { repo };
    let mut req = create("secret", "the original secret body");
    req.visibility = Some("private".into());
    let t = provider
        .create_task(tenant, board, Some(alice), req)
        .await
        .expect("create private card");
    provider
        .update_task(tenant, Some(alice), t.id, patch(Some("rewritten"), None))
        .await
        .expect("replace");

    let denied = nook_control::routes::task_detail::list_revisions(
        State(state.clone()),
        auth(bob, tenant),
        Path(t.id.0.to_string()),
    )
    .await;
    assert!(
        matches!(denied, Err(ApiError::NotFound)),
        "a private card's history must be NotFound to a non-owner, got {denied:?}"
    );

    let seen = nook_control::routes::task_detail::list_revisions(
        State(state),
        auth(alice, tenant),
        Path(t.id.0.to_string()),
    )
    .await
    .expect("the owner still reads their undo");
    assert_eq!(seen.0.len(), 1);
    assert_eq!(seen.0[0].body, "the original secret body");

    bed.teardown().await;
}
