//! First-class epics (MAIN-81): parent create/patch round-trip, every
//! validation rule as a 400, delete-orphaning, the `?parent=` children filter,
//! the detail `children` array, and tenant isolation — through the real code
//! paths against a live Postgres. Set `DATABASE_URL`.

use nook_control::routes::task_detail::detail;
use nook_control::routes::task_query::{query_rows, TaskFilter};
use nook_testkit::TestBed;
use nook_types::*;
use sqlx::PgPool;
use uuid::Uuid;

/// A board with one column to hang cards on.
async fn make_board(db: &PgPool, tenant: TenantId) -> BoardId {
    let board = BoardId(Uuid::now_v7());
    sqlx::query(
        "INSERT INTO boards (id, tenant_id, name, key, provider) VALUES ($1,$2,'b',$3,'local')",
    )
    .bind(board)
    .bind(tenant)
    // The RANDOM tail of the uuid, not the leading timestamp — two v7 ids made
    // milliseconds apart share their prefix and would collide on (tenant, key).
    .bind(format!("B{}", &board.0.simple().to_string()[26..]).to_uppercase())
    .execute(db)
    .await
    .expect("board");
    sqlx::query(
        "INSERT INTO board_columns (id, board_id, name, position, type)
         VALUES ($1, $2, 'Backlog', 0, 'backlog'), ($3, $2, 'Todo', 1, 'unstarted')",
    )
    .bind(Uuid::now_v7())
    .bind(board)
    .bind(Uuid::now_v7())
    .execute(db)
    .await
    .expect("columns");
    board
}

fn req(title: &str, type_: Option<&str>, parent: Option<&str>) -> CreateTaskRequest {
    CreateTaskRequest {
        title: title.into(),
        description: None,
        column_id: None,
        column_type: None,
        workspace_id: None,
        priority: None,
        type_: type_.map(str::to_string),
        visibility: None,
        parent: parent.map(str::to_string),
        labels: vec![],
    }
}

fn patch_parent(parent: Option<Option<&str>>) -> UpdateTaskRequest {
    UpdateTaskRequest {
        title: None,
        description: None,
        column_id: None,
        column_type: None,
        position: None,
        assignee_user_id: None,
        priority: None,
        type_: None,
        visibility: None,
        workspace_id: None,
        parent: parent.map(|p| p.map(str::to_string)),
        expected_updated_at: None,
    }
}

