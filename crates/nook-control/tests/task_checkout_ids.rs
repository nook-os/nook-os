//! Tasks address their working directory by checkout id (MAIN-225): the
//! migration backfill, the present-rows resolution `start_work` records, and the
//! id-first `prune_worktree` target resolution.
//!
//! Everything runs against a private `nook_testkit::TestBed`; only rows this test
//! creates are ever touched. Set `DATABASE_URL`.
//!
//! Scope note: `start_work` and `prune_worktree` end-to-end drive a live node
//! (the worktree/remove ops go over the registry), which a unit test cannot stand
//! up. These tests cover the load-bearing logic — the backfill and the id-vs-path
//! resolution both paths use — directly.

use nook_control::services::taskwork::{present_checkout_at, prune_target};
use nook_testkit::TestBed;
use nook_types::*;
use sqlx::PgPool;

/// A present (or missing) checkout row on `node` at `path`, returning its id.
async fn checkout(
    db: &PgPool,
    tenant: TenantId,
    node: NodeId,
    ws: WorkspaceId,
    path: &str,
    missing: bool,
) -> NodeWorkspaceId {
    let id = NodeWorkspaceId::new();
    sqlx::query(
        "INSERT INTO node_workspaces
             (id, tenant_id, node_id, workspace_id, path, kind, missing_at)
         VALUES ($1, $2, $3, $4, $5, 'clone', CASE WHEN $6 THEN now() ELSE NULL END)",
    )
    .bind(id)
    .bind(tenant)
    .bind(node)
    .bind(ws)
    .bind(path)
    .bind(missing)
    .execute(db)
    .await
    .expect("checkout");
    id
}

/// A minimal task on a board, optionally carrying a legacy worktree pair and/or a
/// checkout id. Returns the loaded `TaskItem`.
#[allow(clippy::too_many_arguments)]
async fn task(
    db: &PgPool,
    tenant: TenantId,
    board: BoardId,
    column: ColumnId,
    worktree_path: Option<&str>,
    worktree_node: Option<NodeId>,
    checkout_id: Option<NodeWorkspaceId>,
) -> TaskItem {
    let id = TaskId::new();
    sqlx::query(
        "INSERT INTO tasks
             (id, tenant_id, board_id, column_id, title, position,
              worktree_path, worktree_node_id, checkout_id)
         VALUES ($1, $2, $3, $4, 't', 0, $5, $6, $7)",
    )
    .bind(id)
    .bind(tenant)
    .bind(board)
    .bind(column)
    .bind(worktree_path)
    .bind(worktree_node)
    .bind(checkout_id)
    .execute(db)
    .await
    .expect("task");
    sqlx::query_as::<_, TaskItem>("SELECT * FROM tasks WHERE id = $1")
        .bind(id)
        .fetch_one(db)
        .await
        .expect("load task")
}

/// A tenant + board + one `unstarted` column + a workspace + an owned node.
async fn fixture(bed: &TestBed) -> (TenantId, BoardId, ColumnId, WorkspaceId, NodeId) {
    let tenant = bed.tenant("ci").await;
    let (_u, person) = bed.user(tenant, "member").await;
    let node = bed.node(tenant, person).await;
    let ws = bed.workspace(tenant).await;
    let board = BoardId::new();
    sqlx::query(
        "INSERT INTO boards (id, tenant_id, name, key, provider) VALUES ($1,$2,'b',$3,'local')",
    )
    .bind(board)
    .bind(tenant)
    .bind(format!("B{}", &board.0.simple().to_string()[..6]).to_uppercase())
    .execute(&bed.pool)
    .await
    .expect("board");
    let col = ColumnId::new();
    sqlx::query("INSERT INTO board_columns (id, board_id, name, position, type) VALUES ($1,$2,'Todo',0,'unstarted')")
        .bind(col)
        .bind(board)
        .execute(&bed.pool)
        .await
        .expect("column");
    (tenant, board, col, ws, node)
}

async fn checkout_of(db: &PgPool, id: TaskId) -> Option<NodeWorkspaceId> {
    sqlx::query_scalar("SELECT checkout_id FROM tasks WHERE id = $1")
        .bind(id)
        .fetch_one(db)
        .await
        .expect("read checkout_id")
}

// ── AC-2: migration backfill ─────────────────────────────────────────────────

/// The exact backfill statement from 0027, run against fixture data.
async fn run_backfill(db: &PgPool) {
    sqlx::query(
        "UPDATE tasks t
         SET checkout_id = nw.id
         FROM node_workspaces nw
         WHERE t.checkout_id IS NULL
           AND t.worktree_path IS NOT NULL
           AND t.worktree_node_id IS NOT NULL
           AND nw.node_id = t.worktree_node_id
           AND nw.path = t.worktree_path
           AND nw.missing_at IS NULL",
    )
    .execute(db)
    .await
    .expect("backfill");
}

