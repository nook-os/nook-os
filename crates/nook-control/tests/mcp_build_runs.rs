//! MAIN-525: the build-run status tools. Driven at the `NookBackend` layer with
//! a resolved `McpCaller` — the same call the tools make once `mcp_auth` has
//! resolved the caller — so the tenant scoping under test is the real one.
//!
//! Rows are inserted directly rather than raised through `converge_builds`:
//! this is a READ surface, and a test that has to stand up a placeable executor
//! to assert what a finished run looks like is testing the dispatcher instead.
//!
//! Needs Postgres: set `DATABASE_URL` (`NOOK_REQUIRE_DB=1` in the suite).

use nook_control::mcp_backend::McpBackend;
use nook_db::{params, Db};
use nook_mcp::{BuildRunQuery, McpCaller, NookBackend};
use nook_testkit::TestBed;
use nook_types::*;

const TAIL: u32 = 100;

fn caller(tenant: TenantId, user: UserId, person: uuid::Uuid) -> McpCaller {
    McpCaller {
        person_id: person,
        user_id: user,
        tenant_id: tenant,
    }
}

fn query(workspace: WorkspaceId) -> BuildRunQuery {
    BuildRunQuery {
        workspace: workspace.to_string(),
        live_only: false,
        kind: None,
        limit: None,
    }
}

/// A board with one column, and the board key the task keys are built from.
async fn board(bed: &TestBed, tenant: TenantId) -> (BoardId, ColumnId, String) {
    let board = BoardId::new();
    let key = format!("R{}", &board.0.simple().to_string()[..5]).to_uppercase();
    bed.db()
        .exec(
            "INSERT INTO boards (id, tenant_id, name, key, provider) VALUES ($1,$2,'b',$3,'local')",
            params![board, tenant, key.clone()],
        )
        .await
        .expect("board");
    let col = ColumnId::new();
    bed.db()
        .exec(
            "INSERT INTO board_columns (id, board_id, name, position, type)
             VALUES ($1, $2, 'Todo', 0, 'unstarted')",
            params![col, board],
        )
        .await
        .expect("column");
    (board, col, key)
}

/// A team-visible card numbered `number`, so it has a resolvable board key.
#[allow(clippy::too_many_arguments)]
async fn task(
    bed: &TestBed,
    tenant: TenantId,
    board: BoardId,
    col: ColumnId,
    creator: UserId,
    workspace: WorkspaceId,
    number: i32,
    pr_url: Option<&str>,
) -> TaskId {
    let id = TaskId::new();
    bed.db()
        .exec(
            "INSERT INTO tasks
                 (id, tenant_id, board_id, column_id, title, type, created_by, workspace_id,
                  number, pr_url)
             VALUES ($1,$2,$3,$4,$5,'task',$6,$7,$8,$9)",
            params![
                id,
                tenant,
                board,
                col,
                format!("t-{number}"),
                creator,
                workspace.0,
                number,
                pr_url.map(str::to_string)
            ],
        )
        .await
        .expect("task");
    id
}

/// A loop job in whatever lifecycle position the case needs.
#[allow(clippy::too_many_arguments)]
async fn job(
    bed: &TestBed,
    tenant: TenantId,
    workspace: WorkspaceId,
    target: Option<TaskId>,
    requested_by: UserId,
    kind: &str,
    state: &str,
    executor: Option<NodeId>,
) -> JobId {
    let id = JobId::new();
    bed.db()
        .exec(
            "INSERT INTO loop_jobs
                 (id, tenant_id, kind, target_task_id, workspace_id, requested_by, state,
                  executor_node_id)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8)",
            params![
                id,
                tenant,
                kind.to_string(),
                target.map(|t| t.0),
                workspace.0,
                requested_by,
                state.to_string(),
                executor.map(|n| n.0)
            ],
        )
        .await
        .expect("job");
    id
}

async fn transcript(bed: &TestBed, job: JobId, content: &str) {
    bed.db()
        .exec(
            "INSERT INTO loop_job_transcript (id, job_id, source, content)
             VALUES ($1,$2,'agent',$3)",
            params![uuid::Uuid::now_v7(), job, content.to_string()],
        )
        .await
        .expect("transcript");
}

