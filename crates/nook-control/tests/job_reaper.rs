//! Loop-job reaper (MAIN-164): a job whose executor node went dark is failed
//! after the grace window; a paused job and a job on a live node are never
//! touched; the reap is atomic across replicas; a reaped job re-runs. Each test
//! runs on its OWN private database (MAIN-156), so the global scan only ever
//! sees this test's rows.
//!
//! Needs Postgres: `DATABASE_URL` (`NOOK_REQUIRE_DB=1` in the suite).

use nook_control::services::jobs;
use nook_types::*;
use sqlx::PgPool;

use nook_testkit::TestBed;

/// A board + column + a team-visible task to anchor a job on.
async fn target_task(db: &PgPool, tenant: TenantId, creator: UserId) -> TaskId {
    let board = BoardId::new();
    sqlx::query(
        "INSERT INTO boards (id, tenant_id, name, key, provider) VALUES ($1,$2,'b',$3,'local')",
    )
    .bind(board)
    .bind(tenant)
    .bind(format!("B{}", &board.0.simple().to_string()[26..32]).to_uppercase())
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
    let task = TaskId::new();
    sqlx::query(
        "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, type, created_by)
         VALUES ($1,$2,$3,$4,'t','task',$5)",
    )
    .bind(task)
    .bind(tenant)
    .bind(board)
    .bind(col)
    .bind(creator)
    .execute(db)
    .await
    .expect("task");
    task
}

/// A node whose `last_seen_at` is `secs_ago` seconds in the past.
async fn node_seen(db: &PgPool, tenant: TenantId, secs_ago: i64) -> NodeId {
    let id = NodeId::new();
    sqlx::query(
        "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status, last_seen_at)
         VALUES ($1,$2,$3,$4,'online', now() - ($5::bigint * interval '1 second'))",
    )
    .bind(id)
    .bind(tenant)
    .bind(format!("n-{}", id.0.simple()))
    .bind(format!("h-{}", id.0.simple()))
    .bind(secs_ago)
    .execute(db)
    .await
    .expect("node");
    id
}

/// A job on `target`, executed by `node`, in the given lifecycle state.
async fn job(
    db: &PgPool,
    tenant: TenantId,
    user: UserId,
    target: TaskId,
    node: NodeId,
    state: &str,
) -> JobId {
    let id = JobId::new();
    sqlx::query(
        "INSERT INTO loop_jobs (id, tenant_id, kind, target_task_id, requested_by, state, executor_node_id)
         VALUES ($1,$2,'spec',$3,$4,$5,$6)",
    )
    .bind(id)
    .bind(tenant)
    .bind(target)
    .bind(user)
    .bind(state)
    .bind(node)
    .execute(db)
    .await
    .expect("job");
    id
}

async fn job_state(db: &PgPool, id: JobId) -> String {
    sqlx::query_scalar("SELECT state FROM loop_jobs WHERE id = $1")
        .bind(id)
        .fetch_one(db)
        .await
        .expect("state")
}

async fn transcript_text(db: &PgPool, id: JobId) -> String {
    let lines: Vec<String> =
        sqlx::query_scalar("SELECT content FROM loop_job_transcript WHERE job_id = $1 ORDER BY id")
            .bind(id)
            .fetch_all(db)
            .await
            .expect("transcript");
    lines.join("\n")
}

#[tokio::test]
async fn a_claimed_or_running_job_on_a_dead_executor_is_reaped() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("reap").await;
    let (user, _p) = bed.user(tenant, "owner").await;
    let target = target_task(&bed.pool, tenant, user).await;
    // A node last seen 1000s ago — well past the 180s grace.
    let dead = node_seen(&bed.pool, tenant, 1000).await;
    let claimed = job(&bed.pool, tenant, user, target, dead, "claimed").await;
    let running = job(&bed.pool, tenant, user, target, dead, "running").await;
    let state = bed.app_state().await;

    let reaped = jobs::reap_stale_executors(&state, 180).await.expect("reap");
    assert_eq!(reaped, 2, "both the claimed and the running job are reaped");

    for id in [claimed, running] {
        assert_eq!(job_state(&bed.pool, id).await, "failed");
        let t = transcript_text(&bed.pool, id).await;
        assert!(
            t.contains("executor node offline since") && t.contains("reaped after 180s"),
            "the transcript names the cause: {t:?}"
        );
    }

    bed.teardown().await;
}

