//! The board-signal review sweep and manual enqueue (MAIN-408).
//!
//! Each test runs against its OWN private database (TestBed), so the sweep is
//! exercised in isolation and never sees another test's boards.
//!
//! The set is built around the thing AC-3 actually asks for. It is easy to
//! write two tests that each prove their own path dedupes and to believe that
//! covers it — it does not, because two paths with two *separate* rules would
//! also pass both. What proves ONE rule is the CROSS pair: a job raised by the
//! sweep must block the manual path, and a job raised manually must block the
//! sweep. Those two tests are the point of this file; the rest are the ACs.
//!
//! Needs Postgres: set `DATABASE_URL` (`NOOK_REQUIRE_DB=1` in the suite).

use nook_control::services::{jobs, review_sweep};
use nook_control::state::AppState;
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;

/// A board with one `review`-type column and one `unstarted` column, so a test
/// can put a card on either side of the signal.
async fn board(bed: &TestBed, tenant: TenantId) -> (BoardId, ColumnId, ColumnId) {
    let board = BoardId::new();
    bed.db()
        .exec(
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

    let todo = ColumnId::new();
    bed.db()
        .exec(
            "INSERT INTO board_columns (id, board_id, name, position, type)
             VALUES ($1, $2, 'Todo', 0, 'unstarted')",
            params![todo, board],
        )
        .await
        .expect("todo column");

    let review = ColumnId::new();
    bed.db()
        .exec(
            "INSERT INTO board_columns (id, board_id, name, position, type)
             VALUES ($1, $2, 'In Review', 1, 'review')",
            params![review, board],
        )
        .await
        .expect("review column");

    (board, todo, review)
}

/// A team-visible card in `col`, attached to `workspace`.
async fn card(
    bed: &TestBed,
    tenant: TenantId,
    board: BoardId,
    col: ColumnId,
    creator: UserId,
    workspace: WorkspaceId,
) -> TaskId {
    let id = TaskId::new();
    bed.db()
        .exec(
            "INSERT INTO tasks
                (id, tenant_id, board_id, column_id, title, type, visibility,
                 created_by, workspace_id)
             VALUES ($1,$2,$3,$4,'card','task','team',$5,$6)",
            params![id, tenant, board, col, creator, workspace.0],
        )
        .await
        .expect("card");
    id
}

/// How many review jobs exist for a workspace, in any state — the number AC-4
/// is about. Counted straight from the table rather than through the dedupe, so
/// a bug IN the dedupe cannot hide itself from this assertion.
async fn review_count(bed: &TestBed, workspace: WorkspaceId) -> i64 {
    bed.db()
        .query_scalar_opt::<i64>(
            "SELECT COUNT(*) FROM loop_jobs WHERE kind = 'review' AND workspace_id = $1",
            params![workspace.0],
        )
        .await
        .expect("count")
        .unwrap_or(0)
}

/// A tenant with an owner, a workspace, a board, and a card sitting in review —
/// the full signal, with the sweep still OFF.
async fn signalling(bed: &TestBed, hint: &str) -> (AppState, TenantId, WorkspaceId, UserId) {
    let state = bed.app_state().await;
    let tenant = bed.tenant(hint).await;
    let (owner, _) = bed.user(tenant, "owner").await;
    let workspace = bed.workspace(tenant).await;
    let (b, _todo, review) = board(bed, tenant).await;
    card(bed, tenant, b, review, owner, workspace).await;
    (state, tenant, workspace, owner)
}

// ── AC-1: the switch ────────────────────────────────────────────────────────

/// Verify step 1: setting off → no jobs enqueued, ever.
///
/// The signal is fully present — a card really is in a review column — so this
/// fails if the sweep reads the board without consulting the setting.
#[tokio::test]
async fn with_the_sweep_off_a_signalling_board_raises_nothing() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, _tenant, workspace, _owner) = signalling(&bed, "m408off").await;

    assert_eq!(review_sweep::sweep(&state).await.expect("sweep"), 0);
    assert_eq!(
        review_count(&bed, workspace).await,
        0,
        "the sweep raised a job with its setting absent (default off)"
    );

    bed.teardown().await;
}

