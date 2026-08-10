//! Which build compose stacks a node is told to bring down (MAIN-507).
//!
//! Every test runs on a private `nook_testkit::TestBed`. Set `DATABASE_URL`.
//!
//! Scope note, the same one `loop_worktree_lifecycle` carries: actually running
//! `docker compose down` needs a live node and a daemon, which a unit test
//! cannot stand up. What is decidable here is the part that decides — which
//! projects a node is asked to reap — and that is a set difference over rows.
//! The node's own half (the NG-3 name guard, and the AC-3 ordering against the
//! directory removal) is unit-tested in `nook-node`'s `compose` module.

use nook_control::services::stack_reaper;
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;

/// A worktree path of the shape the node's `build_dirname` produces.
fn build_worktree(key: &str) -> String {
    format!(
        "/root/.nook/clone-cache/host/worktrees/build-019f840f-2d80-7163-b4b1-8b1e12d7e0d3-{key}"
    )
}

/// The project `scripts/compose-project.sh` exports for that worktree.
fn project(key: &str) -> String {
    format!(
        "nook-build-019f840f-2d80-7163-b4b1-8b1e12d7e0d3-{}",
        key.to_lowercase()
    )
}

struct Fixture {
    tenant: TenantId,
    user: UserId,
    board: BoardId,
    node: NodeId,
}

async fn fixture(bed: &TestBed) -> Fixture {
    let tenant = bed.tenant("stackreap").await;
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
    Fixture {
        tenant,
        user,
        board,
        node,
    }
}

async fn column(bed: &TestBed, f: &Fixture, name: &str, kind: &str, position: i32) -> ColumnId {
    let id = ColumnId::new();
    bed.db()
        .exec(
            "INSERT INTO board_columns (id, board_id, name, position, type)
             VALUES ($1,$2,$3,$4,$5)",
            params![id, f.board, name, position, kind],
        )
        .await
        .expect("column");
    id
}

/// A card in `col` whose build worktree is recorded on the node.
async fn card_with_worktree(bed: &TestBed, f: &Fixture, col: ColumnId, key: &str) -> TaskId {
    let id = TaskId::new();
    bed.db()
        .exec(
            "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, type, created_by,
                                worktree_path, worktree_node_id)
             VALUES ($1,$2,$3,$4,'card','task',$5,$6,$7)",
            params![
                id,
                f.tenant,
                f.board,
                col,
                f.user,
                build_worktree(key),
                f.node
            ],
        )
        .await
        .expect("task");
    id
}

/// AC-1: the card is over, so its stack is collected. Both terminal types, so a
/// canceled card is not quietly left holding 11 containers.
#[tokio::test]
async fn a_finished_cards_stack_is_collected() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let done = column(&bed, &f, "Done", "completed", 3).await;
    let dropped = column(&bed, &f, "Canceled", "canceled", 4).await;
    card_with_worktree(&bed, &f, done, "MAIN-496").await;
    card_with_worktree(&bed, &f, dropped, "MAIN-489").await;
    let state = bed.app_state().await;

    let held = vec![project("MAIN-496"), project("MAIN-489")];
    let reaped = stack_reaper::sweep_stacks_on_node(&state, f.node, &held)
        .await
        .expect("sweep");

    assert_eq!(reaped, 2, "both terminal cards' stacks should be collected");
    bed.teardown().await;
}

/// AC-6, the negative that matters most: a card in review has a FINISHED build
/// and a live worktree, and a repair run reuses both. Taking its stack down
/// breaks the next pass, so review is not terminal and is never touched.
#[tokio::test]
async fn a_card_still_in_review_keeps_its_stack() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let review = column(&bed, &f, "In Review", "review", 2).await;
    let progress = column(&bed, &f, "In Progress", "started", 1).await;
    card_with_worktree(&bed, &f, review, "MAIN-397").await;
    card_with_worktree(&bed, &f, progress, "MAIN-475").await;
    let state = bed.app_state().await;

    let held = vec![project("MAIN-397"), project("MAIN-475")];
    let reaped = stack_reaper::sweep_stacks_on_node(&state, f.node, &held)
        .await
        .expect("sweep");

    assert_eq!(
        reaped, 0,
        "an unfinished card's stack must survive the sweep"
    );
    bed.teardown().await;
}

