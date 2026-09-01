//! Durable interactions (MAIN-159): the load-bearing properties — the
//! pending→answered lifecycle, first-answer-wins under contention, authorization
//! refused WITHOUT leaking the prompt, visibility-scoped listing, and the
//! anti-spoof gate on creation. Each test runs on its OWN private database
//! (MAIN-156), so assertions need no scope-to-own-rows discipline.
//!
//! Needs Postgres: `DATABASE_URL` (`NOOK_REQUIRE_DB=1` in the suite).

use nook_control::auth::{AuthCtx, Principal};
use nook_control::error::ApiError;
use nook_control::services::interactions;
use nook_db::{params, Db};
use nook_types::*;
use uuid::Uuid;

use nook_testkit::TestBed;

/// A board + column + a task with the given visibility, created by `creator`.
async fn task(bed: &TestBed, tenant: TenantId, creator: UserId, visibility: &str) -> TaskId {
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
    let id = TaskId::new();
    bed.db()
        .exec(
            "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, type, created_by, visibility)
         VALUES ($1,$2,$3,$4,'t','task',$5,$6)",
            params![id, tenant, board, col, creator, visibility],
        )
        .await
        .expect("task");
    id
}

/// A claimed job on `target`, executed by `node` — the shape the anti-spoof gate
/// checks against.
async fn claimed_job(
    bed: &TestBed,
    tenant: TenantId,
    user: UserId,
    target: TaskId,
    node: NodeId,
) -> JobId {
    let id = JobId::new();
    bed.db()
        .exec(
            "INSERT INTO loop_jobs (id, tenant_id, kind, target_task_id, requested_by, state, executor_node_id)
         VALUES ($1,$2,'spec',$3,$4,'claimed',$5)",
            params![id, tenant, target, user, node],
        )
        .await
        .expect("job");
    id
}

/// A job on `target`, executed by `node`, in the given lifecycle state — the
/// bridging tests need a `running` job (the state a mid-run ask fires from).
async fn job_in_state(
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

/// The job's current lifecycle state, read straight from the row — what a
/// restarted control plane would see, since the pause is database state.
async fn job_state(bed: &TestBed, id: JobId) -> String {
    bed.db()
        .query_scalar("SELECT state FROM loop_jobs WHERE id = $1", params![id])
        .await
        .expect("job state")
}

/// The concatenated transcript content for a job, oldest first.
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

async fn node(bed: &TestBed, tenant: TenantId) -> NodeId {
    let id = NodeId::new();
    bed.db()
        .exec(
            "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status)
         VALUES ($1,$2,$3,$4,'offline')",
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

fn user_ctx(tenant: TenantId, user: UserId) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId(Uuid::now_v7()),
        user_id: user,
        tenant_id: tenant,
        principal: Principal::User,
        cookie_session: true,
    }
}

fn node_ctx(tenant: TenantId, user: UserId, n: NodeId) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId(n.0),
        user_id: user,
        tenant_id: tenant,
        principal: Principal::Node(n),
        cookie_session: false,
    }
}

fn ask(prompt: &str) -> CreateInteractionRequest {
    CreateInteractionRequest {
        prompt: prompt.into(),
        choices: None,
        job_id: None,
        task_id: None,
        session_id: None,
    }
}

