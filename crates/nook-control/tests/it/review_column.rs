//! The "In Review" column (MAIN-71): the widened type CHECK admits `review`,
//! the migration backfill positions exactly one review column before the first
//! completed column and is idempotent, and the by-type resolution `submit_pr`
//! uses parks a PR in review — falling back to completed when a board has none.
//!
//! Needs a running Postgres (the dev stack's works): set `DATABASE_URL`.

use nook_control::services::tasks::column_of_type;
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::{BoardId, TenantId};
use uuid::Uuid;

async fn make_board(bed: &TestBed) -> BoardId {
    let tenant = TenantId(Uuid::now_v7());
    bed.db()
        .exec(
            "INSERT INTO tenants (id, name, slug) VALUES ($1, $2, $2)",
            params![tenant, format!("t-{}", tenant.0.simple())],
        )
        .await
        .expect("tenant");
    let board = BoardId(Uuid::now_v7());
    bed.db()
        .exec(
            "INSERT INTO boards (id, tenant_id, name, key, provider) VALUES ($1,$2,'b',$3,'local')",
            params![
                board,
                tenant,
                format!("B{}", &board.0.simple().to_string()[..6]).to_uppercase()
            ],
        )
        .await
        .expect("board");
    board
}

async fn add_col(bed: &TestBed, board: BoardId, name: &str, pos: i32, kind: &str) {
    bed.db()
        .exec(
            "INSERT INTO board_columns (id, board_id, name, position, type) VALUES ($1,$2,$3,$4,$5)",
            params![Uuid::now_v7(), board, name, pos, kind],
        )
        .await
        .unwrap_or_else(|e| panic!("insert {name}/{kind}: {e}"));
}

/// Columns of the board as `(name, position, type)`, ordered by position.
async fn cols(bed: &TestBed, board: BoardId) -> Vec<(String, i32, String)> {
    bed.db()
        .query_all(
            "SELECT name, position, type FROM board_columns WHERE board_id = $1 ORDER BY position",
            params![board],
        )
        .await
        .expect("cols")
}

/// The migration's backfill, run against ONE board. Mirrors
/// `0010_review_column.sql` so the positioning + idempotency it relies on is
/// pinned by a test rather than only exercised once at deploy.
///
/// Scoped to `board` with `= $1`, not global. The migration is global, but the
/// test must be: a global backfill mutates every other test's boards, and under
/// parallel DB load that races `submit_pr_resolution` — inserting a review
/// column into the board it needs to have none — which is exactly the isolation
/// gap that surfaced once the suite gained more concurrent DB tests.
async fn run_backfill(bed: &TestBed, board: BoardId) {
    bed.db()
        .exec(
        "UPDATE board_columns AS c SET position = position + 1
         WHERE c.board_id = $1
           AND EXISTS (SELECT 1 FROM board_columns d WHERE d.board_id = c.board_id AND d.type = 'completed')
           AND NOT EXISTS (SELECT 1 FROM board_columns d WHERE d.board_id = c.board_id AND d.type = 'review')
           AND c.position >= (SELECT min(position) FROM board_columns d WHERE d.board_id = c.board_id AND d.type = 'completed')",
            params![board],
        )
        .await
        .expect("shift");
    bed.db()
        .exec(
        "INSERT INTO board_columns (id, board_id, name, position, type)
         SELECT $2, b.id, 'In Review',
                (SELECT min(position) FROM board_columns c WHERE c.board_id = b.id AND c.type = 'completed') - 1,
                'review'
         FROM boards b
         WHERE b.id = $1
           AND EXISTS (SELECT 1 FROM board_columns c WHERE c.board_id = b.id AND c.type = 'completed')
           AND NOT EXISTS (SELECT 1 FROM board_columns c WHERE c.board_id = b.id AND c.type = 'review')",
            params![board, Uuid::now_v7()],
        )
        .await
        .expect("insert review");
}

