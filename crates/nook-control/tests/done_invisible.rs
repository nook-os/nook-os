//! Finished work is invisible to the loop (MAIN-464): the pick query excludes
//! `completed`- and `canceled`-column tasks by default whatever labels they
//! carry, `claim_inner` refuses them with their own 400s, and the two lifts
//! (`parent=`, an explicit `column_type`) still return them — all through the
//! real code paths against a live database. Set `DATABASE_URL`.
//!
//! The sibling of `backlog_invisible.rs`: MAIN-80 shut the triage end of the
//! board and left this one open, so a merged card with `agent-ready` still
//! attached was handed back to the next builder (MAIN-441, MAIN-302).
//!
//! Setup + teardown run through `nook_testkit::TestBed` (MAIN-156).

use nook_control::error::ApiError;
use nook_control::routes::task_query::{claim_inner, query_rows, TaskFilter};
use nook_db::{params, Db, DbPool};
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

/// The four columns this file needs: Todo, In Review, Done, Canceled.
struct Board {
    tenant: TenantId,
    board: BoardId,
    todo: ColumnId,
    review: ColumnId,
    done: ColumnId,
    canceled: ColumnId,
}

async fn fixture(db: &DbPool) -> Board {
    let tenant = TenantId(Uuid::now_v7());
    db.exec(
        "INSERT INTO tenants (id, name, slug) VALUES ($1, $2, $2)",
        params![tenant, format!("t-{}", tenant.0.simple())],
    )
    .await
    .expect("tenant");
    let board = BoardId(Uuid::now_v7());
    db.exec(
        "INSERT INTO boards (id, tenant_id, name, key, provider) VALUES ($1,$2,'b',$3,'local')",
        params![
            board,
            tenant,
            // The random tail, not the shared v7 timestamp prefix, so keys
            // don't collide.
            format!("B{}", &board.0.simple().to_string()[26..]).to_uppercase()
        ],
    )
    .await
    .expect("board");
    let (todo, review, done, canceled) = (
        ColumnId(Uuid::now_v7()),
        ColumnId(Uuid::now_v7()),
        ColumnId(Uuid::now_v7()),
        ColumnId(Uuid::now_v7()),
    );
    db.exec(
        "INSERT INTO board_columns (id, board_id, name, position, type)
         VALUES ($1,$2,'Todo',0,'unstarted'), ($3,$2,'In Review',1,'review'),
                ($4,$2,'Done',2,'completed'), ($5,$2,'Canceled',3,'canceled')",
        params![todo, board, review, done, canceled],
    )
    .await
    .expect("columns");
    Board {
        tenant,
        board,
        todo,
        review,
        done,
        canceled,
    }
}

async fn task(db: &DbPool, b: &Board, col: ColumnId, number: i32) -> TaskId {
    let id = TaskId::new();
    db.exec(
        "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, number, type)
         VALUES ($1,$2,$3,$4,$5,$6,'task')",
        params![id, b.tenant, b.board, col, format!("task {number}"), number],
    )
    .await
    .expect("task");
    id
}

/// Attach a label directly, which is what a human leaving `agent-ready` on a
/// card they then moved to Done has done.
async fn label(db: &DbPool, b: &Board, task: TaskId, name: &str) {
    let label = Uuid::now_v7();
    db.exec(
        "INSERT INTO labels (id, tenant_id, name, color) VALUES ($1,$2,$3,'#3fb950')",
        params![label, b.tenant, name],
    )
    .await
    .expect("label");
    db.exec(
        "INSERT INTO task_labels (task_id, label_id) VALUES ($1,$2)",
        params![task, label],
    )
    .await
    .expect("task_label");
}

async fn pick_ids(db: &DbPool, tenant: TenantId, f: &TaskFilter) -> Vec<TaskId> {
    // These fixtures set no visibility (default non-private), so any viewer
    // sees them; the pick's MAIN-76 predicate is exercised by task_visibility.rs.
    query_rows(
        &nook_control::repo::tasks::DbTaskRepository::new(db.clone()),
        tenant,
        UserId::new(),
        f,
    )
    .await
    .expect("pick")
    .into_iter()
    .map(|t| t.id)
    .collect()
}

