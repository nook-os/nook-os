//! The board is the build loop's work source (MAIN-458).
//!
//! What these pin: the pick contract as code (only an `agent-ready`,
//! unassigned, unblocked card on a board column raises a run), the AC-3 board
//! mechanics (claim-on-raise, In Review on `pr_opened`, comment+label+release
//! on `blocked`), outcome gating (a card is consumed only by a recorded
//! outcome), and dedupe against a live run. The `owed()` wakeup rule itself —
//! holds, backoff expiry, replica determinism — is pinned by
//! `run_reconcile`'s own tests and is deliberately not re-proved here.
//!
//! Engine-neutral (MAIN-264): nothing here names a `sqlx` type.

use nook_control::routes::task_query::{query_rows, TaskFilter};
use nook_control::services::jobs;
use nook_control::services::kanban::{KanbanProvider, LocalBoardProvider};
use nook_db::{params, Db, DbPool};
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

/// A board with the three columns the mechanics move a card across.
async fn board_fixture(db: &DbPool, tenant: TenantId) -> BoardId {
    let board = BoardId(Uuid::now_v7());
    db.exec(
        "INSERT INTO boards (id, tenant_id, name, key, provider) VALUES ($1,$2,'b','BC','local')",
        params![board, tenant],
    )
    .await
    .expect("board");
    for (i, (name, ty)) in [
        ("Todo", "unstarted"),
        ("Doing", "started"),
        ("Rev", "review"),
    ]
    .iter()
    .enumerate()
    {
        db.exec(
            "INSERT INTO board_columns (id, board_id, name, position, type)
             VALUES ($1, $2, $3, $4, $5)",
            params![Uuid::now_v7(), board, *name, i as i32, *ty],
        )
        .await
        .expect("column");
    }
    board
}

async fn approved_card(
    db: &DbPool,
    tenant: TenantId,
    board: BoardId,
    ws: WorkspaceId,
    user: UserId,
    title: &str,
) -> TaskItem {
    let provider = LocalBoardProvider {
        repo: std::sync::Arc::new(nook_control::repo::tasks::DbTaskRepository::new(db.clone())),
    };
    let task = provider
        .create_task(
            tenant,
            board,
            Some(user),
            CreateTaskRequest {
                title: title.into(),
                description: Some("## AC-1 — do the thing".into()),
                column_id: None,
                column_type: Some("unstarted".into()),
                workspace_id: Some(ws),
                priority: None,
                type_: None,
                visibility: None,
                parent: None,
                labels: vec!["agent-ready".into()],
            },
        )
        .await
        .expect("card");
    task
}

#[tokio::test]
async fn an_approved_card_raises_one_claimed_run_and_only_one() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("bconv").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let board = board_fixture(&bed.db(), tenant).await;
    let card = approved_card(&bed.db(), tenant, board, ws, user, "build me").await;

    let c = jobs::converge_builds(&state, tenant, user, ws, None)
        .await
        .expect("converge");
    assert_eq!(c.raised, 1, "one approved card, one run");
    let job = &c.jobs[0];
    assert_eq!(job.kind, "build");
    assert_eq!(job.target_task_id, Some(card.id));
    assert!(
        job.build_fingerprint.is_some(),
        "the run records what the card looked like"
    );

    // AC-3: the CP claimed the card when raising — assigned, and moved to the
    // started column, which also removes it from the pick contract.
    let row = state
        .tasks
        .get_row(tenant, card.id)
        .await
        .expect("read")
        .expect("card");
    assert_eq!(row.assignee_user_id, Some(user), "claimed");

    // Converging again raises nothing: the card is assigned (out of the pick)
    // AND its run is live (dedupe) — either alone suffices.
    let again = jobs::converge_builds(&state, tenant, user, ws, None)
        .await
        .expect("converge 2");
    assert_eq!(again.raised, 0, "no second run for a live card");

    bed.teardown().await;
}