#[tokio::test]
async fn pending_answered_lifecycle() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ix").await;
    let (user, _p) = bed.user(tenant, "owner").await;
    let t = task(&bed, tenant, user, "team").await;
    let state = bed.app_state().await;

    let req = CreateInteractionRequest {
        task_id: Some(t),
        choices: Some(vec!["yes".into(), "no".into()]),
        ..ask("Deploy?")
    };
    let created = interactions::create(&state, tenant, &user_ctx(tenant, user), req)
        .await
        .expect("create");
    assert_eq!(created.state, "pending");
    assert_eq!(
        created.choices.as_deref(),
        Some(&["yes".to_string(), "no".to_string()][..])
    );

    let answered = interactions::answer(&state, tenant, user, created.id, "yes".into())
        .await
        .expect("answer");
    assert_eq!(answered.state, "answered");
    assert_eq!(answered.response.as_deref(), Some("yes"));
    assert_eq!(answered.answered_by, Some(user));

    // Pullable: a later read (what `ask --wait` polls) sees the answer.
    let pulled = interactions::get(&state, tenant, &user_ctx(tenant, user), created.id)
        .await
        .expect("get");
    assert_eq!(pulled.state, "answered");
    assert_eq!(pulled.response.as_deref(), Some("yes"));

    // Answering again is refused — it is no longer pending.
    let again = interactions::answer(&state, tenant, user, created.id, "no".into()).await;
    assert!(
        matches!(again, Err(ApiError::Conflict(_))),
        "second answer is a conflict"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn first_answer_wins_under_concurrency() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ix").await;
    let (u1, _p1) = bed.user(tenant, "owner").await;
    let (u2, _p2) = bed.user(tenant, "member").await;
    let t = task(&bed, tenant, u1, "team").await; // team-visible to both
    let state = bed.app_state().await;

    let created = interactions::create(
        &state,
        tenant,
        &user_ctx(tenant, u1),
        CreateInteractionRequest {
            task_id: Some(t),
            ..ask("Which one?")
        },
    )
    .await
    .expect("create");

    // Two people answer the same pending interaction at once.
    let (a, b) = tokio::join!(
        interactions::answer(&state, tenant, u1, created.id, "u1".into()),
        interactions::answer(&state, tenant, u2, created.id, "u2".into()),
    );

    // Exactly one wins; the other gets a clean conflict.
    let wins = [a.is_ok(), b.is_ok()].iter().filter(|x| **x).count();
    let conflicts = [&a, &b]
        .iter()
        .filter(|r| matches!(r, Err(ApiError::Conflict(_))))
        .count();
    assert_eq!(wins, 1, "exactly one answer wins");
    assert_eq!(
        conflicts, 1,
        "the loser gets a conflict, not a silent overwrite"
    );

    // The stored answer is the winner's, set once.
    let final_i = interactions::get(&state, tenant, &user_ctx(tenant, u1), created.id)
        .await
        .expect("get");
    assert_eq!(final_i.state, "answered");
    let winner = if a.is_ok() { u1 } else { u2 };
    assert_eq!(final_i.answered_by, Some(winner));

    bed.teardown().await;
}

#[tokio::test]
async fn a_private_subject_refuses_without_leaking_the_prompt() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ix").await;
    let (owner, _po) = bed.user(tenant, "owner").await; // creator of the private card
    let (other, _px) = bed.user(tenant, "member").await; // a teammate who cannot see it
    let private = task(&bed, tenant, owner, "private").await;
    let state = bed.app_state().await;

    // The card's creator can raise an interaction on it.
    let created = interactions::create(
        &state,
        tenant,
        &user_ctx(tenant, owner),
        CreateInteractionRequest {
            task_id: Some(private),
            ..ask("secret prompt")
        },
    )
    .await
    .expect("owner can create on their own private card");

    // A teammate who cannot see the card is refused as NotFound — never the
    // prompt — on both read and answer.
    let read = interactions::get(&state, tenant, &user_ctx(tenant, other), created.id).await;
    assert!(
        matches!(read, Err(ApiError::NotFound)),
        "read refused as NotFound"
    );
    let ans = interactions::answer(&state, tenant, other, created.id, "peek".into()).await;
    assert!(
        matches!(ans, Err(ApiError::NotFound)),
        "answer refused as NotFound"
    );

    // And it stayed pending — the refused answer did not take.
    let still = interactions::get(&state, tenant, &user_ctx(tenant, owner), created.id)
        .await
        .expect("owner get");
    assert_eq!(still.state, "pending");

    bed.teardown().await;
}

