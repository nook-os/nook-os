//! Loop-job node execution — the control-plane half (MAIN-161): dispatching a
//! claimed job to its executor, applying the node's `JobFinished`, and the
//! crash-honesty transitions (node disconnect / no executor). The node-side
//! worktree machinery is tested in `nook-node`; here we prove the state machine
//! and the run hand-off. Each test runs on its OWN private database (MAIN-156).
//!
//! Needs Postgres: `DATABASE_URL` (`NOOK_REQUIRE_DB=1` in the suite).

use nook_control::services::jobs;
use nook_control::state::AppState;
use nook_control::ws::registry::NodeHandle;
use nook_db::{params, Db};
use nook_types::*;
use tokio::sync::mpsc;
use uuid::Uuid;

use nook_testkit::TestBed;

/// A board (with a known key) + column + task (with a known number), so the
/// dispatched message's `target_task_key` is `<key>-<number>`.
async fn task_with_key(bed: &TestBed, tenant: TenantId, key: &str, number: i32) -> TaskId {
    let board = BoardId::new();
    bed.db()
        .exec(
            "INSERT INTO boards (id, tenant_id, name, key, provider) VALUES ($1,$2,'b',$3,'local')",
            params![board, tenant, key],
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
    let id = TaskId::new();
    bed.db()
        .exec(
            "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, type, number)
         VALUES ($1,$2,$3,$4,'t','task',$5)",
            params![id, tenant, board, col, number],
        )
        .await
        .expect("task");
    id
}

async fn node(bed: &TestBed, tenant: TenantId) -> NodeId {
    let id = NodeId::new();
    bed.db()
        .exec(
            "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status)
         VALUES ($1,$2,$3,$4,'online')",
            params![
                id,
                tenant,
                format!("n-{}", id.0.simple()),
                format!("h-{}", id.0.simple())
            ],
        )
        .await
        .expect("node");
    id
}

/// A `node_workspaces` row carrying a clonable remote + branch.
async fn node_workspace(
    bed: &TestBed,
    tenant: TenantId,
    node: NodeId,
    ws: WorkspaceId,
    url: Option<&str>,
    branch: Option<&str>,
) {
    bed.db()
        .exec(
            "INSERT INTO node_workspaces (id, tenant_id, node_id, workspace_id, path, git_remote_url, git_branch)
         VALUES ($1,$2,$3,$4,$5,$6,$7)",
            params![
                Uuid::now_v7(),
                tenant,
                node,
                ws,
                format!("/checkouts/{}", ws.0.simple()),
                url.map(str::to_string),
                branch.map(str::to_string)
            ],
        )
        .await
        .expect("node_workspace");
}

/// A job in `state` on `target`, optionally already claimed by `executor`.
async fn job(
    bed: &TestBed,
    tenant: TenantId,
    target: TaskId,
    ws: Option<WorkspaceId>,
    state: &str,
    executor: Option<NodeId>,
) -> JobId {
    let id = JobId::new();
    bed.db()
        .exec(
            "INSERT INTO loop_jobs
            (id, tenant_id, kind, target_task_id, workspace_id, requested_by, state, executor_node_id)
         VALUES ($1,$2,'spec',$3,$4,$5,$6,$7)",
            params![
                id,
                tenant,
                target,
                ws.map(|w| w.0),
                Uuid::now_v7(), // requested_by — a person id; unused here
                state,
                executor.map(|n| n.0)
            ],
        )
        .await
        .expect("job");
    id
}

async fn load(bed: &TestBed, id: JobId) -> LoopJob {
    // Through the `Db` surface, not raw sqlx: row mapping is `FromDbRow` since
    // MAIN-327, and a DTO no longer implements sqlx's `FromRow` at all.
    bed.db()
        .query_one("SELECT * FROM loop_jobs WHERE id = $1", params![id])
        .await
        .expect("load job")
}

async fn transcript_text(bed: &TestBed, id: JobId) -> String {
    let lines: Vec<(String,)> = bed
        .db()
        .query_all(
            "SELECT content FROM loop_job_transcript WHERE job_id = $1 ORDER BY id",
            params![id],
        )
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
    let target = task_with_key(&bed, tenant, "ACME", 7).await;
    let n = node(&bed, tenant).await;
    node_workspace(
        &bed,
        tenant,
        n,
        ws,
        Some("git@example.test:acme/repo.git"),
        Some("trunk"),
    )
    .await;
    let j = job(&bed, tenant, target, Some(ws), "claimed", Some(n)).await;
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

    jobs::dispatch_to_node(&state, tenant, &load(&bed, j).await)
        .await
        .expect("dispatch");

    // The job is now running…
    assert_eq!(load(&bed, j).await.state, "running");
    // …and the node was told to run it, with the resolved repo + ticket key.
    match rx.try_recv().expect("a RunLoopJob was sent") {
        nook_proto::ControlToNode::RunLoopJob {
            kind,
            target_task_key,
            repo_url,
            branch,
            server_url,
            ..
        } => {
            assert_eq!(kind, "spec");
            assert_eq!(target_task_key, "ACME-7");
            assert_eq!(repo_url, "git@example.test:acme/repo.git");
            assert_eq!(branch, "trunk");
            // A deployment that advertises no agent URL sends none (MAIN-465):
            // the node falls back to its own configured server address.
            assert_eq!(server_url, None);
        }
        other => panic!("expected RunLoopJob, got {other:?}"),
    }

    bed.teardown().await;
}

/// MAIN-465: a deployment that advertises its HTTP API (`NOOK_PUBLIC_API_URL`)
/// sends that address with every run, so the run's `nook` CLI dials — and its
/// transcript names — the canonical control plane rather than the internal
/// service name the executor node happens to reach it by.
#[tokio::test]
async fn dispatch_carries_the_advertised_api_url_when_configured() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("lje-url").await;
    let ws = bed.workspace(tenant).await;
    let target = task_with_key(&bed, tenant, "ACME", 8).await;
    let n = node(&bed, tenant).await;
    node_workspace(
        &bed,
        tenant,
        n,
        ws,
        Some("git@example.test:acme/repo.git"),
        Some("trunk"),
    )
    .await;
    let j = job(&bed, tenant, target, Some(ws), "claimed", Some(n)).await;

    let mut cfg = bed.config();
    cfg.public_api_url = Some("https://cp.example.test".into());
    // The agent listener's address must NOT leak into the run (the reviewer's
    // scope-conflict catch on the first cut of this PR): set it to a decoy and
    // assert the API URL is what rides the message.
    cfg.agent_public_url = Some("https://agents.example.test:8081".into());
    let state = AppState::new(bed.db(), cfg, None).await;

    let (tx, mut rx) = mpsc::channel(4);
    state.registry.register_node(
        n,
        NodeHandle {
            tenant_id: tenant,
            tx,
        },
    );

    jobs::dispatch_to_node(&state, tenant, &load(&bed, j).await)
        .await
        .expect("dispatch");

    match rx.try_recv().expect("a RunLoopJob was sent") {
        nook_proto::ControlToNode::RunLoopJob { server_url, .. } => {
            assert_eq!(server_url.as_deref(), Some("https://cp.example.test"));
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
    let target = task_with_key(&bed, tenant, "ACME", 8).await;
    let n = node(&bed, tenant).await;
    // A node_workspaces row exists but carries no remote URL.
    node_workspace(&bed, tenant, n, ws, None, None).await;
    let j = job(&bed, tenant, target, Some(ws), "claimed", Some(n)).await;
    let state = bed.app_state().await;
    let (tx, _rx) = mpsc::channel(4);
    state.registry.register_node(
        n,
        NodeHandle {
            tenant_id: tenant,
            tx,
        },
    );

    jobs::dispatch_to_node(&state, tenant, &load(&bed, j).await)
        .await
        .expect("dispatch");

    assert_eq!(load(&bed, j).await.state, "failed");
    assert!(transcript_text(&bed, j)
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
    let target = task_with_key(&bed, tenant, "ACME", 9).await;
    let n = node(&bed, tenant).await;
    node_workspace(
        &bed,
        tenant,
        n,
        ws,
        Some("git@example.test:acme/repo.git"),
        Some("main"),
    )
    .await;
    let j = job(&bed, tenant, target, Some(ws), "claimed", Some(n)).await;
    let state = bed.app_state().await;
    // Node is NOT registered — `send_to_node` returns false.

    jobs::dispatch_to_node(&state, tenant, &load(&bed, j).await)
        .await
        .expect("dispatch");

    assert_eq!(load(&bed, j).await.state, "failed");
    assert!(transcript_text(&bed, j).await.contains("offline"));

    bed.teardown().await;
}

#[tokio::test]
async fn finish_completes_or_fails_with_the_tail() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("lje").await;
    let target = task_with_key(&bed, tenant, "ACME", 10).await;
    let n = node(&bed, tenant).await;
    let ok_job = job(&bed, tenant, target, None, "running", Some(n)).await;
    let bad_job = job(&bed, tenant, target, None, "running", Some(n)).await;
    let state = bed.app_state().await;

    jobs::finish(&state, tenant, ok_job, true, "")
        .await
        .expect("finish ok");
    assert_eq!(load(&bed, ok_job).await.state, "completed");

    jobs::finish(&state, tenant, bad_job, false, "panic: boom at line 3")
        .await
        .expect("finish fail");
    assert_eq!(load(&bed, bad_job).await.state, "failed");
    assert!(transcript_text(&bed, bad_job)
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
    let target = task_with_key(&bed, tenant, "ACME", 11).await;
    let n = node(&bed, tenant).await;
    let other = node(&bed, tenant).await;

    let claimed = job(&bed, tenant, target, None, "claimed", Some(n)).await;
    let running = job(&bed, tenant, target, None, "running", Some(n)).await;
    let done = job(&bed, tenant, target, None, "completed", Some(n)).await;
    let elsewhere = job(&bed, tenant, target, None, "running", Some(other)).await;
    let state = bed.app_state().await;

    jobs::fail_stranded_for_node(&state, n)
        .await
        .expect("fail stranded");

    assert_eq!(
        load(&bed, claimed).await.state,
        "failed",
        "claimed → failed"
    );
    assert_eq!(
        load(&bed, running).await.state,
        "failed",
        "running → failed"
    );
    assert_eq!(
        load(&bed, done).await.state,
        "completed",
        "terminal untouched"
    );
    assert_eq!(
        load(&bed, elsewhere).await.state,
        "running",
        "another node's job is untouched"
    );
    assert!(transcript_text(&bed, running)
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
    let executor = node(&bed, tenant).await;
    let bystander = node(&bed, tenant).await;
    node_workspace(
        &bed,
        tenant,
        executor,
        ws,
        Some("git@x:acme/exec.git"),
        Some("main"),
    )
    .await;
    node_workspace(
        &bed,
        tenant,
        bystander,
        ws,
        Some("git@x:acme/other.git"),
        Some("dev"),
    )
    .await;
    let state = bed.app_state().await;

    // The executor's own row wins.
    let (url, branch) = jobs::resolve_repo(&state, tenant, ws, executor)
        .await
        .expect("resolve")
        .expect("some");
    assert_eq!(url, "git@x:acme/exec.git");
    assert_eq!(branch, "main");

    // A node with no row for this workspace falls back to any usable remote.
    let stranger = node(&bed, tenant).await;
    let fallback = jobs::resolve_repo(&state, tenant, ws, stranger)
        .await
        .expect("resolve");
    assert!(fallback.is_some(), "falls back to another node's remote");

    bed.teardown().await;
}

#[tokio::test]
async fn resolve_repo_falls_back_to_the_workspace_own_remote() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("lje").await;
    let ws = bed.workspace(tenant).await;
    // A workspace with a declared remote but NO `node_workspaces` checkout on any
    // node — the shape a freshly-seeded dogfood workspace has (MAIN-341). Before
    // this fallback the job died with "no known git remote" for a workspace that
    // plainly had one; now it resolves to the workspace's own remote, branch
    // defaulting to `main`.
    bed.db()
        .exec(
            "UPDATE workspaces SET git_remote_url = $1 WHERE id = $2",
            params!["/workspace/nook-dogfood.git", ws],
        )
        .await
        .expect("set workspace remote");
    let stranger = node(&bed, tenant).await;
    let state = bed.app_state().await;
    let (url, branch) = jobs::resolve_repo(&state, tenant, ws, stranger)
        .await
        .expect("resolve")
        .expect("falls back to the workspace's own remote");
    assert_eq!(url, "/workspace/nook-dogfood.git");
    assert_eq!(branch, "main");

    bed.teardown().await;
}

#[tokio::test]
async fn a_node_cannot_touch_a_job_it_does_not_execute() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("lje").await;
    let target = task_with_key(&bed, tenant, "ACME", 12).await;
    let runner = node(&bed, tenant).await;
    let intruder = node(&bed, tenant).await;
    let j = job(&bed, tenant, target, None, "running", Some(runner)).await;
    let state = bed.app_state().await;

    // An intruder node's transcript + finish are dropped (security): a node token
    // is scoped to its own runs.
    jobs::transcript_from_node(&state, intruder, j, "agent", "evil injection")
        .await
        .expect("call returns ok");
    jobs::finish_from_node(&state, intruder, j, false, "spoofed kill")
        .await
        .expect("call returns ok");
    assert_eq!(
        load(&bed, j).await.state,
        "running",
        "intruder cannot end it"
    );
    assert!(
        transcript_text(&bed, j).await.is_empty(),
        "intruder cannot inject a transcript line"
    );

    // The actual executor's transcript + finish are applied.
    jobs::transcript_from_node(&state, runner, j, "agent", "real output")
        .await
        .expect("executor transcript");
    assert!(transcript_text(&bed, j).await.contains("real output"));
    jobs::finish_from_node(&state, runner, j, true, "")
        .await
        .expect("executor finish");
    assert_eq!(load(&bed, j).await.state, "completed");

    bed.teardown().await;
}

/// A steering message is recorded ONCE.
///
/// Both ends used to append it: `post_message` on send, and the node again when
/// `--replay-user-messages` echoed the turn back. The human typed once and saw
/// their line twice, which read as the agent parroting them.
///
/// The control plane is the end that must keep it — it is the only one that
/// can. A queued job has no executor to echo anything, an offline node never
/// echoes, and the REST call has to return the entry it created.
#[tokio::test]
async fn a_steering_message_is_recorded_once() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("lje").await;
    let (user, _person) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let target = task_with_key(&bed, tenant, "ACME", 42).await;
    let n = node(&bed, tenant).await;
    let j = job(&bed, tenant, target, Some(ws), "running", Some(n)).await;
    let state = bed.app_state().await;

    let body = "check the tenant before you draft";
    jobs::post_message(&state, tenant, user, j, body)
        .await
        .expect("steering message accepted");

    let lines = transcript_text(&bed, j).await;
    let mine = lines.matches(body).count();
    assert_eq!(
        mine, 1,
        "the steering message appears {mine} times — the control plane records \
         it on send, so the node must not record it again on the echo"
    );

    bed.teardown().await;
}

/// MAIN-515 NG-4: placement crossing tenants is only half a fix if the run's
/// own reports are then thrown away.
///
/// A node authenticates on its websocket in the tenant the MACHINE was joined
/// into. Once an owner's node runs a job raised in another of their tenants,
/// that connection tenant is not the job's — and every `*_from_node` handler
/// looked the job up with it, found nothing, and dropped the report as a spoof.
/// The run would have executed to completion with no transcript, no worktree
/// record and no terminal state, to be reaped later as stalled.
#[tokio::test]
async fn a_job_reported_from_its_executor_lands_whichever_tenant_the_node_is_homed_in() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let home = bed.tenant("njoined").await;
    let other = bed.tenant("jobtenant").await;
    // The machine lives in `home`; the work lives in `other`.
    let runner = node(&bed, home).await;
    let target = task_with_key(&bed, other, "ACME", 7).await;
    let j = job(&bed, other, target, None, "running", Some(runner)).await;
    let state = bed.app_state().await;

    jobs::transcript_from_node(&state, runner, j, "agent", "real output")
        .await
        .expect("executor transcript");
    assert!(
        transcript_text(&bed, j).await.contains("real output"),
        "the executor's own line is not a spoof just because its machine is \
         homed elsewhere"
    );

    jobs::finish_from_node(&state, runner, j, true, "")
        .await
        .expect("executor finish");
    assert_eq!(
        load(&bed, j).await.state,
        "completed",
        "and the run reaches a terminal state rather than being reaped as stalled"
    );

    bed.teardown().await;
}