/// The approval gate as code (NG-1): the same card WITHOUT `agent-ready` — or
/// with `blocked` beside it — raises nothing, and nothing here ever applies
/// the label.
#[tokio::test]
async fn the_pick_contract_gates_the_converger() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("bgate").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let board = board_fixture(&bed.db(), tenant).await;

    // A card with no approval label at all.
    let provider = LocalBoardProvider {
        repo: std::sync::Arc::new(nook_control::repo::tasks::DbTaskRepository::new(bed.db())),
    };
    provider
        .create_task(
            tenant,
            board,
            Some(user),
            CreateTaskRequest {
                title: "not approved".into(),
                description: None,
                column_id: None,
                column_type: Some("unstarted".into()),
                workspace_id: Some(ws),
                priority: None,
                type_: None,
                visibility: None,
                parent: None,
                labels: vec![],
            },
        )
        .await
        .expect("card");

    // An approved card that is also blocked.
    let blocked = approved_card(&bed.db(), tenant, board, ws, user, "held").await;
    state
        .tasks
        .attach_label(tenant, blocked.id, "blocked")
        .await
        .expect("blocked label");

    let c = jobs::converge_builds(&state, tenant, user, ws, None)
        .await
        .expect("converge");
    assert_eq!(c.raised, 0, "neither card passes the pick contract");

    bed.teardown().await;
}

/// The other end of the contract (MAIN-464): a card that finished with
/// `agent-ready` still on it raises nothing. The label is the human's to
/// remove (NG-1), so the column has to be what decides — and it does here only
/// because `BuildWork` asks the pick query rather than filtering items itself.
#[tokio::test]
async fn a_finished_card_raises_nothing_even_with_agent_ready() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("bdone").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let board = board_fixture(&bed.db(), tenant).await;

    let done_col = ColumnId(Uuid::now_v7());
    bed.db()
        .exec(
            "INSERT INTO board_columns (id, board_id, name, position, type)
             VALUES ($1, $2, 'Done', 3, 'completed')",
            params![done_col, board],
        )
        .await
        .expect("done column");

    let card = approved_card(&bed.db(), tenant, board, ws, user, "already shipped").await;
    bed.db()
        .exec(
            "UPDATE tasks SET column_id = $1 WHERE id = $2",
            params![done_col, card.id],
        )
        .await
        .expect("move to Done");

    let c = jobs::converge_builds(&state, tenant, user, ws, None)
        .await
        .expect("converge");
    assert_eq!(c.raised, 0, "a shipped card is not work to raise a run for");

    bed.teardown().await;
}

#[tokio::test]
async fn outcomes_mirror_to_the_board_and_consume_the_card() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("boutc").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let board = board_fixture(&bed.db(), tenant).await;
    let card = approved_card(&bed.db(), tenant, board, ws, user, "outcome me").await;

    let c = jobs::converge_builds(&state, tenant, user, ws, None)
        .await
        .expect("converge");
    assert_eq!(c.raised, 1);
    let job = c.jobs[0].clone();
    // The run must be LIVE to record an outcome; queued counts (claimed by
    // the queue consumer in production, but the guard accepts live states).
    state
        .jobs
        .transition(job.id, "running")
        .await
        .expect("to running");

    // pr_opened: the PR lands on the card and the card parks in In Review.
    let done = jobs::record_build_outcome(
        &state,
        tenant,
        job.id,
        &BuildOutcomeRequest {
            outcome: "pr_opened".into(),
            url: Some("https://github.com/acme/api/pull/7".into()),
            question: None,
        },
    )
    .await
    .expect("outcome");
    assert_eq!(done.build_outcome.as_deref(), Some("pr_opened"));
    let row = state
        .tasks
        .get_row(tenant, card.id)
        .await
        .expect("read")
        .expect("card");
    assert_eq!(
        row.pr_url.as_deref(),
        Some("https://github.com/acme/api/pull/7")
    );

    // Consumed: the same content raises nothing more (outcome-gating, AC-2) —
    // the card is out of the pick contract (assigned + in review), and even
    // its heads say "done at this fingerprint".
    let again = jobs::converge_builds(&state, tenant, user, ws, None)
        .await
        .expect("converge 2");
    assert_eq!(again.raised, 0);

    bed.teardown().await;
}