#[tokio::test]
async fn list_pending_is_visibility_scoped() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ix").await;
    let (owner, _po) = bed.user(tenant, "owner").await;
    let (other, _px) = bed.user(tenant, "member").await;
    let team = task(&bed, tenant, owner, "team").await;
    let private = task(&bed, tenant, owner, "private").await;
    let state = bed.app_state().await;

    let i_team = interactions::create(
        &state,
        tenant,
        &user_ctx(tenant, owner),
        CreateInteractionRequest {
            task_id: Some(team),
            ..ask("team q")
        },
    )
    .await
    .expect("team");
    let i_priv = interactions::create(
        &state,
        tenant,
        &user_ctx(tenant, owner),
        CreateInteractionRequest {
            task_id: Some(private),
            ..ask("private q")
        },
    )
    .await
    .expect("priv");
    let i_free = interactions::create(
        &state,
        tenant,
        &user_ctx(tenant, owner),
        ask("standalone q"),
    )
    .await
    .expect("free");

    // The teammate sees the team-visible and the subject-less ones, NOT the
    // private-card one.
    let seen: Vec<_> = interactions::list_pending(&state, tenant, other)
        .await
        .expect("list")
        .into_iter()
        .map(|i| i.id)
        .collect();
    assert!(
        seen.contains(&i_team.id),
        "team-visible interaction is listed"
    );
    assert!(
        seen.contains(&i_free.id),
        "subject-less interaction is tenant-wide"
    );
    assert!(
        !seen.contains(&i_priv.id),
        "the private-card interaction is hidden"
    );

    // The owner sees all three.
    let owner_seen: Vec<_> = interactions::list_pending(&state, tenant, owner)
        .await
        .expect("list")
        .into_iter()
        .map(|i| i.id)
        .collect();
    assert!([i_team.id, i_priv.id, i_free.id]
        .iter()
        .all(|id| owner_seen.contains(id)));

    bed.teardown().await;
}

#[tokio::test]
async fn a_node_cannot_pause_a_job_it_does_not_run() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ix").await;
    let (user, _p) = bed.user(tenant, "owner").await;
    let t = task(&bed, tenant, user, "team").await;
    let runner = node(&bed, tenant).await;
    let intruder = node(&bed, tenant).await;
    let job = claimed_job(&bed, tenant, user, t, runner).await;
    let state = bed.app_state().await;

    let job_ask = |job_id: JobId| CreateInteractionRequest {
        job_id: Some(job_id),
        ..ask("continue?")
    };

    // A node that is NOT the executor is refused (anti-spoof, AC-4).
    let spoof = interactions::create(
        &state,
        tenant,
        &node_ctx(tenant, user, intruder),
        job_ask(job),
    )
    .await;
    assert!(
        matches!(spoof, Err(ApiError::ForbiddenMsg(_))),
        "a non-executor node is refused"
    );

    // The actual executor may raise it, and the subject is the job's target.
    let ok = interactions::create(
        &state,
        tenant,
        &node_ctx(tenant, user, runner),
        job_ask(job),
    )
    .await
    .expect("the executor can pause its own job");
    assert_eq!(ok.job_id, Some(job));
    assert_eq!(
        ok.task_id,
        Some(t),
        "the subject is the job's target ticket"
    );
    assert_eq!(ok.requested_by_node_id, Some(runner));

    // Answering it drives the push path (best-effort to an offline node — must
    // not error) and unblocks the pull.
    let answered = interactions::answer(&state, tenant, user, ok.id, "go".into())
        .await
        .expect("answer succeeds even though the node is offline");
    assert_eq!(answered.response.as_deref(), Some("go"));

    bed.teardown().await;
}

#[tokio::test]
async fn a_node_pulls_its_own_answer_even_on_a_private_card() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ix").await;
    // The node's user identity (a node authenticates as the tenant owner) is
    // deliberately NOT the private card's creator, so only the requester-bypass
    // — not the node's user visibility — can resolve the pull.
    let (node_user, _pu) = bed.user(tenant, "owner").await;
    let (author, _pa) = bed.user(tenant, "member").await;
    let private = task(&bed, tenant, author, "private").await;
    let runner = node(&bed, tenant).await;
    let job = claimed_job(&bed, tenant, author, private, runner).await;
    let state = bed.app_state().await;

    // The executor raises a job-scoped pause on the private card.
    let created = interactions::create(
        &state,
        tenant,
        &node_ctx(tenant, node_user, runner),
        CreateInteractionRequest {
            job_id: Some(job),
            ..ask("continue?")
        },
    )
    .await
    .expect("the executor raises the pause");
    assert_eq!(created.requested_by_node_id, Some(runner));

    // The card's author (who can see it) answers.
    interactions::answer(&state, tenant, author, created.id, "go".into())
        .await
        .expect("author answers");

    // The requesting node pulls its own answer — despite having no user identity
    // that could see the private card. This is what `ask --wait` relies on.
    let pulled = interactions::get(
        &state,
        tenant,
        &node_ctx(tenant, node_user, runner),
        created.id,
    )
    .await
    .expect("the requesting node pulls its answer");
    assert_eq!(pulled.state, "answered");
    assert_eq!(pulled.response.as_deref(), Some("go"));

    // And the node's plain user identity cannot: prove the bypass is scoped to
    // the requesting NODE, not to its user (who cannot see the private card).
    let as_user = interactions::get(&state, tenant, &user_ctx(tenant, node_user), created.id).await;
    assert!(
        matches!(as_user, Err(ApiError::NotFound)),
        "the same user, not acting as the requesting node, is still refused"
    );

    bed.teardown().await;
}

