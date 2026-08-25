//! The Postgres `held_on_nodes` and its in-memory fake answer the same fixture
//! the same way (MAIN-616 AC-5).
//!
//! Placement reads this to decide whether a node has room, and the Nodes table
//! reads it to explain that decision to a person. A fake that counted a state
//! the real query does not would make every `jobs_fake` assertion about
//! capacity a statement about nothing — so the agreement is asserted directly,
//! against one fixture built twice.
//!
//! Needs Postgres: `DATABASE_URL` (`NOOK_REQUIRE_DB=1` in the suite).

use nook_control::repo::jobs::{FakeLoopJobRepository, LoopJobRepository, NewLoopJob};
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;

/// A board + column + task, so a job has the ticket its `target_task_id`
/// references.
async fn target_task(bed: &TestBed, tenant: TenantId) -> TaskId {
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
            "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, type)
         VALUES ($1,$2,$3,$4,'t','task')",
            params![task, tenant, board, col],
        )
        .await
        .expect("task");
    task
}

fn new_job(id: JobId, tenant: TenantId, task: TaskId, requester: UserId) -> NewLoopJob {
    NewLoopJob {
        id,
        tenant,
        kind: "spec".into(),
        target_task_id: Some(task),
        workspace_id: None,
        requested_by: requester,
        seed: None,
        predecessor_job_id: None,
        review_pr_number: None,
        review_head_sha: None,
        review_forced: false,
        build_fingerprint: None,
    }
}

#[tokio::test]
async fn the_fake_and_postgres_hold_the_same_jobs() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("held").await;
    let (user, _person) = bed.user(tenant, "owner").await;
    let task = target_task(&bed, tenant).await;
    let state = bed.app_state().await;
    let fake = FakeLoopJobRepository::new();

    let (mine, elsewhere) = (NodeId::new(), NodeId::new());
    // Every state a job can be in, so the two implementations have to agree
    // about the terminal ones as much as about the held ones.
    let fixture: Vec<(JobId, &str, Option<NodeId>)> = vec![
        (JobId::new(), "claimed", Some(mine)),
        (JobId::new(), "running", Some(mine)),
        (JobId::new(), "waiting_on_human", Some(mine)),
        (JobId::new(), "completed", Some(mine)),
        (JobId::new(), "failed", Some(mine)),
        (JobId::new(), "canceled", Some(mine)),
        (JobId::new(), "waiting_on_human", Some(elsewhere)),
        (JobId::new(), "queued", None),
    ];

    for (id, job_state, node) in &fixture {
        state
            .jobs
            .create(new_job(*id, tenant, task, user))
            .await
            .expect("db create");
        fake.create(new_job(*id, tenant, task, user))
            .await
            .expect("fake create");
        if let Some(node) = node {
            // Straight to the column on both sides: the fixture stands in states
            // no legal transition would reach in one step, which is the point —
            // the query is being pinned, not the state machine.
            bed.db()
                .exec(
                    "UPDATE loop_jobs SET state = $2, executor_node_id = $3 WHERE id = $1",
                    params![id, *job_state, node],
                )
                .await
                .expect("db place");
            fake.claim_for_executor(*id, *node)
                .await
                .expect("fake place");
            fake.force_state(*id, job_state);
        }
    }

    for narrowed in [Some(mine), Some(elsewhere), None] {
        let db = state.jobs.held_on_nodes(narrowed).await.expect("db held");
        let mem = fake.held_on_nodes(narrowed).await.expect("fake held");
        assert_eq!(
            db, mem,
            "the two implementations disagree for held_on_nodes({narrowed:?})"
        );
    }

    let held = state.jobs.held_on_nodes(Some(mine)).await.expect("db held");
    let mine = held.get(&mine).expect("the node holds work");
    assert_eq!(
        (mine.in_flight.len(), mine.waiting_on_human.len()),
        (2, 1),
        "claimed and running are in flight, the paused one is counted apart, \
         and the three terminal rows are held by nobody"
    );

    bed.teardown().await;
}
