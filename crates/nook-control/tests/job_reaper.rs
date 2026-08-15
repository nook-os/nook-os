//! Loop-job reaper (MAIN-164): a job whose executor node went dark is failed
//! after the grace window; a paused job and a job on a live node are never
//! touched; the reap is atomic across replicas; a reaped job re-runs. Each test
//! runs on its OWN private database (MAIN-156), so the global scan only ever
//! sees this test's rows.
//!
//! Needs Postgres: `DATABASE_URL` (`NOOK_REQUIRE_DB=1` in the suite).

use nook_control::services::jobs;
use nook_db::dialect::{time_math, type_mapping};
use nook_db::{params, Db};
use nook_types::*;

use nook_testkit::TestBed;

/// A board + column + a team-visible task to anchor a job on.
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

/// `now()` minus the bound seconds at `placeholder`, in this bed's dialect —
/// the one expression all three fixtures below age a row with. Not named
/// `secs_ago`: each of them binds a local of that name, which would shadow it.
fn aged(bed: &TestBed, placeholder: &str) -> String {
    time_math(bed.engine()).now_minus_scaled(
        &type_mapping(bed.engine()).cast(placeholder, "bigint"),
        "1 second",
    )
}

/// A node whose `last_seen_at` is `secs_ago` seconds in the past.
async fn node_seen(bed: &TestBed, tenant: TenantId, secs_ago: i64) -> NodeId {
    let id = NodeId::new();
    let ago = aged(bed, "$5");
    bed.db()
        .exec(
            &format!(
                "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status, last_seen_at)
         VALUES ($1,$2,$3,$4,'online', {ago})"
            ),
            params![
                id,
                tenant,
                format!("n-{}", id.0.simple()),
                format!("h-{}", id.0.simple()),
                secs_ago
            ],
        )
        .await
        .expect("node");
    id
}

/// Age a job's `updated_at`, so a scan measuring silence has something to see.
async fn last_touched(bed: &TestBed, id: JobId, secs_ago: i64) {
    let ago = aged(bed, "$2");
    bed.db()
        .exec(
            &format!("UPDATE loop_jobs SET updated_at = {ago} WHERE id = $1"),
            params![id, secs_ago],
        )
        .await
        .expect("age the job");
}

/// One transcript entry `secs_ago` seconds old — a run showing a sign of life.
async fn entry(bed: &TestBed, id: JobId, secs_ago: i64) {
    let ago = aged(bed, "$3");
    bed.db()
        .exec(
            &format!(
                "INSERT INTO loop_job_transcript (id, job_id, source, content, at)
             VALUES ($1,$2,'agent','· Bash', {ago})"
            ),
            params![JobTranscriptId::new(), id, secs_ago],
        )
        .await
        .expect("transcript entry");
}

/// A job on `target`, executed by `node`, in the given lifecycle state.
async fn job(
    bed: &TestBed,
    tenant: TenantId,
    user: UserId,
    target: TaskId,
    node: NodeId,
    state: &str,
) -> JobId {
    let id = JobId::new();
    bed.db()
        .exec(
            "INSERT INTO loop_jobs (id, tenant_id, kind, target_task_id, requested_by, state, executor_node_id)
         VALUES ($1,$2,'spec',$3,$4,$5,$6)",
            params![id, tenant, target, user, state, node],
        )
        .await
        .expect("job");
    id
}

/// A `build` job — the conclusion test reads a different column per kind, so
/// the `spec` rows above cannot express a concluded run.
async fn build_job(
    bed: &TestBed,
    tenant: TenantId,
    user: UserId,
    target: TaskId,
    node: NodeId,
    state: &str,
) -> JobId {
    let id = JobId::new();
    bed.db()
        .exec(
            "INSERT INTO loop_jobs (id, tenant_id, kind, target_task_id, requested_by, state, executor_node_id)
         VALUES ($1,$2,'build',$3,$4,$5,$6)",
            params![id, tenant, target, user, state, node],
        )
        .await
        .expect("build job");
    id
}