#[tokio::test]
async fn backfill_sets_matching_present_checkout_and_leaves_others_null() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (tenant, board, col, ws, node) = fixture(&bed).await;

    // A task with a live worktree that resolves to a present checkout.
    let present = checkout(&bed.pool, tenant, node, ws, "/srv/wt-a", false).await;
    let matched = task(
        &bed.pool,
        tenant,
        board,
        col,
        Some("/srv/wt-a"),
        Some(node),
        None,
    )
    .await;

    // A task whose worktree path resolves only to a MISSING checkout → stays NULL.
    checkout(&bed.pool, tenant, node, ws, "/srv/wt-gone", true).await;
    let gone = task(
        &bed.pool,
        tenant,
        board,
        col,
        Some("/srv/wt-gone"),
        Some(node),
        None,
    )
    .await;

    // A task with no worktree at all → stays NULL.
    let bare = task(&bed.pool, tenant, board, col, None, None, None).await;

    run_backfill(&bed.pool).await;

    assert_eq!(checkout_of(&bed.pool, matched.id).await, Some(present));
    assert_eq!(
        checkout_of(&bed.pool, gone.id).await,
        None,
        "missing checkout → NULL"
    );
    assert_eq!(
        checkout_of(&bed.pool, bare.id).await,
        None,
        "no worktree → NULL"
    );

    // Idempotent: a re-run does not disturb the already-set value.
    run_backfill(&bed.pool).await;
    assert_eq!(checkout_of(&bed.pool, matched.id).await, Some(present));

    bed.teardown().await;
}

// ── AC-3: start_work's present-rows resolution ───────────────────────────────

#[tokio::test]
async fn present_checkout_at_matches_present_rows_only() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (tenant, _board, _col, ws, node) = fixture(&bed).await;

    let present = checkout(&bed.pool, tenant, node, ws, "/srv/live", false).await;
    checkout(&bed.pool, tenant, node, ws, "/srv/dead", true).await;

    assert_eq!(
        present_checkout_at(&bed.db(), node, "/srv/live")
            .await
            .unwrap(),
        Some(present),
        "a present checkout resolves"
    );
    assert_eq!(
        present_checkout_at(&bed.db(), node, "/srv/dead")
            .await
            .unwrap(),
        None,
        "a missing checkout does not (start_work leaves checkout_id NULL)"
    );
    assert_eq!(
        present_checkout_at(&bed.db(), node, "/srv/never")
            .await
            .unwrap(),
        None,
        "an unscanned worktree path resolves to nothing"
    );

    bed.teardown().await;
}

// ── AC-4: prune_worktree's id-first target resolution ────────────────────────

#[tokio::test]
async fn prune_target_prefers_the_checkout_id_then_falls_back_to_the_legacy_pair() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let (tenant, board, col, ws, node) = fixture(&bed).await;

    // checkout_id set to a present row → resolves that row's path/node, even when
    // the legacy strings point elsewhere.
    let live = checkout(&bed.pool, tenant, node, ws, "/srv/by-id", false).await;
    let by_id = task(
        &bed.pool,
        tenant,
        board,
        col,
        Some("/srv/legacy"),
        Some(node),
        Some(live),
    )
    .await;
    assert_eq!(
        prune_target(&state, &by_id).await.unwrap(),
        Some(("/srv/by-id".to_string(), node)),
        "id wins over the legacy path"
    );

    // checkout_id points at a MISSING row → fall back to the legacy pair.
    let dead = checkout(&bed.pool, tenant, node, ws, "/srv/dead", true).await;
    let fallback = task(
        &bed.pool,
        tenant,
        board,
        col,
        Some("/srv/legacy"),
        Some(node),
        Some(dead),
    )
    .await;
    assert_eq!(
        prune_target(&state, &fallback).await.unwrap(),
        Some(("/srv/legacy".to_string(), node)),
        "a missing checkout falls back to the legacy pair"
    );

    // No id, legacy only → the legacy pair.
    let legacy_only = task(
        &bed.pool,
        tenant,
        board,
        col,
        Some("/srv/only"),
        Some(node),
        None,
    )
    .await;
    assert_eq!(
        prune_target(&state, &legacy_only).await.unwrap(),
        Some(("/srv/only".to_string(), node))
    );

    // Neither → None (the "no worktree to prune" case).
    let bare = task(&bed.pool, tenant, board, col, None, None, None).await;
    assert_eq!(prune_target(&state, &bare).await.unwrap(), None);

    bed.teardown().await;
}
