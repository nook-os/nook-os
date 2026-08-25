//! A board belongs to a workspace (MAIN-637): adoption, auto-creation and the
//! two-way consistency rule. The one-shot boot backfill that caught up
//! pre-existing workspaces was retired in MAIN-640, having done its job;
//! `board_backfill_removed.rs` is what keeps it retired.
//!
//! Driven through the real route handlers, because every rule here is a
//! refusal and a refusal that lives only in the repository is one an endpoint
//! can forget to ask for. Set `DATABASE_URL`.
//!
//! Engine-neutral: every write binds through `params!` or goes through a
//! handler, and nothing here does interval arithmetic.

use axum::extract::{Path, State};
use axum::Json;
use nook_control::auth::{AuthCtx, Principal};
use nook_control::routes::{boards, workspaces};
use nook_db::{params, Db, DbPool};
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

fn ctx(tenant: TenantId, user: UserId) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId(Uuid::new_v4()),
        user_id: user,
        tenant_id: tenant,
        principal: Principal::User,
        cookie_session: true,
    }
}

/// A board written straight to the table, so a test can start from the state
/// prod is actually in: a board with cards and no workspace.
async fn raw_board(
    db: &DbPool,
    tenant: TenantId,
    key: &str,
    workspace: Option<WorkspaceId>,
) -> BoardId {
    let id = BoardId(Uuid::now_v7());
    db.exec(
        "INSERT INTO boards (id, tenant_id, workspace_id, name, key, provider)
         VALUES ($1, $2, $3, $4, $5, 'local')",
        params![
            id,
            tenant,
            workspace.map(|w| w.0),
            format!("board {key}"),
            key
        ],
    )
    .await
    .expect("board");
    id
}

async fn a_column(db: &DbPool, board: BoardId) -> ColumnId {
    let id = ColumnId(Uuid::now_v7());
    db.exec(
        "INSERT INTO board_columns (id, board_id, name, position, type)
         VALUES ($1, $2, 'Todo', 0, 'unstarted')",
        params![id, board],
    )
    .await
    .expect("column");
    id
}

async fn update(
    state: &nook_control::AppState,
    c: &AuthCtx,
    board: BoardId,
    body: serde_json::Value,
) -> Result<Board, nook_control::error::ApiError> {
    let req: UpdateBoardRequest = serde_json::from_value(body).expect("request");
    boards::update_board(State(state.clone()), *c, Path(board), Json(req))
        .await
        .map(|Json(b)| b)
}

/// AC-1. Attach, detach, leave alone — and the key survives every one of them,
/// which is the whole of NG-1: `MAIN` keeps every `MAIN-N` across an adoption.
#[tokio::test]
async fn patch_attaches_detaches_and_never_touches_the_key() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let db = bed.db();
    let tenant = bed.tenant("adopt").await;
    let (user, _) = bed.user(tenant, "owner").await;
    let c = ctx(tenant, user);
    let ws = bed.workspace(tenant).await;
    let board = raw_board(&db, tenant, "MAIN", None).await;

    let attached = update(
        &state,
        &c,
        board,
        serde_json::json!({ "name": "board MAIN", "workspace_id": ws.0 }),
    )
    .await
    .expect("attach");
    assert_eq!(attached.workspace_id, Some(ws));
    assert_eq!(attached.key.as_deref(), Some("MAIN"));

    // Omitted leaves it attached — the case a plain rename must not disturb.
    let renamed = update(&state, &c, board, serde_json::json!({ "name": "renamed" }))
        .await
        .expect("rename");
    assert_eq!(renamed.workspace_id, Some(ws));
    assert_eq!(renamed.name, "renamed");
    assert_eq!(renamed.key.as_deref(), Some("MAIN"));

    let detached = update(
        &state,
        &c,
        board,
        serde_json::json!({ "name": "renamed", "workspace_id": null }),
    )
    .await
    .expect("detach");
    assert_eq!(detached.workspace_id, None);
    assert_eq!(detached.key.as_deref(), Some("MAIN"));

    bed.teardown().await;
}