/// A `review` job: a workspace and no ticket, which is what
/// `loop_jobs_target_check` requires of this kind.
async fn review_job(
    bed: &TestBed,
    tenant: TenantId,
    user: UserId,
    workspace: WorkspaceId,
    node: NodeId,
    state: &str,
) -> JobId {
    let id = JobId::new();
    bed.db()
        .exec(
            "INSERT INTO loop_jobs (id, tenant_id, kind, workspace_id, requested_by, state, executor_node_id)
         VALUES ($1,$2,'review',$3,$4,$5,$6)",
            params![id, tenant, workspace, user, state, node],
        )
        .await
        .expect("review job");
    id
}

/// Record a run's conclusion the way the outcome call does: the column, and
/// deliberately NOT `state` (NG-4) — which is the shape this whole card is about.
async fn record_conclusion(bed: &TestBed, id: JobId, column: &str, value: &str) {
    bed.db()
        .exec(
            &format!("UPDATE loop_jobs SET {column} = $2 WHERE id = $1"),
            params![id, value],
        )
        .await
        .expect("record the conclusion");
}

async fn job_state(bed: &TestBed, id: JobId) -> String {
    bed.db()
        .query_scalar("SELECT state FROM loop_jobs WHERE id = $1", params![id])
        .await
        .expect("state")
}

async fn transcript_text(bed: &TestBed, id: JobId) -> String {
    let lines: Vec<String> = bed
        .db()
        .query_scalar_all(
            "SELECT content FROM loop_job_transcript WHERE job_id = $1 ORDER BY id",
            params![id],
        )
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
    let target = target_task(&bed, tenant, user).await;
    // A node last seen 1000s ago — well past the 180s grace.
    let dead = node_seen(&bed, tenant, 1000).await;
    let claimed = job(&bed, tenant, user, target, dead, "claimed").await;
    let running = job(&bed, tenant, user, target, dead, "running").await;
    let state = bed.app_state().await;

    let reaped = jobs::reap_stale_executors(&state, 180).await.expect("reap");
    assert_eq!(reaped, 2, "both the claimed and the running job are reaped");

    for id in [claimed, running] {
        assert_eq!(job_state(&bed, id).await, "failed");
        let t = transcript_text(&bed, id).await;
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
    let target = target_task(&bed, tenant, user).await;
    let dead = node_seen(&bed, tenant, 1000).await;
    // A paused job on the very same dead node.
    let paused = job(&bed, tenant, user, target, dead, "waiting_on_human").await;
    let state = bed.app_state().await;

    // Even with a zero grace (everything unseen is "stale"), the pause is exempt.
    let reaped = jobs::reap_stale_executors(&state, 0).await.expect("reap");
    assert_eq!(reaped, 0, "waiting_on_human is exempt from reaping (AC-2)");
    assert_eq!(job_state(&bed, paused).await, "waiting_on_human");

    bed.teardown().await;
}

#[tokio::test]
async fn a_job_whose_executor_was_seen_recently_is_untouched() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("reap").await;
    let (user, _p) = bed.user(tenant, "owner").await;
    let target = target_task(&bed, tenant, user).await;
    // Seen just now — well within the grace.
    let live = node_seen(&bed, tenant, 0).await;
    let running = job(&bed, tenant, user, target, live, "running").await;
    let state = bed.app_state().await;

    let reaped = jobs::reap_stale_executors(&state, 180).await.expect("reap");
    assert_eq!(reaped, 0, "a live executor's job is never reaped");
    assert_eq!(job_state(&bed, running).await, "running");

    bed.teardown().await;
}

#[tokio::test]
async fn the_reap_is_atomic_and_loses_a_live_transition_cleanly() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("reap").await;
    let (user, _p) = bed.user(tenant, "owner").await;
    let target = target_task(&bed, tenant, user).await;
    let dead = node_seen(&bed, tenant, 1000).await;
    let state = bed.app_state().await;

    // A job that completes between scan and update falls out of the guard set:
    // the conditional UPDATE (state IN claimed/running) never touches it (AC-5).
    let finished = job(&bed, tenant, user, target, dead, "running").await;
    jobs::transition(&state, tenant, finished, "completed")
        .await
        .expect("complete");

    // And a genuinely stale running job IS reaped — but only once: a second
    // replica's scan finds it already failed and no longer in the guard set.
    let stale = job(&bed, tenant, user, target, dead, "running").await;

    let first = jobs::reap_stale_executors(&state, 0).await.expect("reap 1");
    assert_eq!(first, 1, "only the genuinely-running stale job is reaped");
    let second = jobs::reap_stale_executors(&state, 0).await.expect("reap 2");
    assert_eq!(second, 0, "a second reaper double-fails nothing");

    assert_eq!(job_state(&bed, finished).await, "completed", "untouched");
    assert_eq!(job_state(&bed, stale).await, "failed");

    bed.teardown().await;
}

