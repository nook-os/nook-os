//! Loop jobs core (MAIN-127): lifecycle, enqueue-on-create, cancel, transcript,
//! target validation. Each test runs against its OWN private database (MAIN-156
//! TestBed), so the new `0020_loop_jobs` migration is exercised in isolation and
//! never touches the shared dev ledger.
//!
//! Needs Postgres: set `DATABASE_URL` (`NOOK_REQUIRE_DB=1` in the suite).

use std::time::Duration;

use nook_control::services::jobs;
use nook_control::state::AppState;
use nook_types::*;
use sqlx::PgPool;

use nook_testkit::TestBed;

/// A board with one column to hang tasks on.
async fn board(db: &PgPool, tenant: TenantId) -> (BoardId, ColumnId) {
    let board = BoardId::new();
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
    let col = ColumnId::new();
    sqlx::query(
        "INSERT INTO board_columns (id, board_id, name, position, type)
         VALUES ($1, $2, 'Triage', 0, 'unstarted')",
    )
    .bind(col)
    .bind(board)
    .execute(db)
    .await
    .expect("column");
    (board, col)
}

/// A task of `type_` (e.g. "task" or "epic") in `board`, created by `creator`,
/// optionally in `workspace`. Team-visible so any tenant user may open a job on
/// it.
async fn task(
    db: &PgPool,
    tenant: TenantId,
    board: BoardId,
    col: ColumnId,
    type_: &str,
    creator: UserId,
    workspace: Option<WorkspaceId>,
) -> TaskId {
    let id = TaskId::new();
    sqlx::query(
        "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, type, created_by, workspace_id)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8)",
    )
    .bind(id)
    .bind(tenant)
    .bind(board)
    .bind(col)
    .bind(format!("t-{}", id.0.simple()))
    .bind(type_)
    .bind(creator)
    .bind(workspace)
    .execute(db)
    .await
    .expect("task");
    id
}

/// tenant + owner user + board/column, ready to open jobs against.
async fn fixture(bed: &TestBed) -> (AppState, TenantId, UserId, BoardId, ColumnId) {
    let tenant = bed.tenant("jobs").await;
    let (user, _person) = bed.user(tenant, "owner").await;
    let (b, c) = board(&bed.pool, tenant).await;
    (bed.app_state().await, tenant, user, b, c)
}

#[tokio::test]
async fn create_enqueues_a_work_item_and_records_an_event() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, user, b, c) = fixture(&bed).await;
    let ws = bed.workspace(tenant).await;
    let target = task(&bed.pool, tenant, b, c, "task", user, Some(ws)).await;

    let detail = jobs::create(
        &state,
        tenant,
        user,
        CreateLoopJobRequest {
            kind: "spec".into(),
            target_task_id: target,
        },
    )
    .await
    .expect("create job");

    assert_eq!(detail.job.state, "queued");
    assert_eq!(detail.job.kind, "spec");
    assert_eq!(
        detail.job.workspace_id,
        Some(ws),
        "workspace derived from target"
    );
    assert!(
        detail.transcript.is_empty(),
        "a fresh job has no transcript"
    );

    // AC-2: a `loop.job` work item is on the queue, payload = the job id.
    let claimed = state
        .queue
        .receive(&[jobs::WORK_TYPE.to_string()], 10, Duration::from_secs(30))
        .await
        .expect("receive");
    assert_eq!(claimed.len(), 1, "exactly one work item enqueued");
    let payload_id: JobId =
        serde_json::from_slice(&claimed[0].payload).expect("payload is a job id");
    assert_eq!(payload_id, detail.job.id, "payload names the created job");

    // AC-4: a job.created event was recorded for this tenant.
    let (events,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM events WHERE tenant_id = $1 AND kind = 'job.created'")
            .bind(tenant)
            .fetch_one(&bed.pool)
            .await
            .unwrap();
    assert_eq!(events, 1, "job.created recorded");

    bed.teardown().await;
}

#[tokio::test]
async fn decompose_requires_an_epic_target() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, user, b, c) = fixture(&bed).await;
    let plain = task(&bed.pool, tenant, b, c, "task", user, None).await;
    let epic = task(&bed.pool, tenant, b, c, "epic", user, None).await;

    let err = jobs::create(
        &state,
        tenant,
        user,
        CreateLoopJobRequest {
            kind: "decompose".into(),
            target_task_id: plain,
        },
    )
    .await
    .expect_err("decompose on a non-epic is refused");
    assert!(matches!(err, nook_control::error::ApiError::BadRequest(_)));

    jobs::create(
        &state,
        tenant,
        user,
        CreateLoopJobRequest {
            kind: "decompose".into(),
            target_task_id: epic,
        },
    )
    .await
    .expect("decompose on an epic is allowed");

    // An unknown kind is rejected too.
    let bad = jobs::create(
        &state,
        tenant,
        user,
        CreateLoopJobRequest {
            kind: "translate".into(),
            target_task_id: epic,
        },
    )
    .await
    .expect_err("unknown kind refused");
    assert!(matches!(bad, nook_control::error::ApiError::BadRequest(_)));

    bed.teardown().await;
}