/// Verify step 2: setting on + a qualifying signal → exactly one job.
#[tokio::test]
async fn with_the_sweep_on_a_signalling_board_raises_exactly_one() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, workspace, _owner) = signalling(&bed, "m408on").await;
    review_sweep::set(&*state.settings, tenant, true)
        .await
        .expect("enable");

    assert_eq!(review_sweep::sweep(&state).await.expect("sweep"), 1);
    assert_eq!(review_count(&bed, workspace).await, 1);

    bed.teardown().await;
}

/// A board with NO card in a review column is not a signal, however many cards
/// it has elsewhere. Without this, "raises exactly one" above would also pass
/// for a sweep that reviews every workspace it can see.
#[tokio::test]
async fn a_board_with_no_card_in_review_is_not_a_signal() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("m408nosig").await;
    let (owner, _) = bed.user(tenant, "owner").await;
    let workspace = bed.workspace(tenant).await;
    let (b, todo, _review) = board(&bed, tenant).await;
    card(&bed, tenant, b, todo, owner, workspace).await;
    review_sweep::set(&*state.settings, tenant, true)
        .await
        .expect("enable");

    assert_eq!(review_sweep::sweep(&state).await.expect("sweep"), 0);
    assert_eq!(review_count(&bed, workspace).await, 0);

    bed.teardown().await;
}

// ── AC-4: safe to leave on ──────────────────────────────────────────────────

/// No growth in queued jobs when nothing changes. Ten passes over an unchanged
/// board must leave exactly the one job the first pass raised.
#[tokio::test]
async fn repeated_sweeps_over_an_unchanged_board_do_not_grow_the_queue() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, workspace, _owner) = signalling(&bed, "m408idem").await;
    review_sweep::set(&*state.settings, tenant, true)
        .await
        .expect("enable");

    assert_eq!(review_sweep::sweep(&state).await.expect("first"), 1);
    for _ in 0..9 {
        assert_eq!(
            review_sweep::sweep(&state).await.expect("again"),
            0,
            "a later sweep raised a second review for an unchanged board"
        );
    }
    assert_eq!(review_count(&bed, workspace).await, 1);

    bed.teardown().await;
}

/// A job already RUNNING is never re-enqueued. This is the half of AC-4 that a
/// `queued`-only dedupe would silently fail: the moment an executor claims the
/// job, a rule looking for `state = 'queued'` sees nothing and raises another.
#[tokio::test]
async fn a_running_review_is_never_re_enqueued() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, workspace, _owner) = signalling(&bed, "m408running").await;
    review_sweep::set(&*state.settings, tenant, true)
        .await
        .expect("enable");
    assert_eq!(review_sweep::sweep(&state).await.expect("first"), 1);

    // Drive it out of `queued` the way an executor would.
    for to in ["claimed", "running"] {
        bed.db()
            .exec(
                "UPDATE loop_jobs SET state = $2 WHERE kind = 'review' AND workspace_id = $1",
                params![workspace.0, to],
            )
            .await
            .expect("advance");
        assert_eq!(
            review_sweep::sweep(&state).await.expect("sweep"),
            0,
            "the sweep re-enqueued a review while one was {to}"
        );
    }
    assert_eq!(review_count(&bed, workspace).await, 1);

    bed.teardown().await;
}

// ── AC-3: ONE dedupe rule, proven across the two paths ──────────────────────