#[tokio::test]
async fn a_reaped_job_can_be_re_run() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("reap").await;
    let (user, _p) = bed.user(tenant, "owner").await;
    let target = target_task(&bed, tenant, user).await;
    let dead = node_seen(&bed, tenant, 1000).await;
    let original = job(&bed, tenant, user, target, dead, "running").await;
    let state = bed.app_state().await;

    jobs::reap_stale_executors(&state, 180).await.expect("reap");
    assert_eq!(job_state(&bed, original).await, "failed");

    // The existing re-run path forks a fresh queued job linked to its
    // predecessor — the reap did not disturb that lineage (AC-4).
    let fresh = jobs::rerun(&state, tenant, user, original)
        .await
        .expect("a reaped job re-runs");
    assert_eq!(fresh.job.state, "queued");
    assert_eq!(fresh.job.predecessor_job_id, Some(original));

    bed.teardown().await;
}

// ── Orphaned runs on a HEALTHY node (MAIN-506) ──────────────────────────────
//
// The shape an agent restart leaves behind: the node reconnected and is
// heartbeating, so nothing above it can see anything wrong, while the run
// itself has not produced a line since the restart.

#[tokio::test]
async fn a_run_orphaned_by_an_agent_restart_does_not_stay_running_forever() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("stall").await;
    let (user, _p) = bed.user(tenant, "owner").await;
    let target = target_task(&bed, tenant, user).await;
    // The node is FINE — it restarted, reconnected, and heartbeat just now.
    let healthy = node_seen(&bed, tenant, 0).await;
    let orphan = job(&bed, tenant, user, target, healthy, "running").await;
    // It spoke two hours ago, and has said nothing since.
    entry(&bed, orphan, 7_200).await;
    last_touched(&bed, orphan, 7_200).await;
    let state = bed.app_state().await;

    // The liveness reaper is structurally blind to this: the node is healthy,
    // so the cutoff never trips however long the job has been silent (AC-1).
    assert_eq!(
        jobs::reap_stale_executors(&state, 180).await.expect("reap"),
        0,
        "node liveness cannot see an orphan on a live node"
    );
    assert_eq!(job_state(&bed, orphan).await, "running");

    let reaped = jobs::reap_stalled_jobs(&state, 3_600)
        .await
        .expect("stall reap");
    assert_eq!(reaped, 1, "job-level progress does see it");
    assert_eq!(job_state(&bed, orphan).await, "failed", "AC-6");
    let t = transcript_text(&bed, orphan).await;
    assert!(
        t.contains("no progress since") && t.contains("reaped after 3600s"),
        "the transcript names the cause: {t:?}"
    );

    // AC-2: terminal, and the card's work can be picked up again — warm,
    // because the fresh run resumes the same pinned agent session.
    let fresh = jobs::rerun(&state, tenant, user, orphan)
        .await
        .expect("an orphaned job re-runs");
    assert_eq!(fresh.job.state, "queued");
    assert_eq!(fresh.job.predecessor_job_id, Some(orphan));

    bed.teardown().await;
}

