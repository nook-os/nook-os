//! The control plane's half of the build worktree's lifecycle (MAIN-480):
//! recording where a build run works, releasing that record when the card is
//! pruned even against a node that is gone, and telling a reconnecting node
//! which of the trees it holds nobody wants any more.
//!
//! Every test runs on a private `nook_testkit::TestBed`. Set `DATABASE_URL`.
//!
//! Scope note: the node side of prune (`RemoveWorktree` over the registry) needs
//! a live node, which a unit test cannot stand up — so the cases here are the
//! ones that do not: an OFFLINE node is precisely the AC-6 path, and the sweep's
//! decision is a set difference over rows.

use nook_control::services::{jobs, taskwork};
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;

async fn fixture(bed: &TestBed) -> (TenantId, UserId, BoardId, ColumnId, NodeId) {
    let tenant = bed.tenant("wtlife").await;
    let (user, person) = bed.user(tenant, "member").await;
    let node = bed.node(tenant, person).await;
    let board = BoardId::new();
    bed.db()
        .exec(
            "INSERT INTO boards (id, tenant_id, name, key, provider) VALUES ($1,$2,'b',$3,'local')",
            params![
                board,
                tenant,
                format!("B{}", &board.0.simple().to_string()[26..32]).to_uppercase()
            ],
        )
        .await
        .expect("board");
    let col = ColumnId::new();
    bed.db()
        .exec(
            "INSERT INTO board_columns (id, board_id, name, position, type)
             VALUES ($1,$2,'Todo',0,'unstarted')",
            params![col, board],
        )
        .await
        .expect("column");
    (tenant, user, board, col, node)
}

async fn task_in(
    bed: &TestBed,
    tenant: TenantId,
    board: BoardId,
    col: ColumnId,
    creator: UserId,
) -> TaskId {
    let id = TaskId::new();
    bed.db()
        .exec(
            "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, type, created_by)
             VALUES ($1,$2,$3,$4,'card','task',$5)",
            params![id, tenant, board, col, creator],
        )
        .await
        .expect("task");
    id
}

async fn recorded(bed: &TestBed, id: TaskId) -> (Option<String>, Option<NodeId>) {
    let row: Option<(Option<String>, Option<NodeId>)> = bed
        .db()
        .query_opt(
            "SELECT worktree_path, worktree_node_id FROM tasks WHERE id = $1",
            params![id],
        )
        .await
        .expect("read record");
    row.unwrap_or((None, None))
}

/// AC-6: a card must never be stranded by a machine that is gone. The record is
/// what PINS later passes to that node, so refusing to clear it while the node
/// is unreachable would leave the work unplaceable with no way out — the node
/// removes the directory when it next reports what it holds.
#[tokio::test]
async fn pruning_clears_the_record_even_when_the_node_is_unreachable() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (tenant, user, board, col, node) = fixture(&bed).await;
    let state = bed.app_state().await;
    let card = task_in(&bed, tenant, board, col, user).await;
    state
        .tasks
        .record_loop_worktree(card, node, "/cache/worktrees/build-ws-MAIN-42")
        .await
        .expect("record");

    // No node is connected to this test's registry, so the op cannot be
    // delivered — exactly the "node is dark" case.
    let updated = taskwork::prune_worktree(&state, tenant, card)
        .await
        .expect("prune must succeed against an unreachable node");

    assert_eq!(updated.worktree_path, None);
    assert_eq!(updated.worktree_node_id, None);
    assert_eq!(
        recorded(&bed, card).await,
        (None, None),
        "the record is gone from the row, not just the returned copy"
    );
    bed.teardown().await;
}

/// AC-6: pruning a card that never had a worktree is still refused — clearing
/// nothing is a caller mistake, and the unreachable-node tolerance above must
/// not turn it into a silent success.
#[tokio::test]
async fn pruning_a_card_with_no_worktree_is_still_refused() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (tenant, user, board, col, _node) = fixture(&bed).await;
    let state = bed.app_state().await;
    let card = task_in(&bed, tenant, board, col, user).await;

    assert!(
        taskwork::prune_worktree(&state, tenant, card)
            .await
            .is_err(),
        "no worktree to prune is an error, not a no-op success"
    );
    bed.teardown().await;
}

/// AC-1: a reconnecting node reports every build worktree it holds, and is told
/// to remove the ones no card records — including any pruned while it was down.
/// A tree a card still records is left alone, because "no running job" is the
/// normal state of a build tree between its passes.
#[tokio::test]
async fn the_sweep_removes_only_the_trees_no_card_records() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (tenant, user, board, col, node) = fixture(&bed).await;
    let state = bed.app_state().await;

    let live = task_in(&bed, tenant, board, col, user).await;
    state
        .tasks
        .record_loop_worktree(live, node, "/cache/worktrees/build-ws-MAIN-1")
        .await
        .expect("record");

    let held = vec![
        "/cache/worktrees/build-ws-MAIN-1".to_string(), // still wanted
        "/cache/worktrees/build-ws-MAIN-2".to_string(), // pruned while dark
        "/cache/worktrees/build-ws-MAIN-3".to_string(), // card deleted
    ];
    let orphans = jobs::sweep_worktrees_on_node(&state, tenant, node, &held)
        .await
        .expect("sweep");

    assert_eq!(
        orphans, 2,
        "exactly the two the control plane no longer records"
    );

    // And the other direction is deliberately NOT acted on: a path recorded here
    // that the node did not report is left alone, not re-created or cleared.
    let orphans_none = jobs::sweep_worktrees_on_node(&state, tenant, node, &[])
        .await
        .expect("sweep empty");
    assert_eq!(orphans_none, 0);
    assert_eq!(
        recorded(&bed, live).await.0.as_deref(),
        Some("/cache/worktrees/build-ws-MAIN-1"),
        "a node reporting nothing does not erase what the card records"
    );
    bed.teardown().await;
}

/// AC-4: the record is what every later step reads, so only the node actually
/// executing the job may write it — otherwise any node could point another
/// executor's card at a directory on its own disk.
#[tokio::test]
async fn only_the_executing_node_may_record_a_worktree() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (tenant, user, board, col, node) = fixture(&bed).await;
    let (_u2, person2) = bed.user(tenant, "member").await;
    let stranger = bed.node(tenant, person2).await;
    let state = bed.app_state().await;
    let card = task_in(&bed, tenant, board, col, user).await;

    let job = JobId::new();
    bed.db()
        .exec(
            "INSERT INTO loop_jobs
               (id, tenant_id, kind, target_task_id, requested_by, state, executor_node_id)
             VALUES ($1,$2,'build',$3,$4,'running',$5)",
            params![job, tenant, card, user, node],
        )
        .await
        .expect("job");

    jobs::record_worktree_from_node(&state, stranger, job, "/evil/path")
        .await
        .expect("dropped, not errored");
    assert_eq!(
        recorded(&bed, card).await,
        (None, None),
        "a node that does not execute this job cannot speak for its card"
    );

    jobs::record_worktree_from_node(&state, node, job, "/cache/worktrees/build-ws-MAIN-9")
        .await
        .expect("record");
    assert_eq!(
        recorded(&bed, card).await,
        (
            Some("/cache/worktrees/build-ws-MAIN-9".to_string()),
            Some(node)
        ),
        "the executor's report lands on the card"
    );
    bed.teardown().await;
}
