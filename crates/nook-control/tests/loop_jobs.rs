//! Loop jobs core (MAIN-127): lifecycle, enqueue-on-create, cancel, transcript,
//! target validation. Each test runs against its OWN private database (MAIN-156
//! TestBed), so the new `0020_loop_jobs` migration is exercised in isolation and
//! never touches the shared dev ledger.
//!
//! Needs Postgres: set `DATABASE_URL` (`NOOK_REQUIRE_DB=1` in the suite).

use std::time::Duration;

use nook_control::services::jobs;
use nook_control::state::AppState;
use nook_db::{params, Db};
use nook_types::*;

use nook_testkit::TestBed;

/// A board with one column to hang tasks on.
async fn board(bed: &TestBed, tenant: TenantId) -> (BoardId, ColumnId) {
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
    let col = ColumnId::new();
    bed.db()
        .exec(
            "INSERT INTO board_columns (id, board_id, name, position, type)
         VALUES ($1, $2, 'Triage', 0, 'unstarted')",
            params![col, board],
        )
        .await
        .expect("column");
    (board, col)
}

/// A task of `type_` (e.g. "task" or "epic") in `board`, created by `creator`,
/// optionally in `workspace`. Team-visible so any tenant user may open a job on
/// it.
async fn task(
    bed: &TestBed,
    tenant: TenantId,
    board: BoardId,
    col: ColumnId,
    type_: &str,
    creator: UserId,
    workspace: Option<WorkspaceId>,
) -> TaskId {
    let id = TaskId::new();
    bed.db()
        .exec(
            "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, type, created_by, workspace_id)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8)",
            params![
                id,
                tenant,
                board,
                col,
                format!("t-{}", id.0.simple()),
                type_,
                creator,
                workspace.map(|w| w.0)
            ],
        )
        .await
        .expect("task");
    id
}

/// tenant + owner user + board/column, ready to open jobs against.
async fn fixture(bed: &TestBed) -> (AppState, TenantId, UserId, BoardId, ColumnId) {
    let tenant = bed.tenant("jobs").await;
    let (user, _person) = bed.user(tenant, "owner").await;
    let (b, c) = board(bed, tenant).await;
    (bed.app_state().await, tenant, user, b, c)
}

#[tokio::test]
async fn create_enqueues_a_work_item_and_records_an_event() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, user, b, c) = fixture(&bed).await;
    let ws = bed.workspace(tenant).await;
    let target = task(&bed, tenant, b, c, "task", user, Some(ws)).await;

    let detail = jobs::create(
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
    let (events,): (i64,) = bed
        .db()
        .query_one(
            "SELECT count(*) FROM events WHERE tenant_id = $1 AND kind = 'job.created'",
            params![tenant],
        )
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
    let plain = task(&bed, tenant, b, c, "task", user, None).await;
    let epic = task(&bed, tenant, b, c, "epic", user, None).await;

    let err = jobs::create(
        &state,
        tenant,
        user,
        CreateLoopJobRequest {
            kind: "decompose".into(),
            target_task_id: plain.to_string(),
            seed: None,
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
            target_task_id: epic.to_string(),
            seed: None,
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
            target_task_id: epic.to_string(),
            seed: None,
        },
    )
    .await
    .expect_err("unknown kind refused");
    assert!(matches!(bad, nook_control::error::ApiError::BadRequest(_)));

    bed.teardown().await;
}

/// MAIN-144 AC-5: the epic-run route guard, exercised through `jobs::create`
/// like decompose's above — a leaf task has no children to merge.
#[tokio::test]
async fn epic_run_requires_an_epic_target() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, user, b, c) = fixture(&bed).await;
    let plain = task(&bed, tenant, b, c, "task", user, None).await;
    let epic = task(&bed, tenant, b, c, "epic", user, None).await;

    let err = jobs::create(
        &state,
        tenant,
        user,
        CreateLoopJobRequest {
            kind: "epic-run".into(),
            target_task_id: plain.to_string(),
            seed: None,
        },
    )
    .await
    .expect_err("epic-run on a non-epic is refused");
    assert!(matches!(err, nook_control::error::ApiError::BadRequest(_)));

    jobs::create(
        &state,
        tenant,
        user,
        CreateLoopJobRequest {
            kind: "epic-run".into(),
            target_task_id: epic.to_string(),
            seed: None,
        },
    )
    .await
    .expect("epic-run on an epic is allowed");

    bed.teardown().await;
}