#[tokio::test]
async fn a_run_still_writing_transcript_is_never_stall_reaped() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("stall").await;
    let (user, _p) = bed.user(tenant, "owner").await;
    let target = target_task(&bed, tenant, user).await;
    let healthy = node_seen(&bed, tenant, 0).await;

    // A long-running job — claimed hours ago, so `updated_at` alone would
    // condemn it — that is plainly alive: it wrote a line a minute ago.
    let working = job(&bed, tenant, user, target, healthy, "running").await;
    last_touched(&bed, working, 7_200).await;
    entry(&bed, working, 7_200).await;
    entry(&bed, working, 60).await;

    // And one that has never written a line, but was claimed just now — the
    // gap between claim and the agent's first output must not read as silence.
    let starting = job(&bed, tenant, user, target, healthy, "claimed").await;

    let state = bed.app_state().await;
    let reaped = jobs::reap_stalled_jobs(&state, 3_600)
        .await
        .expect("stall reap");
    assert_eq!(reaped, 0, "progress within the window is progress");
    assert_eq!(job_state(&bed, working).await, "running");
    assert_eq!(job_state(&bed, starting).await, "claimed");

    bed.teardown().await;
}

#[tokio::test]
async fn a_paused_run_is_never_stall_reaped() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("stall").await;
    let (user, _p) = bed.user(tenant, "owner").await;
    let target = target_task(&bed, tenant, user).await;
    let healthy = node_seen(&bed, tenant, 0).await;
    // Waiting on a human is silence BY DESIGN, for as long as it takes.
    let paused = job(&bed, tenant, user, target, healthy, "waiting_on_human").await;
    last_touched(&bed, paused, 7_200).await;
    let state = bed.app_state().await;

    // Even with a zero window, where everything is "silent".
    let reaped = jobs::reap_stalled_jobs(&state, 0)
        .await
        .expect("stall reap");
    assert_eq!(reaped, 0);
    assert_eq!(job_state(&bed, paused).await, "waiting_on_human");

    bed.teardown().await;
}

#[tokio::test]
async fn the_stall_reap_is_atomic_and_fails_a_job_once() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("stall").await;
    let (user, _p) = bed.user(tenant, "owner").await;
    let target = target_task(&bed, tenant, user).await;
    let healthy = node_seen(&bed, tenant, 0).await;
    let state = bed.app_state().await;

    // A job that finished between scan and update falls out of the guard set.
    let finished = job(&bed, tenant, user, target, healthy, "running").await;
    last_touched(&bed, finished, 7_200).await;
    jobs::transition(&state, tenant, finished, "completed")
        .await
        .expect("complete");

    let silent = job(&bed, tenant, user, target, healthy, "running").await;
    last_touched(&bed, silent, 7_200).await;

    let first = jobs::reap_stalled_jobs(&state, 3_600)
        .await
        .expect("scan 1");
    assert_eq!(first, 1, "only the genuinely silent running job is reaped");
    let second = jobs::reap_stalled_jobs(&state, 3_600)
        .await
        .expect("scan 2");
    assert_eq!(second, 0, "a second replica double-fails nothing");

    assert_eq!(job_state(&bed, finished).await, "completed", "untouched");
    assert_eq!(job_state(&bed, silent).await, "failed");

    bed.teardown().await;
}

// ── Runs that already concluded (MAIN-607) ──────────────────────────────────
//
// Silence after an outcome is not failure: MAIN-600's run recorded `pr_opened`,
// the PR was reviewed and approved, and the completion signal that normally
// follows never arrived — after which the stall reaper failed the job and handed
// the finished card back for a fresh build.

#[tokio::test]
async fn a_build_run_that_recorded_its_outcome_is_concluded_not_failed() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("stall").await;
    let (user, _p) = bed.user(tenant, "owner").await;
    let target = target_task(&bed, tenant, user).await;
    let healthy = node_seen(&bed, tenant, 0).await;
    let done = build_job(&bed, tenant, user, target, healthy, "running").await;
    record_conclusion(&bed, done, "build_outcome", "pr_opened").await;
    // Silent since, exactly as the anomaly left it.
    entry(&bed, done, 7_200).await;
    last_touched(&bed, done, 7_200).await;
    let state = bed.app_state().await;

    let reap = jobs::scan_stalled_jobs(&state, 3_600)
        .await
        .expect("stall scan");
    assert_eq!(reap.concluded, 1, "AC-2");
    assert_eq!(reap.failed, 0, "AC-1: a concluded run is not a stalled one");
    assert_eq!(reap.handed_back, 0, "AC-3: the handback is not reached");
    assert_eq!(job_state(&bed, done).await, "completed", "AC-2");

    // AC-4: the operator can tell this apart from a stall failure, and the
    // outcome it names is why.
    let t = transcript_text(&bed, done).await;
    assert!(
        t.contains("already recorded its outcome (pr_opened)"),
        "the transcript names the conclusion: {t:?}"
    );
    assert!(
        !t.contains("reaped after"),
        "and does not read as a stall failure: {t:?}"
    );

    // Exactly once, like every other ending here: a second replica's scan finds
    // it terminal and does nothing.
    let again = jobs::scan_stalled_jobs(&state, 3_600)
        .await
        .expect("second scan");
    assert_eq!((again.concluded, again.failed), (0, 0));

    bed.teardown().await;
}