#[tokio::test]
async fn a_blocked_outcome_hands_the_card_back_with_the_question() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("bblok").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let board = board_fixture(&bed.db(), tenant).await;
    let card = approved_card(&bed.db(), tenant, board, ws, user, "ambiguous").await;

    let c = jobs::converge_builds(&state, tenant, user, ws, None)
        .await
        .expect("converge");
    let job = c.jobs[0].clone();
    state
        .jobs
        .transition(job.id, "running")
        .await
        .expect("to running");

    jobs::record_build_outcome(
        &state,
        tenant,
        job.id,
        &BuildOutcomeRequest {
            outcome: "blocked".into(),
            url: None,
            question: Some("AC-2 conflicts with NG-1 — which wins?".into()),
        },
    )
    .await
    .expect("outcome");

    let row = state
        .tasks
        .get_row(tenant, card.id)
        .await
        .expect("read")
        .expect("card");
    assert_eq!(row.assignee_user_id, None, "the claim came back");
    let comments = state.tasks.comments_of(card.id).await.expect("comments");
    assert!(
        comments
            .iter()
            .any(|c| c.body_md.contains("which wins?") && c.author_name == "nook-build loop"),
        "the question is durable on the card"
    );
    // The `blocked` label now gates the pick, so the card reappears only
    // after a human answers and removes it — nothing more raised now.
    let again = jobs::converge_builds(&state, tenant, user, ws, None)
        .await
        .expect("converge 2");
    assert_eq!(again.raised, 0);

    // And the pick query a human runs agrees about why.
    let rows = query_rows(
        state.tasks.as_ref(),
        tenant,
        user,
        &TaskFilter {
            label: vec!["agent-ready".into()],
            not_label: vec!["blocked".into()],
            assignee: Some("none".into()),
            workspace: Some(ws.0),
            ..Default::default()
        },
    )
    .await
    .expect("pick");
    assert!(
        rows.is_empty(),
        "the card is out of the pick until unblocked"
    );

    bed.teardown().await;
}

/// An inconclusive run — completed with NO outcome — does not consume the
/// card: after the claim is released it is owed again (held only by the
/// shared backoff, whose window `run_reconcile`'s tests own).
#[tokio::test]
async fn a_run_that_concluded_nothing_does_not_consume_the_card() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("bhold").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let board = board_fixture(&bed.db(), tenant).await;
    let card = approved_card(&bed.db(), tenant, board, ws, user, "flaky").await;

    let c = jobs::converge_builds(&state, tenant, user, ws, None)
        .await
        .expect("converge");
    let job = c.jobs[0].clone();
    state.jobs.transition(job.id, "running").await.expect("run");
    // The run dies with no outcome recorded; the claim comes back the way the
    // reaper/failure path returns it.
    state
        .jobs
        .transition(job.id, "completed")
        .await
        .expect("die");
    state
        .tasks
        .release_assignment(card.id, tenant)
        .await
        .expect("release");

    let heads = state.jobs.build_run_heads(tenant, ws).await.expect("heads");
    assert_eq!(heads.len(), 1);
    assert!(
        heads[0].done_head.is_none(),
        "no outcome, so nothing is 'done' — the card is NOT consumed"
    );
    assert!(
        heads[0].attempted_head.is_some(),
        "…but the attempt is recorded, which is what the backoff holds on"
    );

    bed.teardown().await;
}

/// A forge that answers a fixed PR list — enough to drive the repair source
/// without a network.
struct StaticForge(Vec<nook_control::services::forge::PullRequest>);

/// A forge whose `pr_details` the test dictates — the seam the outcome call's
/// Closes check reads through when the workspace has no token of its own.
struct BodyForge(Result<String, String>);