/// AC-2. A workspace has at most one board, and the second attempt is refused
/// by name rather than quietly stored.
#[tokio::test]
async fn a_workspace_takes_only_one_board() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let db = bed.db();
    let tenant = bed.tenant("one-board").await;
    let (user, _) = bed.user(tenant, "owner").await;
    let c = ctx(tenant, user);
    let ws = bed.workspace(tenant).await;

    let first = raw_board(&db, tenant, "AAA", None).await;
    let second = raw_board(&db, tenant, "BBB", None).await;

    update(
        &state,
        &c,
        first,
        serde_json::json!({ "name": "board AAA", "workspace_id": ws.0 }),
    )
    .await
    .expect("first attaches");

    let err = update(
        &state,
        &c,
        second,
        serde_json::json!({ "name": "board BBB", "workspace_id": ws.0 }),
    )
    .await
    .expect_err("second is refused");
    let msg = err.to_string();
    assert!(
        msg.contains("board AAA") && msg.contains("at most one"),
        "the refusal must name the board already there — got {msg:?}"
    );

    // Re-attaching the board that is already there is not a conflict with
    // itself: an idempotent PATCH has to stay idempotent.
    update(
        &state,
        &c,
        first,
        serde_json::json!({ "name": "board AAA", "workspace_id": ws.0 }),
    )
    .await
    .expect("re-attaching the same board is a no-op");

    bed.teardown().await;
}

/// AC-3. A new workspace comes with its board: one board, the five typed
/// columns, and a key derived from the name.
#[tokio::test]
async fn creating_a_workspace_creates_its_board() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("ws-board").await;
    let (user, _) = bed.user(tenant, "owner").await;
    let c = ctx(tenant, user);

    let req: CreateWorkspaceRequest =
        serde_json::from_value(serde_json::json!({ "name": "Payments service" })).expect("request");
    let Json(ws) = workspaces::create(State(state.clone()), c, Json(req))
        .await
        .expect("workspace");

    let board = state
        .tasks
        .board_of_workspace(tenant, ws.id)
        .await
        .expect("query")
        .expect("the workspace has a board");
    assert_eq!(board.key.as_deref(), Some("PAYM"));

    let columns = state.tasks.board_columns(board.id).await.expect("columns");
    let shape: Vec<(&str, &str)> = columns
        .iter()
        .map(|c| (c.name.as_str(), c.r#type.as_str()))
        .collect();
    assert_eq!(
        shape,
        vec![
            ("Triage", "backlog"),
            ("Todo", "unstarted"),
            ("In Progress", "started"),
            ("In Review", "review"),
            ("Done", "completed"),
        ]
    );

    bed.teardown().await;
}

/// AC-3's other half: no workspace is left boardless. With every derivable key
/// taken the board cannot be made, and the workspace must not survive it.
#[tokio::test]
async fn a_board_that_cannot_be_made_takes_the_workspace_with_it() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let db = bed.db();
    let tenant = bed.tenant("no-key-left").await;
    let (user, _) = bed.user(tenant, "owner").await;
    let c = ctx(tenant, user);

    // `unique_key` tries BASE, BASE2 … BASE99 and then gives up. Take all of
    // them, which is the only way to make board creation fail without reaching
    // into the repository and faking one.
    for n in 1..100 {
        let key = if n == 1 {
            "SOLO".to_string()
        } else {
            format!("SOLO{n}")
        };
        raw_board(&db, tenant, &key, None).await;
    }

    let req: CreateWorkspaceRequest =
        serde_json::from_value(serde_json::json!({ "name": "Solo repo" })).expect("request");
    workspaces::create(State(state.clone()), c, Json(req))
        .await
        .expect_err("no key is free, so the workspace creation fails as a whole");

    let left: Vec<Workspace> = state
        .workspaces
        .list(tenant)
        .await
        .expect("list")
        .into_iter()
        .filter(|w| w.name == "Solo repo")
        .collect();
    assert!(
        left.is_empty(),
        "the workspace must have been rolled back, found {left:?}"
    );

    bed.teardown().await;
}

