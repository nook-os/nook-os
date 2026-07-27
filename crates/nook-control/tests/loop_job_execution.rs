//! Loop-job node execution — the control-plane half (MAIN-161): dispatching a
//! claimed job to its executor, applying the node's `JobFinished`, and the
//! crash-honesty transitions (node disconnect / no executor). The node-side
//! worktree machinery is tested in `nook-node`; here we prove the state machine
//! and the run hand-off. Each test runs on its OWN private database (MAIN-156).
//!
//! Needs Postgres: `DATABASE_URL` (`NOOK_REQUIRE_DB=1` in the suite).

use nook_control::services::jobs;
use nook_control::ws::registry::NodeHandle;
use nook_types::*;
use sqlx::PgPool;
use tokio::sync::mpsc;
use uuid::Uuid;

use nook_testkit::TestBed;

/// A board (with a known key) + column + task (with a known number), so the
/// dispatched message's `target_task_key` is `<key>-<number>`.
async fn task_with_key(db: &PgPool, tenant: TenantId, key: &str, number: i32) -> TaskId {
    let board = BoardId::new();
    sqlx::query(
        "INSERT INTO boards (id, tenant_id, name, key, provider) VALUES ($1,$2,'b',$3,'local')",
    )
    .bind(board)
    .bind(tenant)
    .bind(key)
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
    let id = TaskId::new();
    sqlx::query(
        "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, type, number)
         VALUES ($1,$2,$3,$4,'t','task',$5)",
    )
    .bind(id)
    .bind(tenant)
    .bind(board)
    .bind(col)
    .bind(number)
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

/// A `node_workspaces` row carrying a clonable remote + branch.
async fn node_workspace(
    db: &PgPool,
    tenant: TenantId,
    node: NodeId,
    ws: WorkspaceId,
    url: Option<&str>,
    branch: Option<&str>,
) {
    sqlx::query(
        "INSERT INTO node_workspaces (id, tenant_id, node_id, workspace_id, path, git_remote_url, git_branch)
         VALUES ($1,$2,$3,$4,$5,$6,$7)",
    )
    .bind(Uuid::now_v7())
    .bind(tenant)
    .bind(node)
    .bind(ws)
    .bind(format!("/checkouts/{}", ws.0.simple()))
    .bind(url)
    .bind(branch)
    .execute(db)
    .await
    .expect("node_workspace");
}

/// A job in `state` on `target`, optionally already claimed by `executor`.
async fn job(
    db: &PgPool,
    tenant: TenantId,
    target: TaskId,
    ws: Option<WorkspaceId>,
    state: &str,
    executor: Option<NodeId>,
) -> JobId {
    let id = JobId::new();
    sqlx::query(
        "INSERT INTO loop_jobs
            (id, tenant_id, kind, target_task_id, workspace_id, requested_by, state, executor_node_id)
         VALUES ($1,$2,'spec',$3,$4,$5,$6,$7)",
    )
    .bind(id)
    .bind(tenant)
    .bind(target)
    .bind(ws)
    .bind(Uuid::now_v7()) // requested_by — a person id; unused here
    .bind(state)
    .bind(executor)
    .execute(db)
    .await
    .expect("job");
    id
}

async fn load(db: &PgPool, id: JobId) -> LoopJob {
    sqlx::query_as("SELECT * FROM loop_jobs WHERE id = $1")
        .bind(id)
        .fetch_one(db)
        .await
        .expect("load job")
}

async fn transcript_text(db: &PgPool, id: JobId) -> String {
    let lines: Vec<(String,)> =
        sqlx::query_as("SELECT content FROM loop_job_transcript WHERE job_id = $1 ORDER BY id")
            .bind(id)
            .fetch_all(db)
            .await
            .expect("transcript");
    lines
        .into_iter()
        .map(|(c,)| c)
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn dispatch_runs_the_job_and_sends_run_to_the_node() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("lje").await;
    let ws = bed.workspace(tenant).await;
    let target = task_with_key(&bed.pool, tenant, "ACME", 7).await;
    let n = node(&bed.pool, tenant).await;
    node_workspace(
        &bed.pool,
        tenant,
        n,
        ws,
        Some("git@example.test:acme/repo.git"),
        Some("trunk"),
    )
    .await;
    let j = job(&bed.pool, tenant, target, Some(ws), "claimed", Some(n)).await;
    let state = bed.app_state().await;

    // Register a live connection for the node so `send_to_node` succeeds and we
    // can inspect what was sent.
    let (tx, mut rx) = mpsc::channel(4);
    state.registry.register_node(
        n,
        NodeHandle {
            tenant_id: tenant,
            tx,
        },
    );

    jobs::dispatch_to_node(&state, tenant, &load(&bed.pool, j).await)
        .await
        .expect("dispatch");

    // The job is now running…
    assert_eq!(load(&bed.pool, j).await.state, "running");
    // …and the node was told to run it, with the resolved repo + ticket key.
    match rx.try_recv().expect("a RunLoopJob was sent") {
        nook_proto::ControlToNode::RunLoopJob {
            kind,
            target_task_key,
            repo_url,
            branch,
            ..
        } => {
            assert_eq!(kind, "spec");
            assert_eq!(target_task_key, "ACME-7");
            assert_eq!(repo_url, "git@example.test:acme/repo.git");
            assert_eq!(branch, "trunk");
        }
        other => panic!("expected RunLoopJob, got {other:?}"),
    }

    bed.teardown().await;
}

#[tokio::test]
async fn dispatch_fails_the_job_when_there_is_no_remote() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("lje").await;
    let ws = bed.workspace(tenant).await;
    let target = task_with_key(&bed.pool, tenant, "ACME", 8).await;
    let n = node(&bed.pool, tenant).await;
    // A node_workspaces row exists but carries no remote URL.
    node_workspace(&bed.pool, tenant, n, ws, None, None).await;
    let j = job(&bed.pool, tenant, target, Some(ws), "claimed", Some(n)).await;
    let state = bed.app_state().await;
    let (tx, _rx) = mpsc::channel(4);
    state.registry.register_node(
        n,
        NodeHandle {
            tenant_id: tenant,
            tx,
        },
    );

    jobs::dispatch_to_node(&state, tenant, &load(&bed.pool, j).await)
        .await
        .expect("dispatch");

    assert_eq!(load(&bed.pool, j).await.state, "failed");
    assert!(transcript_text(&bed.pool, j)
        .await
        .contains("no known git remote"));

    bed.teardown().await;
}

