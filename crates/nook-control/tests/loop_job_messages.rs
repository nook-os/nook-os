//! The input half of the loop (MAIN-231): a job's SEED — the general idea a run
//! starts from — and human → agent STEERING MESSAGES sent to a live run.
//!
//! What is proven here: the seed round-trips (row → opening `human` transcript
//! line → the `RunLoopJob` the executor receives); a steering message appends,
//! is delivered to the executor, and resumes a paused run; a caller who cannot
//! see the target card cannot message the job; and a finished job refuses
//! messages outright. Every row is created by the test, on its OWN private
//! database (MAIN-156 TestBed).
//!
//! Needs Postgres: `DATABASE_URL` (`NOOK_REQUIRE_DB=1` in the suite).

use nook_control::error::ApiError;
use nook_control::services::jobs;
use nook_control::ws::registry::NodeHandle;
use nook_db::{params, Db};
use nook_types::*;
use sqlx::PgPool;
use tokio::sync::mpsc;
use uuid::Uuid;

use nook_testkit::TestBed;

/// A board (with a known key) + one column to hang tasks on.
async fn board(db: &PgPool, tenant: TenantId) -> (BoardId, ColumnId) {
    let board = BoardId::new();
    sqlx::query(
        "INSERT INTO boards (id, tenant_id, name, key, provider) VALUES ($1,$2,'b',$3,'local')",
    )
    .bind(board)
    .bind(tenant)
    // v7 ids share a timestamp prefix, so key off the random tail.
    .bind(format!("K{}", &board.0.simple().to_string()[26..]).to_uppercase())
    .execute(db)
    .await
    .expect("board");
    let col = ColumnId::new();
    sqlx::query(
        "INSERT INTO board_columns (id, board_id, name, position, type)
         VALUES ($1,$2,'Triage',0,'unstarted')",
    )
    .bind(col)
    .bind(board)
    .execute(db)
    .await
    .expect("column");
    (board, col)
}

/// A task created by `creator` with the given `visibility` ("team"/"private").
async fn task(
    db: &PgPool,
    tenant: TenantId,
    board: BoardId,
    col: ColumnId,
    creator: UserId,
    workspace: Option<WorkspaceId>,
    visibility: &str,
) -> TaskId {
    let id = TaskId::new();
    sqlx::query(
        "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, type, number,
                            created_by, workspace_id, visibility)
         VALUES ($1,$2,$3,$4,'t','task',7,$5,$6,$7)",
    )
    .bind(id)
    .bind(tenant)
    .bind(board)
    .bind(col)
    .bind(creator)
    .bind(workspace)
    .bind(visibility)
    .execute(db)
    .await
    .expect("task");
    id
}

async fn node(db: &PgPool, tenant: TenantId) -> NodeId {
    let id = NodeId::new();
    sqlx::query(
        "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status)
         VALUES ($1,$2,$3,$4,'online')",
    )
    .bind(id)
    .bind(tenant)
    .bind(format!("n-{}", id.0.simple()))
    .bind(format!("h-{}", id.0.simple()))
    .execute(db)
    .await
    .expect("node");
    id
}

/// A `node_workspaces` row carrying a clonable remote, so dispatch can resolve.
async fn node_workspace(db: &PgPool, tenant: TenantId, node: NodeId, ws: WorkspaceId) {
    sqlx::query(
        "INSERT INTO node_workspaces (id, tenant_id, node_id, workspace_id, path,
                                      git_remote_url, git_branch)
         VALUES ($1,$2,$3,$4,$5,'git@example.test:acme/repo.git','main')",
    )
    .bind(Uuid::now_v7())
    .bind(tenant)
    .bind(node)
    .bind(ws)
    .bind(format!("/checkouts/{}", ws.0.simple()))
    .execute(db)
    .await
    .expect("node_workspace");
}

async fn load(db: &PgPool, id: JobId) -> LoopJob {
    // Through the `Db` surface, not raw sqlx: row mapping is `FromDbRow` since
    // MAIN-327, and a DTO no longer implements sqlx's `FromRow` at all.
    db.query_one("SELECT * FROM loop_jobs WHERE id = $1", params![id])
        .await
        .expect("load job")
}

/// Every transcript line on a job, oldest first, as `(source, content)`.
async fn transcript(db: &PgPool, id: JobId) -> Vec<(String, String)> {
    sqlx::query_as("SELECT source, content FROM loop_job_transcript WHERE job_id = $1 ORDER BY id")
        .bind(id)
        .fetch_all(db)
        .await
        .expect("transcript")
}

/// Force a job into `state` with `executor` — the shortcut past the lifecycle so
/// each test starts where it means to.
async fn put_in_state(db: &PgPool, id: JobId, state: &str, executor: Option<NodeId>) {
    sqlx::query("UPDATE loop_jobs SET state = $2, executor_node_id = $3 WHERE id = $1")
        .bind(id)
        .bind(state)
        .bind(executor)
        .execute(db)
        .await
        .expect("set state");
}