#[tokio::test]
async fn lists_a_workspace_s_runs_live_and_finished() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping mcp build runs test — no DATABASE_URL");
        return;
    };
    let tenant = bed.tenant("mcprun").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let node = bed.node(tenant, person).await;
    let node_name: (String,) = bed
        .db()
        .query_one("SELECT name FROM nodes WHERE id = $1", params![node])
        .await
        .unwrap();
    let ws = bed.workspace(tenant).await;
    let (b, c, key) = board(&bed, tenant).await;
    let backend = McpBackend {
        state: bed.app_state().await,
    };

    let running_card = task(&bed, tenant, b, c, user, ws, 1, None).await;
    let done_card = task(&bed, tenant, b, c, user, ws, 2, None).await;
    let running = job(
        &bed,
        tenant,
        ws,
        Some(running_card),
        user,
        "build",
        "running",
        Some(node),
    )
    .await;
    let done = job(
        &bed,
        tenant,
        ws,
        Some(done_card),
        user,
        "build",
        "completed",
        Some(node),
    )
    .await;
    // A review run, which the default `build` filter must leave out.
    job(&bed, tenant, ws, None, user, "review", "running", None).await;

    let all = backend
        .list_build_runs(caller(tenant, user, person), query(ws))
        .await
        .expect("list");
    assert_eq!(all.len(), 2, "builds only by default, live and finished");
    let ids: Vec<JobId> = all.iter().map(|r| r.id).collect();
    assert!(ids.contains(&running) && ids.contains(&done));

    // AC-1: every row names its card, kind, state, executor and start.
    let row = all.iter().find(|r| r.id == running).expect("running row");
    assert_eq!(row.kind, "build");
    assert_eq!(row.state, "running");
    assert_eq!(row.task_key.as_deref(), Some(format!("{key}-1").as_str()));
    assert_eq!(row.executor_node.as_deref(), Some(node_name.0.as_str()));
    assert!(
        row.elapsed_seconds >= 0,
        "a live run reports how long it has been going"
    );

    // AC-1: filterable to live ones.
    let live = backend
        .list_build_runs(
            caller(tenant, user, person),
            BuildRunQuery {
                live_only: true,
                ..query(ws)
            },
        )
        .await
        .expect("live list");
    assert_eq!(live.len(), 1, "only the run that has not finished");
    assert_eq!(live[0].id, running);

    // `any` widens to the review run beside the builds.
    let any = backend
        .list_build_runs(
            caller(tenant, user, person),
            BuildRunQuery {
                kind: Some("any".into()),
                ..query(ws)
            },
        )
        .await
        .expect("any list");
    assert_eq!(any.len(), 3, "every kind of run this repo has had");

    bed.teardown().await;
}