#[tokio::test]
async fn dispatch_fails_the_job_when_the_node_is_offline() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("lje").await;
    let ws = bed.workspace(tenant).await;
    let target = task_with_key(&bed.pool, tenant, "ACME", 9).await;
    let n = node(&bed.pool, tenant).await;
    node_workspace(
        &bed.pool,
        tenant,
        n,
        ws,
        Some("git@example.test:acme/repo.git"),
        Some("main"),
    )
    .await;
    let j = job(&bed.pool, tenant, target, Some(ws), "claimed", Some(n)).await;
    let state = bed.app_state().await;
    // Node is NOT registered — `send_to_node` returns false.

    jobs::dispatch_to_node(&state, tenant, &load(&bed.pool, j).await)
        .await
        .expect("dispatch");

    assert_eq!(load(&bed.pool, j).await.state, "failed");
    assert!(transcript_text(&bed.pool, j).await.contains("offline"));

    bed.teardown().await;
}

#[tokio::test]
async fn finish_completes_or_fails_with_the_tail() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("lje").await;
    let target = task_with_key(&bed.pool, tenant, "ACME", 10).await;
    let n = node(&bed.pool, tenant).await;
    let ok_job = job(&bed.pool, tenant, target, None, "running", Some(n)).await;
    let bad_job = job(&bed.pool, tenant, target, None, "running", Some(n)).await;
    let state = bed.app_state().await;

    jobs::finish(&state, tenant, ok_job, true, "")
        .await
        .expect("finish ok");
    assert_eq!(load(&bed.pool, ok_job).await.state, "completed");

    jobs::finish(&state, tenant, bad_job, false, "panic: boom at line 3")
        .await
        .expect("finish fail");
    assert_eq!(load(&bed.pool, bad_job).await.state, "failed");
    assert!(transcript_text(&bed.pool, bad_job)
        .await
        .contains("boom at line 3"));

    bed.teardown().await;
}