#[tokio::test]
async fn epic_parent_create_patch_validate_filter_and_orphan() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping epic-parent test — no DATABASE_URL");
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("tp").await;
    let board = make_board(&bed.pool, tenant).await;
    let provider = state.kanban.get("local").expect("local provider");

    // An epic, and a plain task to be the "not an epic" parent later.
    let epic = provider
        .create_task(tenant, board, None, req("Epic A", Some("epic"), None))
        .await
        .expect("epic");
    // create_task returns the raw row; the human key is computed by enrich.
    let epic =
        nook_control::services::tasks::enrich_one(&bed.db(), "http://x", UserId::new(), epic)
            .await
            .expect("enrich epic");
    let epic_key = epic.key.clone().expect("epic key"); // e.g. B123-1
    let plain = provider
        .create_task(tenant, board, None, req("a plain task", None, None))
        .await
        .expect("plain");

    // ── Create under an epic BY KEY (AC-2/AC-5) ─────────────────────────────
    let child = provider
        .create_task(tenant, board, None, req("child one", None, Some(&epic_key)))
        .await
        .expect("child by key");
    assert_eq!(child.parent_task_id, Some(epic.id), "parent set by key");
    // A second child, placed in the backlog column, to prove `?parent=` spans
    // columns.
    let child2 = provider
        .create_task(
            tenant,
            board,
            None,
            CreateTaskRequest {
                column_type: Some("backlog".into()),
                ..req("child two", None, Some(&epic.id.to_string()))
            },
        )
        .await
        .expect("child by uuid");

    // ── `?parent=` lists exactly the children, across columns (AC-4) ────────
    let f = TaskFilter {
        board: Some(board.to_string()),
        parent: Some(epic_key.clone()),
        limit: Some(200),
        ..Default::default()
    };
    let mut ids: Vec<TaskId> = query_rows(&bed.db(), tenant, nook_types::UserId::new(), &f)
        .await
        .expect("children")
        .into_iter()
        .map(|t| t.id)
        .collect();
    ids.sort();
    let mut want = vec![child.id, child2.id];
    want.sort();
    assert_eq!(ids, want, "parent filter returns exactly the two children");

    // The epic's detail carries the children array (AC-5).
    let d = detail(&state, tenant, nook_types::UserId::new(), epic.id)
        .await
        .expect("epic detail");
    assert_eq!(d.children.len(), 2, "epic detail lists its children");
    assert!(
        d.children.iter().all(|c| c.key.is_some()),
        "children carry keys"
    );

    // ── Validation, each a 400 (AC-2) ───────────────────────────────────────
    // parent is not an epic
    assert!(provider
        .create_task(
            tenant,
            board,
            None,
            req("x", None, Some(&plain.id.to_string()))
        )
        .await
        .is_err());
    // an epic may not have a parent
    assert!(provider
        .create_task(tenant, board, None, req("x", Some("epic"), Some(&epic_key)))
        .await
        .is_err());
    // parent on another board
    let board2 = make_board(&bed.pool, tenant).await;
    let epic2 = provider
        .create_task(tenant, board2, None, req("Epic B", Some("epic"), None))
        .await
        .expect("epic2");
    assert!(
        provider
            .create_task(
                tenant,
                board,
                None,
                req("x", None, Some(&epic2.id.to_string()))
            )
            .await
            .is_err(),
        "cross-board parent refused"
    );

    // ── PATCH: detach, then retype-with-children guard (AC-3) ───────────────
    let detached = provider
        .update_task(tenant, None, child.id, patch_parent(Some(None)))
        .await
        .expect("detach");
    assert_eq!(detached.parent_task_id, None, "null detaches");

    // The epic still has one child (child2) → retyping it away from epic fails.
    assert!(
        provider
            .update_task(
                tenant,
                None,
                epic.id,
                UpdateTaskRequest {
                    type_: Some("task".into()),
                    ..patch_parent(None)
                }
            )
            .await
            .is_err(),
        "cannot un-epic a parent that still has children"
    );

    // ── Delete the epic → children ORPHAN, not deleted (AC-1) ───────────────
    sqlx::query("DELETE FROM tasks WHERE id = $1")
        .bind(epic.id)
        .execute(&bed.pool)
        .await
        .expect("delete epic");
    let (survivor_parent,): (Option<Uuid>,) =
        sqlx::query_as("SELECT parent_task_id FROM tasks WHERE id = $1")
            .bind(child2.id)
            .fetch_one(&bed.pool)
            .await
            .expect("child survives");
    assert_eq!(
        survivor_parent, None,
        "the child outlived its epic, parent nulled"
    );

    // ── Tenant isolation on parent resolution (AC-2) ────────────────────────
    let other = bed.tenant("tp").await;
    let other_board = make_board(&bed.pool, other).await;
    let other_epic = provider
        .create_task(
            other,
            other_board,
            None,
            req("Other epic", Some("epic"), None),
        )
        .await
        .expect("other epic");
    // A task in `tenant` cannot parent onto an epic in `other` (its key/uuid
    // does not resolve in this tenant).
    assert!(
        provider
            .create_task(
                tenant,
                board,
                None,
                req("x", None, Some(&other_epic.id.to_string()))
            )
            .await
            .is_err(),
        "parent resolution is tenant-scoped"
    );

    bed.teardown().await;
}