#[async_trait::async_trait]
impl nook_control::services::forge::Forge for BodyForge {
    async fn prs_needing_review(
        &self,
        _repo: &nook_control::services::forge::Repo,
    ) -> anyhow::Result<Vec<nook_control::services::forge::PullRequest>> {
        Ok(vec![])
    }
    async fn pr_details(
        &self,
        _repo: &nook_control::services::forge::Repo,
        _number: u64,
    ) -> anyhow::Result<nook_control::services::forge::PrDetails> {
        match &self.0 {
            Ok(body) => Ok(nook_control::services::forge::PrDetails {
                mergeable: Some(true),
                body: body.clone(),
                merge_state: nook_control::services::forge::MergeState::Open,
            }),
            Err(e) => anyhow::bail!("{e}"),
        }
    }
}

/// Point the deployment forge at a dictated PR body (or a failure), against a
/// remote whose CANONICAL casing differs from the lowercased stored form —
/// which is the shape `gh pr create` echoes back.
async fn with_body_forge(
    bed: &TestBed,
    state: &nook_control::state::AppState,
    ws: WorkspaceId,
    remote: &str,
    body: Result<String, String>,
) -> nook_control::state::AppState {
    bed.db()
        .exec(
            "UPDATE workspaces SET git_remote_url = $2 WHERE id = $1",
            params![ws, remote],
        )
        .await
        .expect("remote");
    let mut state = state.clone();
    state.review_demand = std::sync::Arc::new(nook_control::services::forge::ReviewDemand::new(
        Some(Box::new(BodyForge(body))),
        std::time::Duration::ZERO,
    ));
    state
}

#[async_trait::async_trait]
impl nook_control::services::forge::Forge for StaticForge {
    async fn prs_needing_review(
        &self,
        _repo: &nook_control::services::forge::Repo,
    ) -> anyhow::Result<Vec<nook_control::services::forge::PullRequest>> {
        Ok(self.0.clone())
    }
}

/// Wire a workspace to a GitHub remote and swap the state's demand for a
/// fake, so `repair_items` has a forge to ask.
async fn with_fake_forge(
    bed: &TestBed,
    state: &nook_control::state::AppState,
    ws: WorkspaceId,
    prs: Vec<nook_control::services::forge::PullRequest>,
) -> nook_control::state::AppState {
    bed.db()
        .exec(
            "UPDATE workspaces SET git_remote_url = 'git@github.com:acme/api.git' WHERE id = $1",
            params![ws],
        )
        .await
        .expect("remote");
    let mut state = state.clone();
    state.review_demand = std::sync::Arc::new(nook_control::services::forge::ReviewDemand::new(
        Some(Box::new(StaticForge(prs))),
        std::time::Duration::ZERO,
    ));
    state
}

