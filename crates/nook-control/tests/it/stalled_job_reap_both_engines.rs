//! `reap_stalled_jobs` executes on WHICHEVER engine the bed is running
//! (MAIN-506) — the divergence guard for the one new query.
//!
//! Separate from `job_reaper.rs` deliberately. That binary's helpers age a
//! node's `last_seen_at` with `now() - ($1::bigint * interval '1 second')`,
//! which is Postgres-only, so the whole file is on the SQLite CI allow-list and
//! its coverage stops at the default engine. The stall scan is exactly the kind
//! of query that parses on one engine and dies on the other — a correlated
//! `MAX()` subquery plus a `time_math` cutoff — so it gets a home both legs can
//! run.
//!
//! What keeps this file engine-neutral is that every clock it moves is a BOUND
//! `Timestamptz` rather than an interval expression, and the stall scan never
//! joins `nodes`, so no node helper is needed either.

use nook_control::services::jobs;
use nook_db::{params, Db, Engine};
use nook_types::*;

use nook_testkit::TestBed;

fn ago(secs: i64) -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now() - chrono::Duration::seconds(secs)
}

/// A board + column + task to anchor a job on.
async fn target_task(bed: &TestBed, tenant: TenantId, creator: UserId) -> TaskId {
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
             VALUES ($1,$2,'Triage',0,'unstarted')",
            params![col, board],
        )
        .await
        .expect("column");
    let task = TaskId::new();
    bed.db()
        .exec(
            "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, type, created_by)
             VALUES ($1,$2,$3,$4,'t','task',$5)",
            params![task, tenant, board, col, creator],
        )
        .await
        .expect("task");
    task
}

/// A `build` job in `state`, `secs_ago` old, with `build_outcome` recorded —
/// the CASE the candidate read folds `build_outcome`/`review_verdict` into is
/// as engine-sensitive as the rest of this query (MAIN-607).
async fn concluded_job(
    bed: &TestBed,
    tenant: TenantId,
    user: UserId,
    target: TaskId,
    state: &str,
    secs_ago: i64,
) -> JobId {
    let id = JobId::new();
    bed.db()
        .exec(
            "INSERT INTO loop_jobs
                 (id, tenant_id, kind, target_task_id, requested_by, state, updated_at,
                  build_outcome)
             VALUES ($1,$2,'build',$3,$4,$5,$6,'pr_opened')",
            params![id, tenant, target, user, state, ago(secs_ago)],
        )
        .await
        .expect("concluded job");
    id
}

/// A job in `state` that last changed `secs_ago` seconds ago, with NO executor
/// — the stall scan never joins `nodes`, so it needs none.
async fn job(
    bed: &TestBed,
    tenant: TenantId,
    user: UserId,
    target: TaskId,
    state: &str,
    secs_ago: i64,
) -> JobId {
    let id = JobId::new();
    bed.db()
        .exec(
            "INSERT INTO loop_jobs
                 (id, tenant_id, kind, target_task_id, requested_by, state, updated_at)
             VALUES ($1,$2,'spec',$3,$4,$5,$6)",
            params![id, tenant, target, user, state, ago(secs_ago)],
        )
        .await
        .expect("job");
    id
}

/// One transcript entry `secs_ago` seconds old — a run showing a sign of life.
async fn entry(bed: &TestBed, id: JobId, secs_ago: i64) {
    bed.db()
        .exec(
            "INSERT INTO loop_job_transcript (id, job_id, source, content, at)
             VALUES ($1,$2,'agent','· Bash',$3)",
            params![JobTranscriptId::new(), id, ago(secs_ago)],
        )
        .await
        .expect("transcript entry");
}

async fn job_state(bed: &TestBed, id: JobId) -> String {
    bed.db()
        .query_scalar("SELECT state FROM loop_jobs WHERE id = $1", params![id])
        .await
        .expect("state")
}