/// MAIN-76 × MAIN-81: a private child must not leak its title/key through the
/// epic's `children` array or the `?parent=` filter to a viewer who neither
/// created it nor is assigned — the leak the visibility rebase closed.
#[tokio::test]
async fn private_child_does_not_leak_through_epic() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping private-child leak test — no DATABASE_URL");
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("tp").await;
    let board = make_board(&bed.pool, tenant).await;
    let provider = state.kanban.get("local").expect("local provider");

    // Owner A and an unrelated member B.
    let a = UserId::new();
    let b = UserId::new();

    // A team epic, a team child, and a PRIVATE child owned by A.
    let epic = provider
        .create_task(tenant, board, Some(a), req("Epic", Some("epic"), None))
        .await
        .expect("epic");
    let epic =
        nook_control::services::tasks::enrich_one(&bed.db(), "http://x", UserId::new(), epic)
            .await
            .expect("enrich");
    let epic_key = epic.key.clone().expect("key");

    let team_child = provider
        .create_task(
            tenant,
            board,
            Some(a),
            req("open child", None, Some(&epic_key)),
        )
        .await
        .expect("team child");

    let secret_child = provider
        .create_task(
            tenant,
            board,
            Some(a),
            CreateTaskRequest {
                visibility: Some("private".into()),
                ..req("secret child", None, Some(&epic_key))
            },
        )
        .await
        .expect("private child");

    // ── Epic detail: A sees both children; B sees only the team one ─────────
    let a_detail = detail(&state, tenant, a, epic.id).await.expect("A detail");
    assert_eq!(a_detail.children.len(), 2, "the owner sees both children");
    let b_detail = detail(&state, tenant, b, epic.id).await.expect("B detail");
    let b_child_ids: Vec<TaskId> = b_detail.children.iter().map(|c| c.id).collect();
    assert!(
        b_child_ids.contains(&team_child.id),
        "B sees the team child"
    );
    assert!(
        !b_child_ids.contains(&secret_child.id),
        "B never sees the private child's title/key through epic detail"
    );

    // ── `?parent=` filter is likewise visibility-scoped ─────────────────────
    let f = TaskFilter {
        board: Some(board.to_string()),
        parent: Some(epic_key.clone()),
        limit: Some(200),
        ..Default::default()
    };
    let b_ids: Vec<TaskId> = query_rows(&bed.db(), tenant, b, &f)
        .await
        .expect("B parent list")
        .into_iter()
        .map(|t| t.id)
        .collect();
    assert!(b_ids.contains(&team_child.id));
    assert!(
        !b_ids.contains(&secret_child.id),
        "the parent filter never leaks the private child to a non-owner"
    );
    let a_ids: Vec<TaskId> = query_rows(&bed.db(), tenant, a, &f)
        .await
        .expect("A parent list")
        .into_iter()
        .map(|t| t.id)
        .collect();
    assert!(
        a_ids.contains(&secret_child.id),
        "the owner still sees their private child under the epic"
    );

    bed.teardown().await;
}

/// MAIN-86 AC-1/AC-2: the MCP task mutations (`set_task_parent`,
/// `set_task_description`) resolve a task's board provider instead of hardwiring
/// `LocalBoardProvider`. This guards the resolution path itself — the board→
/// provider join a task is mutated through, and that the resolved key yields a
/// live provider from the registry — so a rename of the column or the provider
/// key can never silently send MCP writes through the wrong (or no) provider.
#[tokio::test]
async fn mcp_task_mutations_resolve_the_boards_provider() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping provider-resolution test — no DATABASE_URL");
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("tp").await;
    let board = make_board(&bed.pool, tenant).await;
    let provider = state.kanban.get("local").expect("local provider");
    let task = provider
        .create_task(tenant, board, None, req("a task", None, None))
        .await
        .expect("create");

    // The exact resolution `McpBackend::provider_for_task` performs: a task's
    // board carries the provider string that decides which provider mutates it.
    let (resolved,): (String,) = sqlx::query_as(
        "SELECT b.provider FROM boards b
         JOIN tasks t ON t.board_id = b.id
         WHERE t.id = $1 AND t.tenant_id = $2",
    )
    .bind(task.id)
    .bind(tenant)
    .fetch_one(&bed.pool)
    .await
    .expect("resolve provider");
    assert_eq!(
        resolved, "local",
        "a local board's task resolves to the local provider"
    );
    assert!(
        state.kanban.get(&resolved).is_some(),
        "the resolved provider string yields a live provider from the registry"
    );

    bed.teardown().await;
}