#[tokio::test]
async fn a_disconnect_fails_only_this_nodes_live_jobs() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("lje").await;
    let target = task_with_key(&bed.pool, tenant, "ACME", 11).await;
    let n = node(&bed.pool, tenant).await;
    let other = node(&bed.pool, tenant).await;

    let claimed = job(&bed.pool, tenant, target, None, "claimed", Some(n)).await;
    let running = job(&bed.pool, tenant, target, None, "running", Some(n)).await;
    let done = job(&bed.pool, tenant, target, None, "completed", Some(n)).await;
    let elsewhere = job(&bed.pool, tenant, target, None, "running", Some(other)).await;
    let state = bed.app_state().await;

    jobs::fail_stranded_for_node(&state, tenant, n)
        .await
        .expect("fail stranded");

    assert_eq!(
        load(&bed.pool, claimed).await.state,
        "failed",
        "claimed → failed"
    );
    assert_eq!(
        load(&bed.pool, running).await.state,
        "failed",
        "running → failed"
    );
    assert_eq!(
        load(&bed.pool, done).await.state,
        "completed",
        "terminal untouched"
    );
    assert_eq!(
        load(&bed.pool, elsewhere).await.state,
        "running",
        "another node's job is untouched"
    );
    assert!(transcript_text(&bed.pool, running)
        .await
        .contains("disconnected"));

    bed.teardown().await;
}

#[tokio::test]
async fn resolve_repo_prefers_the_executor_then_falls_back() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("lje").await;
    let ws = bed.workspace(tenant).await;
    let executor = node(&bed.pool, tenant).await;
    let bystander = node(&bed.pool, tenant).await;
    node_workspace(
        &bed.pool,
        tenant,
        executor,
        ws,
        Some("git@x:acme/exec.git"),
        Some("main"),
    )
    .await;
    node_workspace(
        &bed.pool,
        tenant,
        bystander,
        ws,
        Some("git@x:acme/other.git"),
        Some("dev"),
    )
    .await;
    let state = bed.app_state().await;

    // The executor's own row wins.
    let (url, branch) = jobs::resolve_repo(&state, ws, executor)
        .await
        .expect("resolve")
        .expect("some");
    assert_eq!(url, "git@x:acme/exec.git");
    assert_eq!(branch, "main");

    // A node with no row for this workspace falls back to any usable remote.
    let stranger = node(&bed.pool, tenant).await;
    let fallback = jobs::resolve_repo(&state, ws, stranger)
        .await
        .expect("resolve");
    assert!(fallback.is_some(), "falls back to another node's remote");

    bed.teardown().await;
}

#[tokio::test]
async fn a_node_cannot_touch_a_job_it_does_not_execute() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("lje").await;
    let target = task_with_key(&bed.pool, tenant, "ACME", 12).await;
    let runner = node(&bed.pool, tenant).await;
    let intruder = node(&bed.pool, tenant).await;
    let j = job(&bed.pool, tenant, target, None, "running", Some(runner)).await;
    let state = bed.app_state().await;

    // An intruder node's transcript + finish are dropped (security): a node token
    // is scoped to its own runs.
    jobs::transcript_from_node(&state, tenant, intruder, j, "agent", "evil injection")
        .await
        .expect("call returns ok");
    jobs::finish_from_node(&state, tenant, intruder, j, false, "spoofed kill")
        .await
        .expect("call returns ok");
    assert_eq!(
        load(&bed.pool, j).await.state,
        "running",
        "intruder cannot end it"
    );
    assert!(
        transcript_text(&bed.pool, j).await.is_empty(),
        "intruder cannot inject a transcript line"
    );

    // The actual executor's transcript + finish are applied.
    jobs::transcript_from_node(&state, tenant, runner, j, "agent", "real output")
        .await
        .expect("executor transcript");
    assert!(transcript_text(&bed.pool, j).await.contains("real output"));
    jobs::finish_from_node(&state, tenant, runner, j, true, "")
        .await
        .expect("executor finish");
    assert_eq!(load(&bed.pool, j).await.state, "completed");

    bed.teardown().await;
}