/// AC-1: the seed is stored on the job, opens the transcript as the human line
/// it is, and rides `RunLoopJob` into the executor's session.
#[tokio::test]
async fn a_seed_round_trips_to_the_transcript_and_the_run() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("jobmsg").await;
    let (user, _person) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let (b, c) = board(&bed.pool, tenant).await;
    let target = task(&bed.pool, tenant, b, c, user, Some(ws), "team").await;
    let n = node(&bed.pool, tenant).await;
    node_workspace(&bed.pool, tenant, n, ws).await;
    let state = bed.app_state().await;

    let detail = jobs::create(
        &state,
        tenant,
        user,
        CreateLoopJobRequest {
            kind: "spec".into(),
            target_task_id: target.to_string(),
            seed: Some("  focus on the migration path, not the UI  ".into()),
        },
    )
    .await
    .expect("create seeded job");

    // Stored, trimmed, on the row…
    assert_eq!(
        detail.job.seed.as_deref(),
        Some("focus on the migration path, not the UI"),
        "the seed is stored on the job, trimmed"
    );
    // …and it opens the transcript as a human line (AC-1/AC-4).
    assert_eq!(
        detail.transcript.len(),
        1,
        "the seed is the only opening line"
    );
    assert_eq!(detail.transcript[0].source, "human");
    assert_eq!(
        detail.transcript[0].content,
        "focus on the migration path, not the UI"
    );

    // It reaches the session: dispatch carries it on the run message.
    put_in_state(&bed.pool, detail.job.id, "claimed", Some(n)).await;
    let (tx, mut rx) = mpsc::channel(4);
    state.registry.register_node(
        n,
        NodeHandle {
            tenant_id: tenant,
            tx,
        },
    );
    jobs::dispatch_to_node(&state, tenant, &load(&bed.pool, detail.job.id).await)
        .await
        .expect("dispatch");
    match rx.try_recv().expect("a RunLoopJob was sent") {
        nook_proto::ControlToNode::RunLoopJob { seed, .. } => assert_eq!(
            seed.as_deref(),
            Some("focus on the migration path, not the UI"),
            "the run carries the brief"
        ),
        other => panic!("expected RunLoopJob, got {other:?}"),
    }

    bed.teardown().await;
}

/// A job opened without a seed is exactly what it was before: no seed, no
/// opening line, nothing on the run message.
#[tokio::test]
async fn no_seed_leaves_the_job_and_its_transcript_untouched() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("jobmsg").await;
    let (user, _person) = bed.user(tenant, "owner").await;
    let (b, c) = board(&bed.pool, tenant).await;
    let target = task(&bed.pool, tenant, b, c, user, None, "team").await;
    let state = bed.app_state().await;

    let detail = jobs::create(
        &state,
        tenant,
        user,
        CreateLoopJobRequest {
            kind: "spec".into(),
            target_task_id: target.to_string(),
            // Whitespace is the same as absent.
            seed: Some("   ".into()),
        },
    )
    .await
    .expect("create");

    assert_eq!(detail.job.seed, None, "a blank seed is no seed");
    assert!(detail.transcript.is_empty(), "no opening line");

    bed.teardown().await;
}

/// AC-2/AC-3, the load-bearing one: a steering message appends to the
/// transcript, is delivered to the executor, and resumes a paused run.
#[tokio::test]
async fn a_steering_message_appends_delivers_and_resumes_a_paused_job() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("jobmsg").await;
    let (user, _person) = bed.user(tenant, "owner").await;
    let (b, c) = board(&bed.pool, tenant).await;
    let target = task(&bed.pool, tenant, b, c, user, None, "team").await;
    let n = node(&bed.pool, tenant).await;
    let state = bed.app_state().await;

    let job = jobs::create(
        &state,
        tenant,
        user,
        CreateLoopJobRequest {
            kind: "spec".into(),
            target_task_id: target.to_string(),
            seed: None,
        },
    )
    .await
    .expect("create")
    .job;

    // The run is up and paused on a human.
    put_in_state(&bed.pool, job.id, "waiting_on_human", Some(n)).await;
    let (tx, mut rx) = mpsc::channel(4);
    state.registry.register_node(
        n,
        NodeHandle {
            tenant_id: tenant,
            tx,
        },
    );

    let entry = jobs::post_message(&state, tenant, user, job.id, "  actually, skip the CLI  ")
        .await
        .expect("message accepted");

    // Appended as a human line, trimmed.
    assert_eq!(entry.source, "human");
    assert_eq!(entry.content, "actually, skip the CLI");
    let lines = transcript(&bed.pool, job.id).await;
    assert_eq!(
        lines,
        vec![("human".to_string(), "actually, skip the CLI".to_string())],
        "durable and ordered, with nothing else invented"
    );

    // Delivered to the executor for the live session.
    match rx.try_recv().expect("a JobMessage was sent") {
        nook_proto::ControlToNode::JobMessage { job_id, body } => {
            assert_eq!(job_id, job.id.0.to_string());
            assert_eq!(body, "actually, skip the CLI");
        }
        other => panic!("expected JobMessage, got {other:?}"),
    }

    // And the paused run is running again.
    assert_eq!(
        load(&bed.pool, job.id).await.state,
        "running",
        "unsolicited input resumes the run, exactly like an answer"
    );

    bed.teardown().await;
}