#[tokio::test]
async fn review_is_a_valid_type_and_resolves_by_type() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping review_is_a_valid_type — no DATABASE_URL");
        return;
    };
    let board = make_board(&bed).await;
    // The widened CHECK (AC-1) admits 'review'; this insert would fail otherwise.
    add_col(&bed, board, "In Progress", 0, "started").await;
    add_col(&bed, board, "In Review", 1, "review").await;
    add_col(&bed, board, "Done", 2, "completed").await;

    let review = column_of_type(
        &nook_control::repo::tasks::DbTaskRepository::new(bed.db()),
        board,
        "review",
    )
    .await
    .expect("review resolves");
    let by_pos = cols(&bed, board).await;
    // The resolved id is the review-typed column.
    assert_eq!(by_pos[1].2, "review");
    assert_eq!(by_pos[1].0, "In Review");
    let review_row: (String,) = bed
        .db()
        .query_one(
            "SELECT type FROM board_columns WHERE id = $1",
            params![review],
        )
        .await
        .unwrap();
    assert_eq!(review_row.0, "review");

    bed.teardown().await;
}

#[tokio::test]
async fn submit_pr_resolution_prefers_review_then_falls_back_to_completed() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping submit_pr_resolution — no DATABASE_URL");
        return;
    };
    // A board WITH a review column: review resolves (submit_pr parks here).
    let with_review = make_board(&bed).await;
    add_col(&bed, with_review, "In Progress", 0, "started").await;
    add_col(&bed, with_review, "In Review", 1, "review").await;
    add_col(&bed, with_review, "Done", 2, "completed").await;
    assert!(column_of_type(
        &nook_control::repo::tasks::DbTaskRepository::new(bed.db()),
        with_review,
        "review"
    )
    .await
    .is_ok());

    // A board WITHOUT one: review errors and the fallback (completed) resolves —
    // exactly the chain submit_pr walks.
    let no_review = make_board(&bed).await;
    add_col(&bed, no_review, "In Progress", 0, "started").await;
    add_col(&bed, no_review, "Done", 1, "completed").await;
    assert!(
        column_of_type(
            &nook_control::repo::tasks::DbTaskRepository::new(bed.db()),
            no_review,
            "review"
        )
        .await
        .is_err(),
        "a board with no review column has none to resolve"
    );
    assert!(
        column_of_type(
            &nook_control::repo::tasks::DbTaskRepository::new(bed.db()),
            no_review,
            "completed"
        )
        .await
        .is_ok(),
        "so submit_pr falls back to the completed column"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn backfill_inserts_one_review_before_completed_and_is_idempotent() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping backfill — no DATABASE_URL");
        return;
    };
    // A pre-review board: Triage/Todo/In Progress/Done at 0..3.
    let board = make_board(&bed).await;
    add_col(&bed, board, "Triage", 0, "backlog").await;
    add_col(&bed, board, "Todo", 1, "unstarted").await;
    add_col(&bed, board, "In Progress", 2, "started").await;
    add_col(&bed, board, "Done", 3, "completed").await;

    run_backfill(&bed, board).await;
    let after = cols(&bed, board).await;
    // Exactly one review column, positioned immediately before completed (AC-2).
    let review: Vec<_> = after.iter().filter(|c| c.2 == "review").collect();
    assert_eq!(review.len(), 1, "exactly one review column");
    assert_eq!(review[0].0, "In Review");
    let order: Vec<&str> = after.iter().map(|c| c.2.as_str()).collect();
    assert_eq!(
        order,
        vec!["backlog", "unstarted", "started", "review", "completed"],
        "In Review sits immediately before Done"
    );

    // Idempotent: a second run adds nothing.
    run_backfill(&bed, board).await;
    let again = cols(&bed, board).await;
    assert_eq!(
        again.iter().filter(|c| c.2 == "review").count(),
        1,
        "re-running the backfill is a no-op"
    );
    assert_eq!(again.len(), after.len(), "no duplicate columns on re-run");

    bed.teardown().await;
}