#[tokio::test]
async fn a_paused_job_is_never_reaped() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("reap").await;
    let (user, _p) = bed.user(tenant, "owner").await;
    let target = target_task(&bed.pool, tenant, user).await;
    let dead = node_seen(&bed.pool, tenant, 1000).await;
    // A paused job on the very same dead node.
    let paused = job(&bed.pool, tenant, user, target, dead, "waiting_on_human").await;
    let state = bed.app_state().await;

    // Even with a zero grace (everything unseen is "stale"), the pause is exempt.
    let reaped = jobs::reap_stale_executors(&state, 0).await.expect("reap");
    assert_eq!(reaped, 0, "waiting_on_human is exempt from reaping (AC-2)");
    assert_eq!(job_state(&bed.pool, paused).await, "waiting_on_human");

    bed.teardown().await;
}

#[tokio::test]
async fn a_job_whose_executor_was_seen_recently_is_untouched() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("reap").await;
    let (user, _p) = bed.user(tenant, "owner").await;
    let target = target_task(&bed.pool, tenant, user).await;
    // Seen just now — well within the grace.
    let live = node_seen(&bed.pool, tenant, 0).await;
    let running = job(&bed.pool, tenant, user, target, live, "running").await;
    let state = bed.app_state().await;

    let reaped = jobs::reap_stale_executors(&state, 180).await.expect("reap");
    assert_eq!(reaped, 0, "a live executor's job is never reaped");
    assert_eq!(job_state(&bed.pool, running).await, "running");

    bed.teardown().await;
}

#[tokio::test]
async fn the_reap_is_atomic_and_loses_a_live_transition_cleanly() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("reap").await;
    let (user, _p) = bed.user(tenant, "owner").await;
    let target = target_task(&bed.pool, tenant, user).await;
    let dead = node_seen(&bed.pool, tenant, 1000).await;
    let state = bed.app_state().await;

    // A job that completes between scan and update falls out of the guard set:
    // the conditional UPDATE (state IN claimed/running) never touches it (AC-5).
    let finished = job(&bed.pool, tenant, user, target, dead, "running").await;
    jobs::transition(&state, tenant, finished, "completed")
        .await
        .expect("complete");

    // And a genuinely stale running job IS reaped — but only once: a second
    // replica's scan finds it already failed and no longer in the guard set.
    let stale = job(&bed.pool, tenant, user, target, dead, "running").await;

    let first = jobs::reap_stale_executors(&state, 0).await.expect("reap 1");
    assert_eq!(first, 1, "only the genuinely-running stale job is reaped");
    let second = jobs::reap_stale_executors(&state, 0).await.expect("reap 2");
    assert_eq!(second, 0, "a second reaper double-fails nothing");

    assert_eq!(
        job_state(&bed.pool, finished).await,
        "completed",
        "untouched"
    );
    assert_eq!(job_state(&bed.pool, stale).await, "failed");

    bed.teardown().await;
}

#[tokio::test]
async fn a_reaped_job_can_be_re_run() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("reap").await;
    let (user, _p) = bed.user(tenant, "owner").await;
    let target = target_task(&bed.pool, tenant, user).await;
    let dead = node_seen(&bed.pool, tenant, 1000).await;
    let original = job(&bed.pool, tenant, user, target, dead, "running").await;
    let state = bed.app_state().await;

    jobs::reap_stale_executors(&state, 180).await.expect("reap");
    assert_eq!(job_state(&bed.pool, original).await, "failed");

    // The existing re-run path forks a fresh queued job linked to its
    // predecessor — the reap did not disturb that lineage (AC-4).
    let fresh = jobs::rerun(&state, tenant, user, original)
        .await
        .expect("a reaped job re-runs");
    assert_eq!(fresh.job.state, "queued");
    assert_eq!(fresh.job.predecessor_job_id, Some(original));

    bed.teardown().await;
}