/// AC-6. A card and its board must name the same workspace — on create, and on
/// the PATCH that re-files a card under a different workspace.
#[tokio::test]
async fn a_card_and_its_board_must_agree_on_the_workspace() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let db = bed.db();
    let tenant = bed.tenant("agree").await;
    let (user, _) = bed.user(tenant, "owner").await;
    let c = ctx(tenant, user);
    let mine = bed.workspace(tenant).await;
    let theirs = bed.workspace(tenant).await;

    let board = raw_board(&db, tenant, "MINE", Some(mine)).await;
    let column = a_column(&db, board).await;

    let make = |workspace: Option<WorkspaceId>| CreateTaskRequest {
        title: "a card".into(),
        description: None,
        column_id: Some(column),
        column_type: None,
        workspace_id: workspace,
        priority: None,
        type_: None,
        visibility: None,
        parent: None,
        labels: vec![],
    };

    let err = boards::create_task(
        State(state.clone()),
        c,
        Path(board),
        Json(make(Some(theirs))),
    )
    .await
    .expect_err("the other workspace's card is refused");
    let msg = err.to_string();
    assert!(
        msg.contains(&mine.0.to_string()) && msg.contains(&theirs.0.to_string()),
        "the refusal must name BOTH workspaces — got {msg:?}"
    );

    let Json(ok) =
        boards::create_task(State(state.clone()), c, Path(board), Json(make(Some(mine))))
            .await
            .expect("its own workspace's card is filed");

    // The move: re-filing that card under the other workspace is the same rule.
    let patch: UpdateTaskRequest =
        serde_json::from_value(serde_json::json!({ "workspace_id": theirs.0 })).expect("patch");
    boards::update_task(
        State(state.clone()),
        c,
        Path(ok.id.0.to_string()),
        Json(patch),
    )
    .await
    .expect_err("moving the card to the other workspace is refused");

    // A board with no workspace takes anything, exactly as it does today.
    let loose = raw_board(&db, tenant, "LOOS", None).await;
    let loose_column = a_column(&db, loose).await;
    let _ = boards::create_task(
        State(state.clone()),
        c,
        Path(loose),
        Json(CreateTaskRequest {
            column_id: Some(loose_column),
            ..make(Some(theirs))
        }),
    )
    .await
    .expect("an unattached board accepts any workspace's card");

    bed.teardown().await;
}

/// AC-7. Deleting a workspace destroys its board and every card on it, so it
/// says what it would destroy and refuses until the caller has answered that.
#[tokio::test]
async fn deleting_a_workspace_reports_and_then_takes_the_board() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let db = bed.db();
    let tenant = bed.tenant("delete").await;
    let (user, _) = bed.user(tenant, "owner").await;
    let c = ctx(tenant, user);
    let ws = bed.workspace(tenant).await;
    let board = raw_board(&db, tenant, "GONE", Some(ws)).await;
    let column = a_column(&db, board).await;

    for title in ["one", "two"] {
        let _ = boards::create_task(
            State(state.clone()),
            c,
            Path(board),
            Json(CreateTaskRequest {
                title: title.into(),
                description: None,
                column_id: Some(column),
                column_type: None,
                workspace_id: Some(ws),
                priority: None,
                type_: None,
                visibility: None,
                parent: None,
                labels: vec![],
            }),
        )
        .await
        .expect("card");
    }

    let refused = workspaces::delete(State(state.clone()), c, Path(ws), None)
        .await
        .expect_err("no acknowledgement, no delete");
    let msg = refused.to_string();
    assert!(
        msg.contains("GONE") && msg.contains('2'),
        "the refusal must state the board key and the card count — got {msg:?}"
    );
    assert!(
        state
            .workspaces
            .get(tenant, ws)
            .await
            .expect("query")
            .is_some(),
        "and must have deleted nothing"
    );

    let req: DeleteWorkspaceRequest =
        serde_json::from_value(serde_json::json!({ "delete_board": true })).expect("request");
    let Json(done) = workspaces::delete(State(state.clone()), c, Path(ws), Some(Json(req)))
        .await
        .expect("acknowledged");
    assert_eq!(done.board_deleted.as_deref(), Some("GONE"));
    assert_eq!(done.tasks_deleted, 2);

    assert!(state
        .tasks
        .get_board(tenant, board)
        .await
        .expect("query")
        .is_none());
    assert!(state
        .tasks
        .board_columns(board)
        .await
        .expect("query")
        .is_empty());
    assert_eq!(
        state.tasks.board_task_count(board).await.expect("query"),
        0,
        "the cards go with the board"
    );

    bed.teardown().await;
}

/// A workspace with no board deletes exactly as it always did: the gate is
/// about cards, and there are none to warn about.
#[tokio::test]
async fn a_boardless_workspace_still_deletes_unprompted() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("boardless").await;
    let (user, _) = bed.user(tenant, "owner").await;
    let c = ctx(tenant, user);
    let ws = bed.workspace(tenant).await;

    let Json(done) = workspaces::delete(State(state.clone()), c, Path(ws), None)
        .await
        .expect("nothing to acknowledge");
    assert_eq!(done.board_deleted, None);
    assert_eq!(done.tasks_deleted, 0);

    bed.teardown().await;
}