#[tokio::test]
async fn lifecycle_allows_legal_transitions_and_refuses_illegal_ones() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, user, b, c) = fixture(&bed).await;
    let target = task(&bed.pool, tenant, b, c, "task", user, None).await;

    let spec = CreateLoopJobRequest {
        kind: "spec".into(),
        target_task_id: target,
    };

    // The happy path: queued → claimed → running → completed.
    let id = jobs::create(&state, tenant, user, spec.clone())
        .await
        .expect("create")
        .job
        .id;
    for to in ["claimed", "running", "completed"] {
        let j = jobs::transition(&state, tenant, id, to).await.expect(to);
        assert_eq!(j.state, to);
    }
    // A terminal job refuses further transitions.
    let err = jobs::transition(&state, tenant, id, "running")
        .await
        .expect_err("completed is terminal");
    assert!(matches!(err, nook_control::error::ApiError::Conflict(_)));

    // A skip is illegal: queued → completed is not a legal edge.
    let id2 = jobs::create(&state, tenant, user, spec.clone())
        .await
        .expect("create")
        .job
        .id;
    let err = jobs::transition(&state, tenant, id2, "completed")
        .await
        .expect_err("cannot skip to completed");
    assert!(matches!(err, nook_control::error::ApiError::Conflict(_)));

    bed.teardown().await;
}

#[tokio::test]
async fn cancel_works_from_live_states_and_is_refused_once_terminal() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, user, b, c) = fixture(&bed).await;
    let target = task(&bed.pool, tenant, b, c, "task", user, None).await;

    let waiting = jobs::create(
        &state,
        tenant,
        user,
        CreateLoopJobRequest {
            kind: "spec".into(),
            target_task_id: target,
        },
    )
    .await
    .unwrap()
    .job
    .id;
    // Drive it into a live, mid-flight state, then cancel out of it.
    jobs::transition(&state, tenant, waiting, "claimed")
        .await
        .unwrap();
    jobs::transition(&state, tenant, waiting, "running")
        .await
        .unwrap();
    jobs::transition(&state, tenant, waiting, "waiting_on_human")
        .await
        .unwrap();
    let canceled = jobs::cancel(&state, tenant, waiting).await.expect("cancel");
    assert_eq!(canceled.state, "canceled");

    // Cancelling an already-canceled job is a no-op success, not a 409.
    let again = jobs::cancel(&state, tenant, waiting)
        .await
        .expect("idempotent cancel");
    assert_eq!(again.state, "canceled");

    // But a completed job cannot be canceled.
    let done = jobs::create(
        &state,
        tenant,
        user,
        CreateLoopJobRequest {
            kind: "spec".into(),
            target_task_id: target,
        },
    )
    .await
    .unwrap()
    .job
    .id;
    jobs::transition(&state, tenant, done, "claimed")
        .await
        .unwrap();
    jobs::transition(&state, tenant, done, "running")
        .await
        .unwrap();
    jobs::transition(&state, tenant, done, "completed")
        .await
        .unwrap();
    let err = jobs::cancel(&state, tenant, done)
        .await
        .expect_err("completed cannot cancel");
    assert!(matches!(err, nook_control::error::ApiError::Conflict(_)));

    bed.teardown().await;
}

#[tokio::test]
async fn transcript_appends_and_reads_back_in_order() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, user, b, c) = fixture(&bed).await;
    let target = task(&bed.pool, tenant, b, c, "task", user, None).await;
    let id = jobs::create(
        &state,
        tenant,
        user,
        CreateLoopJobRequest {
            kind: "spec".into(),
            target_task_id: target,
        },
    )
    .await
    .unwrap()
    .job
    .id;

    jobs::append_transcript(&state, id, "system", "job started")
        .await
        .unwrap();
    jobs::append_transcript(&state, id, "agent", "thinking...")
        .await
        .unwrap();

    let detail = jobs::get(&state, tenant, id).await.expect("get");
    assert_eq!(detail.transcript.len(), 2);
    assert_eq!(detail.transcript[0].content, "job started");
    assert_eq!(detail.transcript[0].source, "system");
    assert_eq!(detail.transcript[1].content, "thinking...");

    // A job from another tenant is invisible even by id.
    let other = bed.tenant("other").await;
    let err = jobs::get(&state, other, id)
        .await
        .expect_err("cross-tenant read");
    assert!(matches!(err, nook_control::error::ApiError::NotFound));

    bed.teardown().await;
}

#[tokio::test]
async fn rerun_forks_a_fresh_queued_job_linked_to_its_predecessor() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, user, b, c) = fixture(&bed).await;
    let epic = task(&bed.pool, tenant, b, c, "epic", user, None).await;
    let orig = jobs::create(
        &state,
        tenant,
        user,
        CreateLoopJobRequest {
            kind: "decompose".into(),
            target_task_id: epic,
        },
    )
    .await
    .expect("create")
    .job
    .id;

    // A live job cannot be re-run.
    let err = jobs::rerun(&state, tenant, user, orig)
        .await
        .expect_err("live job");
    assert!(matches!(err, nook_control::error::ApiError::Conflict(_)));

    // Fail it, then re-run: a NEW job, queued, pointing back at the original.
    jobs::transition(&state, tenant, orig, "claimed")
        .await
        .unwrap();
    jobs::transition(&state, tenant, orig, "failed")
        .await
        .unwrap();
    let fresh = jobs::rerun(&state, tenant, user, orig)
        .await
        .expect("rerun");
    assert_ne!(fresh.job.id, orig, "a re-run is a new row");
    assert_eq!(fresh.job.state, "queued");
    assert_eq!(
        fresh.job.predecessor_job_id,
        Some(orig),
        "links to predecessor"
    );
    assert_eq!(fresh.job.kind, "decompose");

    bed.teardown().await;
}