/// The security edge, restated across the boundary: widening the lookup to the
/// job's own tenant must not let ANY node speak for a job it does not execute.
#[tokio::test]
async fn a_stranger_node_in_another_tenant_still_cannot_touch_the_job() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let home = bed.tenant("njoined").await;
    let other = bed.tenant("jobtenant").await;
    let runner = node(&bed, home).await;
    let stranger = node(&bed, other).await;
    let target = task_with_key(&bed, other, "ACME", 8).await;
    let j = job(&bed, other, target, None, "running", Some(runner)).await;
    let state = bed.app_state().await;

    jobs::transcript_from_node(&state, stranger, j, "agent", "evil injection")
        .await
        .expect("call returns ok");
    jobs::finish_from_node(&state, stranger, j, false, "spoofed kill")
        .await
        .expect("call returns ok");
    assert!(transcript_text(&bed, j).await.is_empty());
    assert_eq!(
        load(&bed, j).await.state,
        "running",
        "the node match is what authorizes, and it still refuses everyone else"
    );

    bed.teardown().await;
}

/// A disconnect strands whatever the machine held — including work raised in
/// another of its owner's tenants, which must be failed in ITS tenant.
#[tokio::test]
async fn a_disconnect_fails_cross_tenant_work_in_the_jobs_own_tenant() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let home = bed.tenant("njoined").await;
    let other = bed.tenant("jobtenant").await;
    let n = node(&bed, home).await;
    let here = job(
        &bed,
        home,
        task_with_key(&bed, home, "HOME", 1).await,
        None,
        "running",
        Some(n),
    )
    .await;
    let away = job(
        &bed,
        other,
        task_with_key(&bed, other, "AWAY", 1).await,
        None,
        "running",
        Some(n),
    )
    .await;
    let state = bed.app_state().await;

    jobs::fail_stranded_for_node(&state, n)
        .await
        .expect("fail stranded");

    assert_eq!(load(&bed, here).await.state, "failed");
    assert_eq!(
        load(&bed, away).await.state,
        "failed",
        "a job left running with nothing behind it is the dishonest board this \
         whole path exists to prevent — tenancy does not excuse it"
    );

    bed.teardown().await;
}