/// Verify step 3, and the first half of the cross-path proof: a review raised
/// by the SWEEP blocks the MANUAL path.
///
/// `enqueue_review` returning `None` is the dedupe firing; the count confirms
/// nothing was written behind it.
#[tokio::test]
async fn a_sweep_raised_review_blocks_the_manual_path() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, workspace, owner) = signalling(&bed, "m408cross1").await;
    review_sweep::set(&*state.settings, tenant, true)
        .await
        .expect("enable");
    assert_eq!(review_sweep::sweep(&state).await.expect("sweep"), 1);

    let manual = jobs::enqueue_review(&state, tenant, owner, workspace, None)
        .await
        .expect("manual enqueue");
    assert!(
        manual.is_none(),
        "the manual path raised a second review over the sweep's — the two paths \
         are not sharing one dedupe rule"
    );
    assert_eq!(review_count(&bed, workspace).await, 1);

    bed.teardown().await;
}

/// The other half: a review raised MANUALLY blocks the SWEEP. Asymmetry here
/// would mean two rules that merely agree in one direction.
///
/// Note the sweep is enabled but the board signal is present too — so the only
/// thing stopping a second job is the dedupe, not a missing signal.
#[tokio::test]
async fn a_manually_raised_review_blocks_the_sweep() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, workspace, owner) = signalling(&bed, "m408cross2").await;
    review_sweep::set(&*state.settings, tenant, true)
        .await
        .expect("enable");

    let manual = jobs::enqueue_review(&state, tenant, owner, workspace, None)
        .await
        .expect("manual enqueue");
    assert!(manual.is_some(), "the manual path raised nothing");

    assert_eq!(
        review_sweep::sweep(&state).await.expect("sweep"),
        0,
        "the sweep raised a second review over a manually raised one"
    );
    assert_eq!(review_count(&bed, workspace).await, 1);

    bed.teardown().await;
}

/// A FINISHED review does not block the next one — the dedupe must key on "in
/// flight", not "has ever existed", or a workspace is reviewable exactly once
/// for the lifetime of the deployment.
#[tokio::test]
async fn a_completed_review_does_not_block_the_next_one() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, workspace, owner) = signalling(&bed, "m408done").await;

    let first = jobs::enqueue_review(&state, tenant, owner, workspace, None)
        .await
        .expect("first");
    assert!(first.is_some());

    bed.db()
        .exec(
            "UPDATE loop_jobs SET state = 'completed'
              WHERE kind = 'review' AND workspace_id = $1",
            params![workspace.0],
        )
        .await
        .expect("complete it");

    let second = jobs::enqueue_review(&state, tenant, owner, workspace, None)
        .await
        .expect("second");
    assert!(
        second.is_some(),
        "a completed review blocked the next one — the dedupe is keying on \
         existence rather than on being in flight"
    );
    assert_eq!(review_count(&bed, workspace).await, 2);

    bed.teardown().await;
}

// ── The visibility ruling (MAIN-408, Ryan 2026-08-06) ───────────────────────

/// A review job is TENANT-VISIBLE: a member who is neither its requester nor
/// any card's owner can read it and its transcript.
///
/// This pins the ruling rather than the implementation. If someone later scopes
/// review jobs to a workspace predicate, this test fails and they are forced to
/// come back to the card — which is the point, because the cost of the loose
/// rule was accepted knowingly and reversing it is a decision, not a refactor.
#[tokio::test]
async fn a_review_job_is_visible_to_any_tenant_member() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, workspace, owner) = signalling(&bed, "m408vis").await;
    let (stranger, _) = bed.user(tenant, "member").await;

    let raised = jobs::enqueue_review(&state, tenant, owner, workspace, None)
        .await
        .expect("raise")
        .expect("a job");

    let seen = jobs::get(&state, tenant, stranger, raised.job.id)
        .await
        .expect("a tenant member must be able to read a review job");
    assert_eq!(seen.job.id, raised.job.id);
    assert!(
        seen.job.target_task_id.is_none(),
        "a review job must carry no ticket"
    );
    assert_eq!(seen.job.workspace_id, Some(workspace));

    bed.teardown().await;
}