#[tokio::test]
async fn detail_answers_by_card_key_and_by_run_id() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("mcprun").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let node = bed.node(tenant, person).await;
    let ws = bed.workspace(tenant).await;
    let (b, c, key) = board(&bed, tenant).await;
    let backend = McpBackend {
        state: bed.app_state().await,
    };

    let pr = "https://github.com/acme/widgets/pull/7";
    let card = task(&bed, tenant, b, c, user, ws, 42, Some(pr)).await;
    let older = job(
        &bed,
        tenant,
        ws,
        Some(card),
        user,
        "build",
        "failed",
        Some(node),
    )
    .await;
    let newest = job(
        &bed,
        tenant,
        ws,
        Some(card),
        user,
        "build",
        "running",
        Some(node),
    )
    .await;
    transcript(&bed, newest, "cloning the repo\nchecking out main").await;

    // AC-2: by card key — the card's NEWEST run.
    let by_key = backend
        .get_build_run(caller(tenant, user, person), format!("{key}-42"), TAIL)
        .await
        .expect("by key");
    let run = by_key.run.expect("a run");
    assert_eq!(run.run.id, newest, "the newest run, not the older one");
    assert_eq!(
        by_key.task_key.as_deref(),
        Some(format!("{key}-42").as_str())
    );
    assert_eq!(run.run.state, "running");
    assert_eq!(run.pr_url.as_deref(), Some(pr), "the PR the card records");
    assert_eq!(run.transcript.entries.len(), 1);
    assert!(!run.transcript.truncated, "a short transcript is whole");
    assert!(by_key.summary.contains(&format!("{key}-42")));

    // AC-2: by run id — including an older run the key would not reach.
    let by_id = backend
        .get_build_run(caller(tenant, user, person), older.to_string(), TAIL)
        .await
        .expect("by id");
    let run = by_id.run.expect("a run");
    assert_eq!(run.run.id, older);
    assert_eq!(run.run.state, "failed");
    assert!(
        run.transcript.entries.is_empty(),
        "a run that narrated nothing has an empty tail"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn the_transcript_tail_is_bounded_and_says_so() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("mcprun").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let (b, c, _) = board(&bed, tenant).await;
    let backend = McpBackend {
        state: bed.app_state().await,
    };

    let card = task(&bed, tenant, b, c, user, ws, 9, None).await;
    let run = job(&bed, tenant, ws, Some(card), user, "build", "running", None).await;
    // Ten entries of four lines each: forty lines in all.
    for i in 0..10 {
        transcript(&bed, run, &format!("{i}-a\n{i}-b\n{i}-c\n{i}-d")).await;
    }

    // AC-3: exactly on the boundary, nothing is dropped and nothing claims to be.
    let whole = backend
        .get_build_run(caller(tenant, user, person), run.to_string(), 40)
        .await
        .expect("whole")
        .run
        .expect("a run");
    assert_eq!(whole.transcript.entries.len(), 10);
    assert_eq!(whole.transcript.lines, 40);
    assert_eq!(whole.transcript.total_lines, 40);
    assert!(!whole.transcript.truncated);
    assert!(whole.transcript.note.is_none());

    // One line under it drops the OLDEST whole entry and says how much it left.
    let cut = backend
        .get_build_run(caller(tenant, user, person), run.to_string(), 39)
        .await
        .expect("cut")
        .run
        .expect("a run");
    assert_eq!(
        cut.transcript.entries.len(),
        9,
        "whole entries, newest first"
    );
    assert_eq!(cut.transcript.lines, 36);
    assert_eq!(cut.transcript.total_lines, 40);
    assert!(cut.transcript.truncated);
    assert!(
        cut.transcript
            .note
            .as_deref()
            .is_some_and(|n| n.contains("36") && n.contains("40")),
        "the truncation states both numbers: {:?}",
        cut.transcript.note
    );
    assert_eq!(
        cut.transcript.entries[0].content, "1-a\n1-b\n1-c\n1-d",
        "the tail is the END of the run, and it reads oldest-first"
    );

    // A single entry larger than the whole budget is trimmed, never dropped —
    // a noisy run must not answer "how is it going" with nothing at all.
    // Its own card: one live build run per card is a database invariant.
    let noisy_card = task(&bed, tenant, b, c, user, ws, 10, None).await;
    let noisy = job(
        &bed,
        tenant,
        ws,
        Some(noisy_card),
        user,
        "build",
        "running",
        None,
    )
    .await;
    let long: String = (0..50).map(|i| format!("line-{i}\n")).collect();
    transcript(&bed, noisy, long.trim_end()).await;
    let trimmed = backend
        .get_build_run(caller(tenant, user, person), noisy.to_string(), 5)
        .await
        .expect("trimmed")
        .run
        .expect("a run");
    assert_eq!(trimmed.transcript.entries.len(), 1);
    assert_eq!(trimmed.transcript.lines, 5);
    assert_eq!(trimmed.transcript.total_lines, 50);
    assert!(trimmed.transcript.truncated);
    assert_eq!(
        trimmed.transcript.entries[0].content,
        "line-45\nline-46\nline-47\nline-48\nline-49"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn a_card_nothing_has_built_is_an_empty_answer_not_an_error() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("mcprun").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let (b, c, key) = board(&bed, tenant).await;
    let backend = McpBackend {
        state: bed.app_state().await,
    };
    task(&bed, tenant, b, c, user, ws, 5, None).await;

    // AC-4: an ordinary reply naming the card.
    let answer = backend
        .get_build_run(caller(tenant, user, person), format!("{key}-5"), TAIL)
        .await
        .expect("an empty answer is not an error");
    assert!(answer.run.is_none());
    assert_eq!(
        answer.task_key.as_deref(),
        Some(format!("{key}-5").as_str())
    );
    assert!(
        answer.summary.contains(&format!("{key}-5")),
        "the answer names the card: {:?}",
        answer.summary
    );

    // A card that does not exist at all is still a not-found.
    assert!(
        backend
            .get_build_run(caller(tenant, user, person), format!("{key}-404"), TAIL)
            .await
            .is_err(),
        "a card nobody filed is not an empty answer"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn a_cross_tenant_read_is_refused() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let theirs = bed.tenant("mcprun-a").await;
    let (their_user, _) = bed.user(theirs, "owner").await;
    let their_ws = bed.workspace(theirs).await;
    let (b, c, key) = board(&bed, theirs).await;
    let their_card = task(&bed, theirs, b, c, their_user, their_ws, 1, None).await;
    let their_run = job(
        &bed,
        theirs,
        their_ws,
        Some(their_card),
        their_user,
        "build",
        "running",
        None,
    )
    .await;
    transcript(&bed, their_run, "the launch codes").await;

    let mine = bed.tenant("mcprun-b").await;
    let (my_user, my_person) = bed.user(mine, "owner").await;
    let backend = McpBackend {
        state: bed.app_state().await,
    };
    let outsider = caller(mine, my_user, my_person);

    // AC-5: neither the list, nor the run by id, nor the card by key.
    assert!(
        backend
            .list_build_runs(outsider.clone(), query(their_ws))
            .await
            .is_err(),
        "another tenant's workspace does not resolve"
    );
    assert!(
        backend
            .get_build_run(outsider.clone(), their_run.to_string(), TAIL)
            .await
            .is_err(),
        "another tenant's run id is not readable"
    );
    assert!(
        backend
            .get_build_run(outsider, format!("{key}-1"), TAIL)
            .await
            .is_err(),
        "another tenant's card key is not readable"
    );

    bed.teardown().await;
}