#[tokio::test]
async fn a_review_run_that_recorded_its_verdict_is_concluded_too() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("stall").await;
    let (user, _p) = bed.user(tenant, "owner").await;
    let workspace = bed.workspace(tenant).await;
    let healthy = node_seen(&bed, tenant, 0).await;
    let reviewed = review_job(&bed, tenant, user, workspace, healthy, "running").await;
    record_conclusion(&bed, reviewed, "review_verdict", "approved").await;
    last_touched(&bed, reviewed, 7_200).await;
    let state = bed.app_state().await;

    let reap = jobs::scan_stalled_jobs(&state, 3_600)
        .await
        .expect("stall scan");
    assert_eq!((reap.concluded, reap.failed, reap.handed_back), (1, 0, 0));
    assert_eq!(job_state(&bed, reviewed).await, "completed");
    assert!(transcript_text(&bed, reviewed)
        .await
        .contains("already recorded its outcome (approved)"));

    bed.teardown().await;
}

#[tokio::test]
async fn a_build_run_with_no_recorded_outcome_is_still_failed_and_handed_back() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("stall").await;
    let (user, _p) = bed.user(tenant, "owner").await;
    let target = target_task(&bed, tenant, user).await;
    let healthy = node_seen(&bed, tenant, 0).await;
    // The orphan MAIN-506 was built for, unchanged (AC-6).
    let orphan = build_job(&bed, tenant, user, target, healthy, "running").await;
    last_touched(&bed, orphan, 7_200).await;
    let state = bed.app_state().await;

    let reap = jobs::scan_stalled_jobs(&state, 3_600)
        .await
        .expect("stall scan");
    assert_eq!((reap.failed, reap.concluded), (1, 0));
    assert_eq!(reap.handed_back, 1, "the handback is still reached");
    assert_eq!(job_state(&bed, orphan).await, "failed");
    assert!(transcript_text(&bed, orphan).await.contains("reaped after"));

    bed.teardown().await;
}

#[tokio::test]
async fn a_run_that_concludes_between_the_read_and_the_write_is_not_failed() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("stall").await;
    let (user, _p) = bed.user(tenant, "owner").await;
    let target = target_task(&bed, tenant, user).await;
    let healthy = node_seen(&bed, tenant, 0).await;
    let racing = build_job(&bed, tenant, user, target, healthy, "running").await;
    last_touched(&bed, racing, 7_200).await;
    let state = bed.app_state().await;

    // Stand in the gap the guard exists for: the scan has read this job as
    // unconcluded, and the run records `pr_opened` before the write lands.
    let candidates = state
        .jobs
        .stalled_candidates(3_600)
        .await
        .expect("candidates");
    assert_eq!(candidates.len(), 1);
    assert!(candidates[0].recorded_outcome.is_none());
    record_conclusion(&bed, racing, "build_outcome", "pr_opened").await;

    assert!(
        !state
            .jobs
            .end_stalled_job(&candidates[0], 3_600)
            .await
            .expect("end"),
        "the re-asserted guard refuses a job that concluded in the gap"
    );
    assert_eq!(job_state(&bed, racing).await, "running");

    // And the next scan reads it as what it now is.
    let reap = jobs::scan_stalled_jobs(&state, 3_600)
        .await
        .expect("second scan");
    assert_eq!((reap.concluded, reap.failed), (1, 0));
    assert_eq!(job_state(&bed, racing).await, "completed");

    bed.teardown().await;
}