/// MAIN-458 AC-1b/AC-6: a repair item exists for a `loop-changes-requested`
/// PR, fingerprinted on the head the reviewer REJECTED — never the PR's
/// current head, which the repair's own push moves. Answered, it rests; a NEW
/// rejected head re-raises; and its outcome never touches the card's claim
/// or the fresh-build record.
#[tokio::test]
async fn repair_items_key_on_the_rejected_head_and_stay_in_their_lane() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("brepair").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let board = board_fixture(&bed.db(), tenant).await;
    // The card under repair: it has a PR recorded and is NOT agent-ready —
    // parked in In Review the way `pr_opened` parks it.
    let card = {
        let provider = LocalBoardProvider {
            repo: std::sync::Arc::new(nook_control::repo::tasks::DbTaskRepository::new(bed.db())),
        };
        provider
            .create_task(
                tenant,
                board,
                Some(user),
                CreateTaskRequest {
                    title: "under repair".into(),
                    description: None,
                    column_id: None,
                    column_type: Some("review".into()),
                    workspace_id: Some(ws),
                    priority: None,
                    type_: None,
                    visibility: None,
                    parent: None,
                    labels: vec![],
                },
            )
            .await
            .expect("card")
    };
    bed.db()
        .exec(
            "UPDATE tasks SET pr_url = 'https://github.com/acme/api/pull/7' WHERE id = $1",
            params![card.id],
        )
        .await
        .expect("pr_url");

    // The rejection on record: a review run that concluded changes_requested
    // at head rej1.
    let review = state
        .jobs
        .create(nook_control::repo::jobs::NewLoopJob {
            id: JobId::new(),
            tenant,
            kind: "review".into(),
            target_task_id: None,
            workspace_id: Some(ws),
            requested_by: user,
            seed: None,
            predecessor_job_id: None,
            review_pr_number: Some(7),
            review_head_sha: Some("rej1".into()),
            review_forced: false,
            build_fingerprint: None,
        })
        .await
        .expect("review run");
    state
        .jobs
        .transition(review.id, "running")
        .await
        .expect("running");
    state
        .jobs
        .set_review_verdict(review.id, "changes_requested")
        .await
        .expect("verdict");
    state
        .jobs
        .transition(review.id, "completed")
        .await
        .expect("done");

    // The PR's CURRENT head has already moved (our own push): the fingerprint
    // must ignore it and key on the rejection.
    let state = with_fake_forge(
        &bed,
        &state,
        ws,
        vec![nook_control::services::forge::PullRequest {
            number: 7,
            head_sha: "moved9".into(),
            labels: vec!["loop-changes-requested".into()],
        }],
    )
    .await;

    let c = jobs::converge_builds(&state, tenant, user, ws, None)
        .await
        .expect("converge");
    assert_eq!(c.raised, 1, "one labeled PR, one repair run");
    let job = c.jobs[0].clone();
    assert_eq!(
        job.build_fingerprint.as_deref(),
        Some("repair:rej1"),
        "fingerprinted on the REJECTED head, not the moved one"
    );
    let row = state
        .tasks
        .get_row(tenant, card.id)
        .await
        .expect("read")
        .expect("card");
    assert_eq!(
        row.assignee_user_id, None,
        "a repair item never claims the card"
    );

    // The run concludes nothing_to_do: recorded, and the card's (absent)
    // claim is left alone — the release is only ever undoing the loop's own.
    state
        .jobs
        .transition(job.id, "running")
        .await
        .expect("running");
    jobs::record_build_outcome(
        &state,
        tenant,
        job.id,
        &BuildOutcomeRequest {
            outcome: "nothing_to_do".into(),
            url: None,
            question: None,
        },
    )
    .await
    .expect("outcome");
    // The node reports the run finished; the outcome above is what makes this
    // completion COUNT (a completed run with no outcome is held, not done).
    state
        .jobs
        .transition(job.id, "completed")
        .await
        .expect("finished");

    // Answered at rej1: nothing more owed, even though the label is still on.
    let again = jobs::converge_builds(&state, tenant, user, ws, None)
        .await
        .expect("converge 2");
    assert_eq!(again.raised, 0, "the same rejected head is answered");

    // A NEW rejection re-raises (AC-6): a fresh changes_requested at rej2.
    let review2 = state
        .jobs
        .create(nook_control::repo::jobs::NewLoopJob {
            id: JobId::new(),
            tenant,
            kind: "review".into(),
            target_task_id: None,
            workspace_id: Some(ws),
            requested_by: user,
            seed: None,
            predecessor_job_id: None,
            review_pr_number: Some(7),
            review_head_sha: Some("rej2".into()),
            review_forced: false,
            build_fingerprint: None,
        })
        .await
        .expect("review 2");
    state
        .jobs
        .transition(review2.id, "running")
        .await
        .expect("running");
    state
        .jobs
        .set_review_verdict(review2.id, "changes_requested")
        .await
        .expect("verdict 2");
    state
        .jobs
        .transition(review2.id, "completed")
        .await
        .expect("done 2");

    let third = jobs::converge_builds(&state, tenant, user, ws, None)
        .await
        .expect("converge 3");
    assert_eq!(third.raised, 1, "a new rejected head re-raises");
    assert_eq!(
        third.jobs[0].build_fingerprint.as_deref(),
        Some("repair:rej2")
    );

    // The FRESH side of the lane split: answer the rej2 repair, then approve
    // the same card — the fresh pick raises its own run in its own
    // fingerprint space, undisturbed by the repair bookkeeping.
    let rep2 = third.jobs[0].clone();
    state
        .jobs
        .transition(rep2.id, "running")
        .await
        .expect("running");
    jobs::record_build_outcome(
        &state,
        tenant,
        rep2.id,
        &BuildOutcomeRequest {
            outcome: "nothing_to_do".into(),
            url: None,
            question: None,
        },
    )
    .await
    .expect("outcome 2");
    state
        .jobs
        .transition(rep2.id, "completed")
        .await
        .expect("finished 2");

    state
        .tasks
        .attach_label(tenant, card.id, "agent-ready")
        .await
        .expect("approve");
    let fresh = jobs::converge_builds(&state, tenant, user, ws, None)
        .await
        .expect("converge 4");
    assert_eq!(fresh.raised, 1, "the fresh lane raises independently");
    let fp = fresh.jobs[0]
        .build_fingerprint
        .clone()
        .expect("fingerprint");
    assert!(
        !fp.starts_with("repair:"),
        "a fresh run's fingerprint is the card's content, not a repair head: {fp}"
    );

    bed.teardown().await;
}