// ── Job bridging (MAIN-162) ──────────────────────────────────────────────────

#[tokio::test]
async fn a_job_scoped_ask_pauses_and_the_answer_resumes_the_run() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ix").await;
    let (user, _p) = bed.user(tenant, "owner").await;
    let t = task(&bed, tenant, user, "team").await;
    let runner = node(&bed, tenant).await;
    // A RUNNING job — the state a mid-run ask fires from.
    let job = job_in_state(&bed, tenant, user, t, runner, "running").await;
    let state = bed.app_state().await;

    // The executor raises a job-scoped ask → the job pauses (AC-1) and the
    // question lands on the transcript.
    let created = interactions::create(
        &state,
        tenant,
        &node_ctx(tenant, user, runner),
        CreateInteractionRequest {
            job_id: Some(job),
            choices: Some(vec!["Postgres".into(), "Redis".into()]),
            ..ask("Which store?")
        },
    )
    .await
    .expect("the executor raises the pause");
    assert_eq!(created.state, "pending");

    // Pause is DATABASE state — a restarted CP would read it straight from the row.
    assert_eq!(
        job_state(&bed, job).await,
        "waiting_on_human",
        "a job-scoped ask pauses the run (persisted)"
    );
    let after_ask = transcript_text(&bed, job).await;
    assert!(
        after_ask.contains("Which store?") && after_ask.contains("Postgres, Redis"),
        "the question (with choices) is on the transcript: {after_ask:?}"
    );

    // A human answers → the run resumes to running (AC-1) and the answer is
    // recorded on the transcript.
    interactions::answer(&state, tenant, user, created.id, "Postgres".into())
        .await
        .expect("answer");
    assert_eq!(
        job_state(&bed, job).await,
        "running",
        "answering resumes the paused run"
    );
    assert!(
        transcript_text(&bed, job)
            .await
            .contains("answered: Postgres"),
        "the answer is on the transcript"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn failing_a_job_cancels_its_pending_ask_and_answering_reports_it() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ix").await;
    let (user, _p) = bed.user(tenant, "owner").await;
    let t = task(&bed, tenant, user, "team").await;
    let runner = node(&bed, tenant).await;
    let job = job_in_state(&bed, tenant, user, t, runner, "running").await;
    let state = bed.app_state().await;

    let created = interactions::create(
        &state,
        tenant,
        &node_ctx(tenant, user, runner),
        CreateInteractionRequest {
            job_id: Some(job),
            ..ask("continue?")
        },
    )
    .await
    .expect("pause raised");
    assert_eq!(job_state(&bed, job).await, "waiting_on_human");

    // The job fails (a node reporting failure) → its pending ask is canceled
    // (AC-3), because a paused ask on dead work is moot.
    nook_control::services::jobs::finish(&state, tenant, job, false, "the run crashed")
        .await
        .expect("finish");
    let reloaded = interactions::get(&state, tenant, &node_ctx(tenant, user, runner), created.id)
        .await
        .expect("get");
    assert_eq!(
        reloaded.state, "canceled",
        "the job's failure cancels its ask"
    );

    // Answering the now-canceled ask reports it clearly — not "already answered".
    let err = interactions::answer(&state, tenant, user, created.id, "go".into()).await;
    match err {
        Err(ApiError::Conflict(msg)) => assert!(
            msg.contains("canceled"),
            "the conflict names the cancellation, not a stale answer: {msg:?}"
        ),
        other => panic!("expected a canceled-report conflict, got {other:?}"),
    }

    bed.teardown().await;
}