#[tokio::test]
async fn pick_excludes_completed_and_canceled_by_default() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping done-exclusion test — no DATABASE_URL");
        return;
    };
    let b = fixture(&bed.db()).await;

    let todo = task(&bed.db(), &b, b.todo, 1).await;
    let in_review = task(&bed.db(), &b, b.review, 2).await;
    let done = task(&bed.db(), &b, b.done, 3).await;
    let canceled = task(&bed.db(), &b, b.canceled, 4).await;
    // AC-3: the label is still attached — the human's to remove (NG-1), and it
    // must not be what decides whether the card is offered.
    label(&bed.db(), &b, done, "agent-ready").await;

    let base = TaskFilter {
        board: Some(b.board.to_string()),
        limit: Some(200),
        ..Default::default()
    };

    // ── Default: completed + canceled excluded (AC-1) ───────────────────────
    let def = pick_ids(&bed.db(), b.tenant, &base).await;
    assert!(def.contains(&todo), "a Todo card is picked");
    assert!(
        def.contains(&in_review),
        "In Review is `review`, not `completed` — still live work"
    );
    assert!(
        !def.contains(&done),
        "a Done card is NOT in the default pick"
    );
    assert!(
        !def.contains(&canceled),
        "nor is a Canceled one — the work is over either way"
    );

    // ── AC-3: the loop's own pick, label and all ────────────────────────────
    let loop_pick = pick_ids(
        &bed.db(),
        b.tenant,
        &TaskFilter {
            label: vec!["agent-ready".into()],
            assignee: Some("none".into()),
            is_blocked: Some(false),
            ..base.clone()
        },
    )
    .await;
    assert!(
        !loop_pick.contains(&done),
        "a card moved to Done with agent-ready still attached simply disappears"
    );

    // ── done=true reveals them again (the opt-in, mirroring `backlog`) ──────
    let with_done = pick_ids(
        &bed.db(),
        b.tenant,
        &TaskFilter {
            done: Some(true),
            ..base.clone()
        },
    )
    .await;
    assert!(with_done.contains(&done), "done=true reveals the Done card");
    assert!(
        with_done.contains(&canceled),
        "and the canceled one — one flag, both finished types"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn column_type_and_parent_lift_the_done_exclusion() {
    // Both lifts exist because the caller has already said it wants the
    // finished end of the board: `column_type=completed` asking for Done and
    // being told "none" would be the same silent lie in the other direction,
    // and an epic's done/total is the reason anyone lists its tickets.
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping done-lift test — no DATABASE_URL");
        return;
    };
    let b = fixture(&bed.db()).await;

    let done = task(&bed.db(), &b, b.done, 1).await;
    let canceled = task(&bed.db(), &b, b.canceled, 2).await;

    let epic = TaskId::new();
    bed.db()
        .exec(
            "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, number, type)
             VALUES ($1,$2,$3,$4,'the epic',3,'epic')",
            params![epic, b.tenant, b.board, b.todo],
        )
        .await
        .expect("epic");
    let child_done = task(&bed.db(), &b, b.done, 4).await;
    let child_todo = task(&bed.db(), &b, b.todo, 5).await;
    for child in [child_done, child_todo] {
        bed.db()
            .exec(
                "UPDATE tasks SET parent_task_id = $1 WHERE id = $2",
                params![epic, child],
            )
            .await
            .expect("set parent");
    }

    let base = TaskFilter {
        board: Some(b.board.to_string()),
        limit: Some(200),
        ..Default::default()
    };

    for (ct, expected) in [("completed", done), ("canceled", canceled)] {
        let rows = pick_ids(
            &bed.db(),
            b.tenant,
            &TaskFilter {
                column_type: Some(ct.into()),
                ..base.clone()
            },
        )
        .await;
        assert!(
            rows.contains(&expected),
            "column_type={ct} returns that column's cards rather than an empty list"
        );
    }

    let children = pick_ids(
        &bed.db(),
        b.tenant,
        &TaskFilter {
            parent: Some(epic.to_string()),
            ..base.clone()
        },
    )
    .await;
    assert!(
        children.contains(&child_done),
        "a parent= query returns the finished child, so done/total is countable"
    );
    assert!(children.contains(&child_todo), "and the unfinished one");

    bed.teardown().await;
}

#[tokio::test]
async fn claim_refuses_completed_and_canceled_with_distinct_messages() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping done-claim test — no DATABASE_URL");
        return;
    };
    let state = bed.app_state().await;
    let b = fixture(&bed.db()).await;
    // A real user row — claiming sets assignee_user_id, which has an FK.
    let claimant = UserId::new();
    bed.db()
        .exec(
            // The person id is BOUND rather than `gen_random_uuid()`: that
            // function is Postgres-only and this test also runs on SQLite
            // (MAIN-435).
            "INSERT INTO users (id, tenant_id, person_id, display_name, email)
             VALUES ($1, $2, $4, 'Claimant', $3)",
            params![
                claimant,
                b.tenant,
                format!("claimant-{}@example.test", claimant.0.simple()),
                Uuid::now_v7()
            ],
        )
        .await
        .expect("claimant user");

    let done = task(&bed.db(), &b, b.done, 1).await;
    let canceled = task(&bed.db(), &b, b.canceled, 2).await;
    let todo = task(&bed.db(), &b, b.todo, 3).await;

    // A 400 saying the work is over — NOT the 409 lost-claim message, which
    // would send a builder off to pick again as if it had merely been beaten.
    let done_err = claim_inner(&state, b.tenant, claimant, &done.to_string(), None)
        .await
        .expect_err("a Done card is not claimable");
    match &done_err {
        ApiError::BadRequest(m) => assert_eq!(m, "this card is done — nothing to build"),
        other => panic!("expected a 400 naming Done, got {other:?}"),
    }

    let canceled_err = claim_inner(&state, b.tenant, claimant, &canceled.to_string(), None)
        .await
        .expect_err("a Canceled card is not claimable");
    match &canceled_err {
        ApiError::BadRequest(m) => assert_eq!(m, "this card was canceled — nothing to build"),
        other => panic!("expected a 400 naming Canceled, got {other:?}"),
    }

    // And the live card still claims, so this is a refusal and not a wall.
    claim_inner(&state, b.tenant, claimant, &todo.to_string(), None)
        .await
        .expect("a Todo card is still claimable");

    bed.teardown().await;
}