/// The manual trigger raises exactly once (AC-4/AC-6): the second enqueue of
/// the same card converges to nothing because the first run is live.
#[tokio::test]
async fn the_manual_trigger_raises_exactly_once() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("bmanual").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let board = board_fixture(&bed.db(), tenant).await;
    let card = approved_card(&bed.db(), tenant, board, ws, user, "on demand").await;

    let first = jobs::converge_builds(&state, tenant, user, ws, Some(card.id))
        .await
        .expect("enqueue");
    assert_eq!(first.raised, 1);
    let second = jobs::converge_builds(&state, tenant, user, ws, Some(card.id))
        .await
        .expect("enqueue 2");
    assert_eq!(second.raised, 0, "the same card is not raised twice");

    bed.teardown().await;
}

/// The label-apply NUDGE (MAIN-458, trigger three): applying `agent-ready`
/// converges the card's workspace immediately — gated on the tenant's
/// `loops.enabled`, checked inside the spawned task.
#[tokio::test]
async fn applying_agent_ready_nudges_a_run_into_existence() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("bnudge").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let board = board_fixture(&bed.db(), tenant).await;
    bed.db()
        .exec(
            "INSERT INTO settings (id, tenant_id, scope, user_id, key, value)
             VALUES ($1, $2, 'tenant', NULL, 'loops.enabled', 'true')",
            params![Uuid::now_v7(), tenant],
        )
        .await
        .expect("loops on");

    // Approved-shaped in every way EXCEPT the label, which the nudge applies.
    let provider = LocalBoardProvider {
        repo: std::sync::Arc::new(nook_control::repo::tasks::DbTaskRepository::new(bed.db())),
    };
    let card = provider
        .create_task(
            tenant,
            board,
            Some(user),
            CreateTaskRequest {
                title: "nudge me".into(),
                description: None,
                column_id: None,
                column_type: Some("unstarted".into()),
                workspace_id: Some(ws),
                priority: None,
                type_: None,
                visibility: None,
                parent: None,
                labels: vec![],
            },
        )
        .await
        .expect("card");

    // The route resolves an EXISTING label; a fresh tenant has none yet.
    bed.db()
        .exec(
            "INSERT INTO labels (id, tenant_id, name, color) VALUES ($1, $2, 'agent-ready', '#3fb950')",
            params![Uuid::now_v7(), tenant],
        )
        .await
        .expect("label row");

    let auth = nook_control::auth::AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: user,
        tenant_id: tenant,
        principal: nook_control::auth::Principal::User,
        cookie_session: false,
    };
    let _labels = nook_control::routes::labels::add(
        axum::extract::State(state.clone()),
        auth,
        axum::extract::Path((card.id.0.to_string(), "agent-ready".into())),
    )
    .await
    .expect("label applied");

    // The nudge is spawned; give it a moment, then insist.
    let mut raised = Vec::new();
    for _ in 0..40 {
        raised = state
            .jobs
            .list_for_task(tenant, card.id)
            .await
            .expect("list");
        if !raised.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(raised.len(), 1, "the nudge raised exactly one run");
    assert_eq!(raised[0].kind, "build");

    bed.teardown().await;
}