#[tokio::test]
async fn the_stall_scan_separates_a_silent_run_from_a_working_one() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("stall").await;
    let (user, _p) = bed.user(tenant, "owner").await;
    let target = target_task(&bed, tenant, user).await;

    // Silent for two hours: nothing since it started.
    let orphan = job(&bed, tenant, user, target, "running", 7_200).await;
    // As old, but still writing — the correlated subquery is the only thing
    // that can tell these two apart.
    let working = job(&bed, tenant, user, target, "running", 7_200).await;
    entry(&bed, working, 60).await;
    // Silent by design, indefinitely.
    let paused = job(&bed, tenant, user, target, "waiting_on_human", 7_200).await;
    // Already over.
    let done = job(&bed, tenant, user, target, "completed", 7_200).await;
    // As silent as the orphan, but it recorded its outcome: concluded, not
    // failed (MAIN-607).
    let recorded = concluded_job(&bed, tenant, user, target, "running", 7_200).await;

    let state = bed.app_state().await;
    let reaped = jobs::reap_stalled_jobs(&state, 3_600)
        .await
        .expect("stall reap");

    assert_eq!(
        reaped, 2,
        "the orphan and the concluded run, and only those"
    );
    assert_eq!(job_state(&bed, orphan).await, "failed");
    assert_eq!(job_state(&bed, recorded).await, "completed");
    assert_eq!(job_state(&bed, working).await, "running");
    assert_eq!(job_state(&bed, paused).await, "waiting_on_human");
    assert_eq!(job_state(&bed, done).await, "completed");

    // And a second replica's scan double-fails nothing.
    assert_eq!(
        jobs::reap_stalled_jobs(&state, 3_600)
            .await
            .expect("second scan"),
        0
    );

    bed.teardown().await;
}

// ── the repair migration (AC-5) ─────────────────────────────────────────────

/// This card's migration, for the engine the bed is on — read from the file the
/// migrator embeds, so the test cannot drift from what a deploy runs.
fn migration_sql(engine: Engine) -> &'static str {
    match engine {
        Engine::Postgres => {
            include_str!("../../migrations/0074_conclude_stalled_recorded_jobs.sql")
        }
        Engine::Sqlite => {
            include_str!("../../migrations_sqlite/0074_conclude_stalled_recorded_jobs.sql")
        }
    }
}

/// Comment lines first: a prose `;` would otherwise split a statement in half.
async fn run_migration(bed: &TestBed) {
    let sql: String = migration_sql(bed.engine())
        .lines()
        .filter(|l| !l.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n");
    for statement in sql.split(';') {
        if statement.trim().is_empty() {
            continue;
        }
        bed.db()
            .exec(statement, params![])
            .await
            .unwrap_or_else(|e| panic!("migration statement failed: {e}\n{statement}"));
    }
}

async fn touched_at(bed: &TestBed, id: JobId) -> chrono::DateTime<chrono::Utc> {
    bed.db()
        .query_scalar(
            "SELECT updated_at FROM loop_jobs WHERE id = $1",
            params![id],
        )
        .await
        .expect("updated_at")
}

#[tokio::test]
async fn the_migration_concludes_the_jobs_this_bug_already_stranded() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("stall").await;
    let (user, _p) = bed.user(tenant, "owner").await;
    let target = target_task(&bed, tenant, user).await;

    // The MAIN-600 shape: outcome recorded, still `running` an hour later.
    let stranded = concluded_job(&bed, tenant, user, target, "running", 7_200).await;
    // Still in flight with nothing recorded — not this migration's business.
    let live = job(&bed, tenant, user, target, "running", 60).await;
    // Already failed by the reaper before this shipped: its card was handed
    // back, and completing the row now would not undo that (NG-7).
    let lost = concluded_job(&bed, tenant, user, target, "failed", 7_200).await;

    run_migration(&bed).await;
    assert_eq!(job_state(&bed, stranded).await, "completed");
    assert_eq!(job_state(&bed, live).await, "running");
    assert_eq!(job_state(&bed, lost).await, "failed");

    // Idempotent: the second run matches nothing, which an unmoved
    // `updated_at` is the evidence for.
    let (settled, untouched) = (
        touched_at(&bed, stranded).await,
        touched_at(&bed, live).await,
    );
    run_migration(&bed).await;
    assert_eq!(job_state(&bed, stranded).await, "completed");
    assert_eq!(touched_at(&bed, stranded).await, settled);
    assert_eq!(touched_at(&bed, live).await, untouched);

    bed.teardown().await;
}