/// MAIN-383: `build` is a creatable kind, and a card holds ONE live build run.
/// The service refusal names the job already on it (a 409, not a 500), and the
/// 0049 partial unique index is the atomic backstop underneath — both proven
/// here, plus that a terminal run frees the card.
#[tokio::test]
async fn build_jobs_enqueue_and_dedupe_to_one_live_run_per_card() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, user, b, c) = fixture(&bed).await;
    let card = task(&bed, tenant, b, c, "task", user, None).await;

    let detail = jobs::create(
        &state,
        tenant,
        user,
        CreateLoopJobRequest {
            kind: "build".into(),
            target_task_id: card.to_string(),
            seed: None,
        },
    )
    .await
    .expect("a build job enqueues");
    assert_eq!(detail.job.kind, "build");
    assert_eq!(detail.job.state, "queued");

    let err = jobs::create(
        &state,
        tenant,
        user,
        CreateLoopJobRequest {
            kind: "build".into(),
            target_task_id: card.to_string(),
            seed: None,
        },
    )
    .await
    .expect_err("a second live build on the same card is refused");
    match err {
        nook_control::error::ApiError::Conflict(msg) => assert!(
            msg.contains(&detail.job.id.to_string()),
            "the refusal names the job already in flight: {msg}"
        ),
        other => panic!("expected Conflict, got {other:?}"),
    }

    // The index is the atomic version of the same rule: a write that skips the
    // service check entirely is still refused by the database.
    let direct = bed
        .db()
        .exec(
            "INSERT INTO loop_jobs (id, tenant_id, kind, target_task_id, requested_by, state)
             VALUES ($1,$2,'build',$3,$4,'queued')",
            params![JobId::new(), tenant, card, user],
        )
        .await;
    assert!(
        direct.is_err(),
        "the 0050 partial unique index refuses a second live build row"
    );

    // A finished run is not "in flight": the card is buildable again.
    bed.db()
        .exec(
            "UPDATE loop_jobs SET state = 'completed' WHERE id = $1",
            params![detail.job.id],
        )
        .await
        .expect("finish");
    jobs::create(
        &state,
        tenant,
        user,
        CreateLoopJobRequest {
            kind: "build".into(),
            target_task_id: card.to_string(),
            seed: None,
        },
    )
    .await
    .expect("a completed run frees the card");

    bed.teardown().await;
}