/// MAIN-459 AC-3: the join is validated BEFORE anything is recorded. A PR url
/// naming another repository is refused, and the refusal changes nothing —
/// the outcome stays unrecorded, so the run can fix its report and try again.
#[tokio::test]
async fn a_pr_outcome_naming_another_repository_is_refused() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("bjoin").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    bed.db()
        .exec(
            "UPDATE workspaces SET git_remote_url = 'git@github.com:acme/api.git' WHERE id = $1",
            params![ws],
        )
        .await
        .expect("remote");
    let state = bed.app_state().await;
    let board = board_fixture(&bed.db(), tenant).await;
    approved_card(&bed.db(), tenant, board, ws, user, "join check").await;

    let c = jobs::converge_builds(&state, tenant, user, ws, None)
        .await
        .expect("converge");
    assert_eq!(c.raised, 1);
    let job = c.jobs[0].clone();
    state
        .jobs
        .transition(job.id, "running")
        .await
        .expect("to running");

    let refused = jobs::record_build_outcome(
        &state,
        tenant,
        job.id,
        &BuildOutcomeRequest {
            outcome: "pr_opened".into(),
            url: Some("https://github.com/evil/other/pull/9".into()),
            question: None,
        },
    )
    .await;
    assert!(refused.is_err(), "another repository's PR must be refused");
    let live = state
        .jobs
        .get(tenant, job.id)
        .await
        .expect("read")
        .expect("job");
    assert_eq!(
        live.build_outcome, None,
        "a refused join records nothing — validate-before-record"
    );

    bed.teardown().await;
}

/// The other half of the validation contract: where the deployment cannot read
/// the PR body (no forge credential, or a forge that does not answer for a
/// repository that does not exist), the URL/repository half still applies and
/// the Closes half is SKIPPED — availability must not gate recording.
#[tokio::test]
async fn an_unreadable_body_skips_the_closes_check_but_still_records() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("bjoin2").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let state = with_body_forge(
        &bed,
        &state,
        ws,
        "git@github.com:nook-testing-nonexistent/api.git",
        Err("503 upstream".into()),
    )
    .await;
    let board = board_fixture(&bed.db(), tenant).await;
    let card = approved_card(&bed.db(), tenant, board, ws, user, "skip check").await;

    let c = jobs::converge_builds(&state, tenant, user, ws, None)
        .await
        .expect("converge");
    assert_eq!(c.raised, 1);
    let job = c.jobs[0].clone();
    state
        .jobs
        .transition(job.id, "running")
        .await
        .expect("to running");

    let done = jobs::record_build_outcome(
        &state,
        tenant,
        job.id,
        &BuildOutcomeRequest {
            outcome: "pr_opened".into(),
            url: Some("https://github.com/nook-testing-nonexistent/api/pull/9".into()),
            question: None,
        },
    )
    .await
    .expect("recorded despite the unreadable body");
    assert_eq!(done.build_outcome.as_deref(), Some("pr_opened"));
    let row = state
        .tasks
        .get_row(tenant, card.id)
        .await
        .expect("read")
        .expect("card");
    assert_eq!(
        row.pr_url.as_deref(),
        Some("https://github.com/nook-testing-nonexistent/api/pull/9")
    );

    bed.teardown().await;
}