/// AC-5: the stacks this bug already orphaned have no card pointing at them at
/// all — the worktree record went with the prune. They are exactly the ones the
/// sweep exists for.
#[tokio::test]
async fn a_stack_no_card_records_is_collected() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let review = column(&bed, &f, "In Review", "review", 2).await;
    card_with_worktree(&bed, &f, review, "MAIN-397").await;
    let state = bed.app_state().await;

    let held = vec![project("MAIN-397"), project("MAIN-201")];
    let reaped = stack_reaper::sweep_stacks_on_node(&state, f.node, &held)
        .await
        .expect("sweep");

    assert_eq!(reaped, 1, "only the unrecorded stack should be collected");
    bed.teardown().await;
}

/// NG-3: a human's own dev stack, the operator node's, and anything else on the
/// machine are not build worktrees and are never named.
#[tokio::test]
async fn nothing_outside_a_build_worktree_is_ever_collected() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;

    let held = vec![
        "nook-nook-os".to_string(),
        "nook-operator".to_string(),
        "services".to_string(),
        "build-tools".to_string(),
    ];
    let reaped = stack_reaper::sweep_stacks_on_node(&state, f.node, &held)
        .await
        .expect("sweep");

    assert_eq!(reaped, 0);
    bed.teardown().await;
}

/// A card's worktree is protected under BOTH spellings its stack can have: the
/// `nook-` one `dev-up.sh` exports, and compose's own default from a bare
/// `docker compose up` in the worktree. Only one of the two was on the card's
/// evidence; both were on the machine.
#[tokio::test]
async fn both_spellings_of_a_live_cards_project_are_protected() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let review = column(&bed, &f, "In Review", "review", 2).await;
    card_with_worktree(&bed, &f, review, "MAIN-502").await;
    let state = bed.app_state().await;

    let held = vec![
        project("MAIN-502"),
        "build-019f840f-2d80-7163-b4b1-8b1e12d7e0d3-main-502".to_string(),
    ];
    let reaped = stack_reaper::sweep_stacks_on_node(&state, f.node, &held)
        .await
        .expect("sweep");

    assert_eq!(reaped, 0);
    bed.teardown().await;
}

/// AC-4: the node is offline, so nothing can be brought down — and the card
/// move that triggered this still succeeds. The next inventory collects it.
#[tokio::test]
async fn an_unreachable_node_does_not_fail_the_card() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let done = column(&bed, &f, "Done", "completed", 3).await;
    let task = card_with_worktree(&bed, &f, done, "MAIN-496").await;
    let state = bed.app_state().await;

    stack_reaper::reap_for_task(&state, f.tenant, task)
        .await
        .expect("an offline node is not an error");
    bed.teardown().await;
}

/// Most cards never boot a stack. A card with no worktree at all reaps nothing
/// and, in particular, comments nothing.
#[tokio::test]
async fn a_card_with_no_worktree_is_silent() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let done = column(&bed, &f, "Done", "completed", 3).await;
    let id = TaskId::new();
    bed.db()
        .exec(
            "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, type, created_by)
             VALUES ($1,$2,$3,$4,'card','task',$5)",
            params![id, f.tenant, f.board, done, f.user],
        )
        .await
        .expect("task");
    let state = bed.app_state().await;

    stack_reaper::reap_for_task(&state, f.tenant, id)
        .await
        .expect("no worktree is not an error");

    let comments: Option<(i64,)> = bed
        .db()
        .query_opt(
            "SELECT count(*) FROM task_comments WHERE task_id = $1",
            params![id],
        )
        .await
        .expect("count");
    assert_eq!(comments.map(|c| c.0), Some(0));
    bed.teardown().await;
}