#[tokio::test]
async fn lifecycle_allows_legal_transitions_and_refuses_illegal_ones() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, user, b, c) = fixture(&bed).await;
    let target = task(&bed, tenant, b, c, "task", user, None).await;

    let spec = CreateLoopJobRequest {
        kind: "spec".into(),
        target_task_id: target.to_string(),
        seed: None,
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
    let target = task(&bed, tenant, b, c, "task", user, None).await;

    let waiting = jobs::create(
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
    let canceled = jobs::cancel(&state, tenant, user, waiting)
        .await
        .expect("cancel");
    assert_eq!(canceled.state, "canceled");

    // Cancelling an already-canceled job is a no-op success, not a 409.
    let again = jobs::cancel(&state, tenant, user, waiting)
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
            target_task_id: target.to_string(),
            seed: None,
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
    let err = jobs::cancel(&state, tenant, user, done)
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
    let target = task(&bed, tenant, b, c, "task", user, None).await;
    let id = jobs::create(
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
    .unwrap()
    .job
    .id;

    jobs::append_transcript(&state, id, "system", "job started")
        .await
        .unwrap();
    jobs::append_transcript(&state, id, "agent", "thinking...")
        .await
        .unwrap();

    let detail = jobs::get(&state, tenant, user, id).await.expect("get");
    assert_eq!(detail.transcript.len(), 2);
    assert_eq!(detail.transcript[0].content, "job started");
    assert_eq!(detail.transcript[0].source, "system");
    assert_eq!(detail.transcript[1].content, "thinking...");

    // A job from another tenant is invisible even by id.
    let other = bed.tenant("other").await;
    let err = jobs::get(&state, other, user, id)
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
    let epic = task(&bed, tenant, b, c, "epic", user, None).await;
    let orig = jobs::create(
        &state,
        tenant,
        user,
        CreateLoopJobRequest {
            kind: "decompose".into(),
            target_task_id: epic.to_string(),
            seed: None,
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

/// A job whose target is a PRIVATE card is reachable only by someone who can see
/// the card — mirroring the create-side gate onto get/cancel/rerun so a private
/// card's transcript never leaks to another tenant member (review should-fix).
#[tokio::test]
async fn private_target_job_is_hidden_from_a_non_owner() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, owner, b, c) = fixture(&bed).await;
    // A second member of the SAME tenant who does not own the card.
    let (other, _) = bed.user(tenant, "member").await;

    // A private card created by `owner`.
    let secret = TaskId::new();
    bed.db()
        .exec(
            "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, type, created_by, visibility)
         VALUES ($1,$2,$3,$4,'secret','task',$5,'private')",
            params![secret, tenant, b, c, owner],
        )
        .await
        .expect("private task");

    // The owner may open a job on it and read it back.
    let id = jobs::create(
        &state,
        tenant,
        owner,
        CreateLoopJobRequest {
            kind: "spec".into(),
            target_task_id: secret.to_string(),
            seed: None,
        },
    )
    .await
    .expect("owner opens a job on their private card")
    .job
    .id;
    jobs::get(&state, tenant, owner, id)
        .await
        .expect("owner reads it");

    // The other member — same tenant, cannot see the card — is refused on every
    // path, as NotFound (never a distinguishable 403).
    for probe in ["get", "cancel", "rerun"] {
        let err = match probe {
            "get" => jobs::get(&state, tenant, other, id).await.err(),
            "cancel" => jobs::cancel(&state, tenant, other, id).await.err(),
            _ => jobs::rerun(&state, tenant, other, id).await.err(),
        };
        assert!(
            matches!(err, Some(nook_control::error::ApiError::NotFound)),
            "{probe} must be NotFound for a non-owner of a private target"
        );
    }
    // And that member cannot open their own job on the invisible card either.
    let denied = jobs::create(
        &state,
        tenant,
        other,
        CreateLoopJobRequest {
            kind: "spec".into(),
            target_task_id: secret.to_string(),
            seed: None,
        },
    )
    .await
    .expect_err("non-owner cannot open a job on a private card");
    assert!(matches!(denied, nook_control::error::ApiError::NotFound));

    bed.teardown().await;
}

#[tokio::test]
async fn list_for_task_returns_the_tickets_jobs_newest_first_and_is_visibility_gated() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, owner, b, c) = fixture(&bed).await;
    let ws = bed.workspace(tenant).await;
    let target = task(&bed, tenant, b, c, "task", owner, Some(ws)).await;

    // Two jobs on the same ticket (the second re-runs the first, say).
    let first = jobs::create(
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
    .expect("first")
    .job
    .id;
    let second = jobs::create(
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
    .expect("second")
    .job
    .id;

    // The ticket's Loop panel lists both, newest first (v7 ids are time-ordered).
    let listed = jobs::list_for_task(&state, tenant, owner, target)
        .await
        .expect("list");
    assert_eq!(
        listed.iter().map(|j| j.id).collect::<Vec<_>>(),
        vec![second, first],
        "newest first"
    );

    // A private-card ticket's jobs stay private — a non-owner gets NotFound, not
    // an empty list, so the ticket's existence never leaks (MAIN-128 AC-5).
    let (intruder, _p) = bed.user(tenant, "member").await;
    let secret = task(&bed, tenant, b, c, "task", owner, Some(ws)).await;
    bed.db()
        .exec(
            "UPDATE tasks SET visibility = 'private' WHERE id = $1",
            params![secret],
        )
        .await
        .unwrap();
    jobs::create(
        &state,
        tenant,
        owner,
        CreateLoopJobRequest {
            kind: "spec".into(),
            target_task_id: secret.to_string(),
            seed: None,
        },
    )
    .await
    .expect("owner opens a job on their private card");
    let denied = jobs::list_for_task(&state, tenant, intruder, secret).await;
    assert!(matches!(
        denied,
        Err(nook_control::error::ApiError::NotFound)
    ));

    bed.teardown().await;
}

/// The Loop panel opens tickets by board KEY (MAIN-209), so the jobs surface must
/// resolve key-or-uuid like every other task-addressed route — create no longer
/// 422s a key, list no longer 400s one, and an unknown key 404s.
#[tokio::test]
async fn jobs_accept_a_board_key_and_reject_unknown() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("jobs-key").await;
    let (user, _p) = bed.user(tenant, "member").await;
    let (board, col) = board(&bed, tenant).await;

    // A task WITH a number, so it has a resolvable key (the `task` helper leaves
    // `number` null — a keyless fixture that could not be addressed by key).
    let task_id = TaskId::new();
    bed.db()
        .exec(
            "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, type, created_by, number)
         VALUES ($1,$2,$3,$4,$5,$6,$7,42)",
            params![task_id, tenant, board, col, "keyed", "task", user],
        )
        .await
    .expect("numbered task");
    let board_key: String = bed
        .db()
        .query_scalar("SELECT key FROM boards WHERE id = $1", params![board])
        .await
        .unwrap();
    let key = format!("{board_key}-42");

    // create by KEY resolves to the same task (a 422 before this fix).
    let by_key = jobs::create(
        &state,
        tenant,
        user,
        CreateLoopJobRequest {
            kind: "spec".into(),
            target_task_id: key.clone(),
            seed: None,
        },
    )
    .await
    .expect("create by key");
    assert_eq!(by_key.job.target_task_id, Some(task_id));

    // create by UUID string still works (AC-4 parity).
    let by_uuid = jobs::create(
        &state,
        tenant,
        user,
        CreateLoopJobRequest {
            kind: "spec".into(),
            target_task_id: task_id.to_string(),
            seed: None,
        },
    )
    .await
    .expect("create by uuid");
    assert_eq!(by_uuid.job.target_task_id, Some(task_id));

    // an unknown key 404s — not a 422 or a 500.
    let unknown = jobs::create(
        &state,
        tenant,
        user,
        CreateLoopJobRequest {
            kind: "spec".into(),
            target_task_id: format!("{board_key}-9999"),
            seed: None,
        },
    )
    .await;
    assert!(matches!(
        unknown,
        Err(nook_control::error::ApiError::NotFound)
    ));

    // the list route resolves the key the same way (resolve_id → list service):
    // the key names the same task, and its jobs list back.
    let resolved = nook_control::services::tasks::resolve_id(
        &nook_control::repo::tasks::DbTaskRepository::new(bed.db()),
        tenant,
        &key,
    )
    .await
    .expect("resolve key");
    assert_eq!(resolved, task_id);
    let listed = jobs::list_for_task(&state, tenant, user, resolved)
        .await
        .expect("list by resolved key");
    assert_eq!(listed.len(), 2, "both jobs on the keyed task are listed");

    bed.teardown().await;
}

/// The `AND state = 'queued'` guard on the executor claim, which nothing
/// covered until MAIN-255 moved the statement and went looking.
///
/// Dispatch runs on every replica and on a poll, so two dispatchers reaching
/// the same queued job at once is ordinary, not exotic. Without the guard the
/// second `UPDATE` matches and **re-places a job that is already running** —
/// two machines execute the same ticket, and the first executor's finish is
/// attributed to a node that never ran it. Dropping the clause is a four-word
/// edit no other test in this file notices.
#[tokio::test]
async fn a_second_dispatcher_cannot_re_place_a_job_that_is_already_claimed() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, user, b, c) = fixture(&bed).await;
    let target = task(&bed, tenant, b, c, "task", user, None).await;

    use nook_control::repo::jobs::NewLoopJob;
    let id = JobId::new();
    state
        .jobs
        .create(NewLoopJob {
            id,
            tenant,
            kind: "spec".into(),
            target_task_id: Some(target),
            workspace_id: None,
            requested_by: user,
            seed: None,
            predecessor_job_id: None,
            review_pr_number: None,
            review_head_sha: None,
        })
        .await
        .expect("queued job");

    let (first_node, second_node) = (NodeId::new(), NodeId::new());

    let first = state
        .jobs
        .claim_for_executor(id, first_node)
        .await
        .expect("first claim");
    let first = first.expect("the first dispatcher places it");
    assert_eq!(first.executor_node_id, Some(first_node));
    assert_eq!(first.state, "claimed");

    let second = state
        .jobs
        .claim_for_executor(id, second_node)
        .await
        .expect("second claim");
    assert!(
        second.is_none(),
        "the second dispatcher must match no row — otherwise the job is placed \
         twice and runs on two machines"
    );

    let after = state
        .jobs
        .get(tenant, id)
        .await
        .expect("reload")
        .expect("still there");
    assert_eq!(
        after.executor_node_id,
        Some(first_node),
        "without the guard this would now read the SECOND node, stealing a run \
         already in flight"
    );

    bed.teardown().await;
}