/// The repaired defect: `github_repo` lowercases the stored remote, while
/// `gh pr create` echoes GitHub's canonical casing — the two must still match,
/// or every outcome for a `Acme/API`-style repo is refused. The dictated body
/// also proves the happy Closes path end to end.
#[tokio::test]
async fn a_canonically_cased_url_matches_a_lowercased_remote() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("bjoin3").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let board = board_fixture(&bed.db(), tenant).await;
    let card = approved_card(&bed.db(), tenant, board, ws, user, "cased").await;
    let state = bed.app_state().await;
    let state = with_body_forge(
        &bed,
        &state,
        ws,
        "git@github.com:Acme/API.git",
        // The fixture board's key is 'BC'; the helper's TaskItem does not
        // carry the joined key, so it is rebuilt the way resolve_id reads it.
        Ok(format!(
            "What changed\n\nCloses BC-{}\n",
            card.number.expect("seeded cards are numbered")
        )),
    )
    .await;

    let c = jobs::converge_builds(&state, tenant, user, ws, None)
        .await
        .expect("converge");
    assert_eq!(c.raised, 1);
    let job = c.jobs[0].clone();
    state
        .jobs
        .transition(job.id, "running")
        .await
        .expect("to running");

    let done = jobs::record_build_outcome(
        &state,
        tenant,
        job.id,
        &BuildOutcomeRequest {
            outcome: "pr_opened".into(),
            url: Some("https://github.com/Acme/API/pull/12".into()),
            question: None,
        },
    )
    .await
    .expect("the canonical casing is this workspace's own repository");
    assert_eq!(done.build_outcome.as_deref(), Some("pr_opened"));

    bed.teardown().await;
}

/// The Closes half refuses by name when the body IS readable: no `Closes`
/// line, and a key that is not this run's card — and a refusal records
/// nothing, so the run can fix its PR and report again.
#[tokio::test]
async fn a_missing_or_wrong_closes_line_is_refused_and_records_nothing() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("bjoin4").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let board = board_fixture(&bed.db(), tenant).await;
    approved_card(&bed.db(), tenant, board, ws, user, "no closes").await;
    let state = bed.app_state().await;
    let state = with_body_forge(
        &bed,
        &state,
        ws,
        "git@github.com:acme/api.git",
        Ok("A body with no closing line at all".into()),
    )
    .await;

    let c = jobs::converge_builds(&state, tenant, user, ws, None)
        .await
        .expect("converge");
    assert_eq!(c.raised, 1);
    let job = c.jobs[0].clone();
    state
        .jobs
        .transition(job.id, "running")
        .await
        .expect("to running");

    let refused = jobs::record_build_outcome(
        &state,
        tenant,
        job.id,
        &BuildOutcomeRequest {
            outcome: "pr_opened".into(),
            url: Some("https://github.com/acme/api/pull/7".into()),
            question: None,
        },
    )
    .await;
    assert!(refused.is_err(), "a body without `Closes` is refused");

    // Now the body closes a key that resolves to no card here.
    let state = with_body_forge(
        &bed,
        &state,
        ws,
        "git@github.com:acme/api.git",
        Ok("Closes ZZZ-999\n".into()),
    )
    .await;
    let refused = jobs::record_build_outcome(
        &state,
        tenant,
        job.id,
        &BuildOutcomeRequest {
            outcome: "pr_opened".into(),
            url: Some("https://github.com/acme/api/pull/7".into()),
            question: None,
        },
    )
    .await;
    assert!(refused.is_err(), "a key with no card here is refused");
    let live = state
        .jobs
        .get(tenant, job.id)
        .await
        .expect("read")
        .expect("job");
    assert_eq!(live.build_outcome, None, "refusals record nothing");

    bed.teardown().await;
}