/// An executor that is not connected cannot have taken the message — say so on
/// the transcript rather than letting "sent" read as "the agent saw it".
#[tokio::test]
async fn an_offline_executor_is_recorded_not_hidden() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("jobmsg").await;
    let (user, _person) = bed.user(tenant, "owner").await;
    let (b, c) = board(&bed.pool, tenant).await;
    let target = task(&bed.pool, tenant, b, c, user, None, "team").await;
    let n = node(&bed.pool, tenant).await;
    let state = bed.app_state().await;

    let job = jobs::create(
        &state,
        tenant,
        user,
        CreateLoopJobRequest {
            kind: "spec".into(),
            target_task_id: target.to_string(),
            seed: None,
        },
    )
    .await
    .expect("create")
    .job;
    // Placed on a node, but nothing is registered for it — no live channel.
    put_in_state(&bed.pool, job.id, "running", Some(n)).await;

    jobs::post_message(&state, tenant, user, job.id, "try the other branch")
        .await
        .expect("message accepted");

    let lines = transcript(&bed.pool, job.id).await;
    assert_eq!(lines.len(), 2, "the message and the honesty about delivery");
    assert_eq!(lines[0].0, "human");
    assert_eq!(lines[1].0, "system");
    assert!(
        lines[1].1.contains("offline"),
        "the system line names the undelivered push: {}",
        lines[1].1
    );

    bed.teardown().await;
}

/// AC-2 authorization: the job's subject visibility governs messaging, exactly
/// as it governs answering an ask. A member who cannot see the private card gets
/// `NotFound` — the job's existence never leaks.
#[tokio::test]
async fn a_member_who_cannot_see_the_card_cannot_message_the_job() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("jobmsg").await;
    let (owner, _p1) = bed.user(tenant, "owner").await;
    // A plain member of the same tenant — no reach into a private card.
    let (stranger, _p2) = bed.user(tenant, "member").await;
    let (b, c) = board(&bed.pool, tenant).await;
    let target = task(&bed.pool, tenant, b, c, owner, None, "private").await;
    let state = bed.app_state().await;

    let job = jobs::create(
        &state,
        tenant,
        owner,
        CreateLoopJobRequest {
            kind: "spec".into(),
            target_task_id: target.to_string(),
            seed: None,
        },
    )
    .await
    .expect("create")
    .job;
    put_in_state(&bed.pool, job.id, "running", None).await;

    let err = jobs::post_message(&state, tenant, stranger, job.id, "let me in")
        .await
        .expect_err("a stranger cannot steer a private card's job");
    assert!(
        matches!(err, ApiError::NotFound),
        "refused as NotFound, not Forbidden: {err:?}"
    );
    assert!(
        transcript(&bed.pool, job.id).await.is_empty(),
        "the refused message left no trace on the transcript"
    );

    // The owner of the card still can.
    jobs::post_message(&state, tenant, owner, job.id, "carry on")
        .await
        .expect("the card's owner may steer their own run");

    bed.teardown().await;
}

/// AC-3: a finished job has no session to steer — refuse with the reason rather
/// than appending to a transcript nothing will read.
#[tokio::test]
async fn a_finished_job_refuses_messages() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("jobmsg").await;
    let (user, _person) = bed.user(tenant, "owner").await;
    let (b, c) = board(&bed.pool, tenant).await;
    let target = task(&bed.pool, tenant, b, c, user, None, "team").await;
    let state = bed.app_state().await;

    for terminal in ["completed", "failed", "canceled"] {
        let job = jobs::create(
            &state,
            tenant,
            user,
            CreateLoopJobRequest {
                kind: "spec".into(),
                target_task_id: target.to_string(),
                seed: None,
            },
        )
        .await
        .expect("create")
        .job;
        put_in_state(&bed.pool, job.id, terminal, None).await;

        let err = jobs::post_message(&state, tenant, user, job.id, "one more thing")
            .await
            .expect_err("a terminal job takes no messages");
        match err {
            ApiError::Conflict(m) => assert!(
                m.contains(terminal),
                "the refusal names the state ({terminal}): {m}"
            ),
            other => panic!("expected Conflict for {terminal}, got {other:?}"),
        }
        assert!(
            transcript(&bed.pool, job.id).await.is_empty(),
            "nothing appended to a {terminal} job"
        );
    }

    // An empty message is refused too, before anything is written.
    let job = jobs::create(
        &state,
        tenant,
        user,
        CreateLoopJobRequest {
            kind: "spec".into(),
            target_task_id: target.to_string(),
            seed: None,
        },
    )
    .await
    .expect("create")
    .job;
    let err = jobs::post_message(&state, tenant, user, job.id, "   ")
        .await
        .expect_err("an empty message is refused");
    assert!(matches!(err, ApiError::BadRequest(_)), "{err:?}");

    bed.teardown().await;
}
