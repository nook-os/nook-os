//! A build run that concludes NOTHING gives its card back (MAIN-489).
//!
//! What these pin: the release and the comment that explain a card's return
//! (AC-1/AC-2), the backoff that keeps the retry from being a hot loop (AC-3),
//! the three-failure escalation that stops the cycle (AC-4), both human nudges
//! that spend the count (AC-5), an outcome resetting it (AC-6), and the human
//! claim this path never touches (AC-7).
//!
//! MAIN-386 moved the COUNT and the escalation into
//! `services::build_ladder` — derived from `loop_jobs` rather than stored, and
//! stopping at `needs-human-review` rather than `blocked`. Everything these
//! tests were written to pin still holds; the label and the file that writes
//! it are what changed.
//!
//! Engine-neutral (MAIN-264): nothing here names a `sqlx` type.

use nook_control::services::build_ladder::{MAX_BACKOFF, MAX_FAILURES};
use nook_control::services::jobs;
use nook_control::services::kanban::{KanbanProvider, LocalBoardProvider};
use nook_control::services::run_reconcile::{owed, FAILURE_BACKOFF};
use nook_control::services::work_source::{BuildWork, WorkSource};
use nook_db::{params, Db, DbPool};
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

async fn board_fixture(db: &DbPool, tenant: TenantId) -> BoardId {
    let board = BoardId(Uuid::now_v7());
    db.exec(
        "INSERT INTO boards (id, tenant_id, name, key, provider) VALUES ($1,$2,'b','BH','local')",
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
    LocalBoardProvider {
        repo: std::sync::Arc::new(nook_control::repo::tasks::DbTaskRepository::new(db.clone())),
    }
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
    .expect("card")
}

/// One whole failing pass: the converger raises and claims, the run dies
/// without ever recording an outcome, and the attempt is aged out of the
/// backoff so the next pass in the test is not merely waiting on the clock.
async fn a_run_that_concludes_nothing(
    bed: &TestBed,
    state: &nook_control::state::AppState,
    tenant: TenantId,
    user: UserId,
    ws: WorkspaceId,
    only_task: Option<TaskId>,
) -> LoopJob {
    let c = jobs::converge_builds(state, tenant, user, ws, only_task)
        .await
        .expect("converge");
    assert_eq!(c.raised, 1, "the card was owed a run");
    let job = c.jobs[0].clone();
    jobs::transition(state, tenant, job.id, "claimed")
        .await
        .expect("claimed");
    jobs::append_transcript(state, job.id, "system", "gh auth status: not logged in")
        .await
        .expect("transcript");
    jobs::transition(state, tenant, job.id, "failed")
        .await
        .expect("failed");
    age_out_the_backoff(bed, job.id).await;
    job
}

/// Past the LONGEST hold the ladder can impose, not merely the first one: the
/// window doubles with the run of failures (MAIN-386 AC-2), so aging by five
/// minutes would leave the second attempt held and the test asserting a run
/// that never came.
async fn age_out_the_backoff(bed: &TestBed, job: JobId) {
    bed.db()
        .exec(
            "UPDATE loop_jobs SET updated_at = $2 WHERE id = $1",
            params![
                job,
                chrono::Utc::now() - MAX_BACKOFF - chrono::Duration::minutes(1)
            ],
        )
        .await
        .expect("age the attempt");
}

async fn labels(state: &nook_control::state::AppState, task: TaskId) -> Vec<String> {
    state
        .tasks
        .labels_of_task(task)
        .await
        .expect("labels")
        .into_iter()
        .map(|l| l.name)
        .collect()
}

async fn comments(state: &nook_control::state::AppState, task: TaskId) -> Vec<String> {
    state
        .tasks
        .comments_of(task)
        .await
        .expect("comments")
        .into_iter()
        .map(|c| c.body_md)
        .collect()
}

async fn column_type(
    state: &nook_control::state::AppState,
    tenant: TenantId,
    task: TaskId,
) -> String {
    let row = state
        .tasks
        .get_row(tenant, task)
        .await
        .expect("read")
        .expect("card");
    state
        .tasks
        .column_type_of(row.column_id)
        .await
        .expect("column")
        .expect("a typed column")
}

/// AC-1/AC-2/AC-3: the wedge, undone. A run that fails without an outcome
/// releases the loop's claim, puts the card back in the unstarted column with a
/// comment saying why — and the card is then held by the ordinary failure
/// backoff rather than re-raised on the very next pass.
#[tokio::test]
async fn a_run_that_concludes_nothing_hands_its_card_back() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("hbback").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let board = board_fixture(&bed.db(), tenant).await;
    let card = approved_card(&bed.db(), tenant, board, ws, user, "wedge me").await;

    let c = jobs::converge_builds(&state, tenant, user, ws, None)
        .await
        .expect("converge");
    assert_eq!(c.raised, 1);
    let job = c.jobs[0].clone();
    jobs::transition(&state, tenant, job.id, "claimed")
        .await
        .expect("claimed");
    jobs::append_transcript(&state, job.id, "system", "gh auth status: not logged in")
        .await
        .expect("transcript");

    // The card is held while the run is alive — that is the state that used to
    // outlive it.
    let held = state
        .tasks
        .get_row(tenant, card.id)
        .await
        .expect("read")
        .expect("card");
    assert_eq!(held.assignee_user_id, Some(user), "claimed for the run");

    jobs::transition(&state, tenant, job.id, "failed")
        .await
        .expect("failed");

    let row = state
        .tasks
        .get_row(tenant, card.id)
        .await
        .expect("read")
        .expect("card");
    assert_eq!(row.assignee_user_id, None, "AC-1: the claim came back");
    assert_eq!(row.claim_expires_at, None, "and its lease with it");
    assert_eq!(
        column_type(&state, tenant, card.id).await,
        "unstarted",
        "AC-2: back where it was picked from"
    );
    let said = comments(&state, card.id).await;
    assert!(
        said.iter().any(|c| c.contains("no outcome recorded")
            && c.contains("attempt 1 of 3")
            && c.contains("gh auth status: not logged in")),
        "AC-2: the card explains itself before it reappears: {said:?}"
    );
    assert!(
        !labels(&state, card.id).await.iter().any(|l| l == "blocked"),
        "one failure escalates nothing"
    );

    // AC-3: pickable again, but held by the backoff — not re-raised now…
    let again = jobs::converge_builds(&state, tenant, user, ws, None)
        .await
        .expect("converge 2");
    assert_eq!(again.raised, 0, "AC-3: held for the backoff window");

    // …and owed once that window passes. Asked of the rule directly, because
    // the alternative is a test that sleeps for five minutes.
    let source = BuildWork {
        tasks: state.tasks.as_ref(),
        tenant,
        viewer: user,
        demand: &state.review_demand,
        token: None,
        rejected_heads: Default::default(),
        unblock_task: None,
    };
    let items = source.items(ws, None).await.expect("items");
    assert_eq!(items.len(), 1, "the card is back IN the work source");
    let heads = state.jobs.build_run_heads(tenant, ws).await.expect("heads");
    let later = chrono::Utc::now() + FAILURE_BACKOFF + chrono::Duration::minutes(1);
    assert_eq!(
        owed(&items, &heads, 1, later).0.len(),
        1,
        "AC-3: owed again after the backoff, once per window"
    );

    bed.teardown().await;
}

/// AC-4/AC-5: three runs in a row concluding nothing stop the cycle with
/// `needs-human-review`, and lifting that label brings the card back with the
/// count spent — a fresh three, not a hair trigger.
#[tokio::test]
async fn three_strikes_block_the_card_and_unblocking_resets_the_count() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("hbstrk").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let board = board_fixture(&bed.db(), tenant).await;
    let card = approved_card(&bed.db(), tenant, board, ws, user, "strike me").await;

    for _ in 0..MAX_FAILURES {
        a_run_that_concludes_nothing(&bed, &state, tenant, user, ws, None).await;
    }

    assert!(
        labels(&state, card.id)
            .await
            .iter()
            .any(|l| l == "needs-human-review"),
        "AC-4: three strikes hand the card to a human"
    );
    let said = comments(&state, card.id).await;
    assert!(
        said.iter().any(|c| c.contains("attempt 3 of 3")),
        "AC-4: the handback says how many attempts were made: {said:?}"
    );
    assert!(
        said.iter()
            .any(|c| c.contains("3 build runs for this card")),
        "AC-4: and the ladder says what that cost the card: {said:?}"
    );
    let fourth = jobs::converge_builds(&state, tenant, user, ws, None)
        .await
        .expect("converge");
    assert_eq!(fourth.raised, 0, "AC-4: no fourth run is raised");

    // AC-5, first nudge: a human removes the label. Through the service the
    // label route calls, so the clear and the label come off together — a
    // detach on its own would leave the count reading three.
    nook_control::services::tasks::detach_label(&state, tenant, card.id, "needs-human-review")
        .await
        .expect("unblock");

    let after = a_run_that_concludes_nothing(&bed, &state, tenant, user, ws, None).await;
    assert_eq!(after.kind, "build");
    let said = comments(&state, card.id).await;
    assert!(
        said.last().expect("a comment").contains("attempt 1 of 3"),
        "AC-5: the nudge spent the count — this is attempt one again: {said:?}"
    );
    assert!(
        !labels(&state, card.id)
            .await
            .iter()
            .any(|l| l == "needs-human-review"),
        "AC-5: one failure after the reset does not re-escalate"
    );

    bed.teardown().await;
}

/// AC-5, second nudge: the manual trigger fires on an escalated card without
/// anybody touching its labels — the way a forced re-review overrules an
/// already-verdicted head — and spends the strikes too.
#[tokio::test]
async fn the_manual_trigger_fires_on_an_escalated_card() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("hbmanu").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let board = board_fixture(&bed.db(), tenant).await;
    let card = approved_card(&bed.db(), tenant, board, ws, user, "force me").await;

    for _ in 0..MAX_FAILURES {
        a_run_that_concludes_nothing(&bed, &state, tenant, user, ws, None).await;
    }
    assert!(labels(&state, card.id)
        .await
        .iter()
        .any(|l| l == "needs-human-review"));

    // The label is still on: only naming the card overrules it.
    a_run_that_concludes_nothing(&bed, &state, tenant, user, ws, Some(card.id)).await;
    let said = comments(&state, card.id).await;
    assert!(
        said.last().expect("a comment").contains("attempt 1 of 3"),
        "AC-5: the named card ran, and its count was spent: {said:?}"
    );
    // …and the reconciler still will not touch it, because the label is
    // exactly as binding as it was before.
    let reconciler = jobs::converge_builds(&state, tenant, user, ws, None)
        .await
        .expect("converge");
    assert_eq!(reconciler.raised, 0, "the bypass is the named card's alone");

    bed.teardown().await;
}

/// MAIN-496's rule, kept: `canceled` means the run never happened — a human
/// stopping it, or the queued-job reaper ending one nothing could place — so it
/// gives the card back like any other terminal state, and moves the ladder not
/// at all.
#[tokio::test]
async fn a_canceled_run_gives_the_card_back_without_a_strike() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("hbcancel").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let board = board_fixture(&bed.db(), tenant).await;
    let card = approved_card(&bed.db(), tenant, board, ws, user, "stop me").await;

    for _ in 0..MAX_FAILURES {
        let c = jobs::converge_builds(&state, tenant, user, ws, None)
            .await
            .expect("converge");
        assert_eq!(c.raised, 1);
        let job = c.jobs[0].clone();
        jobs::transition(&state, tenant, job.id, "canceled")
            .await
            .expect("canceled");

        let row = state
            .tasks
            .get_row(tenant, card.id)
            .await
            .expect("read")
            .expect("card");
        assert_eq!(row.assignee_user_id, None, "AC-1 holds for a cancel too");
        assert_eq!(column_type(&state, tenant, card.id).await, "unstarted");
        age_out_the_backoff(&bed, job.id).await;
    }

    let on = labels(&state, card.id).await;
    assert!(
        !on.iter()
            .any(|l| l == "needs-human-review" || l == "loop-changes-requested"),
        "three cancels are not three failures: {on:?}"
    );
    let said = comments(&state, card.id).await;
    assert!(
        said.iter().all(|c| !c.contains("attempt")),
        "a cancel claims no attempt: {said:?}"
    );

    bed.teardown().await;
}

/// The other half of that bypass: it overrules the LOOP's escalation and
/// nothing else. A card a human blocked for their own reason has no run of
/// failures behind it, so naming it to the manual trigger changes nothing.
#[tokio::test]
async fn the_manual_trigger_does_not_overrule_a_humans_block() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("hbhblk").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let board = board_fixture(&bed.db(), tenant).await;
    let card = approved_card(&bed.db(), tenant, board, ws, user, "not yours").await;
    state
        .tasks
        .attach_label(tenant, card.id, "blocked")
        .await
        .expect("a human blocks it");

    let named = jobs::converge_builds(&state, tenant, user, ws, Some(card.id))
        .await
        .expect("enqueue");
    assert_eq!(
        named.raised, 0,
        "a human's `blocked` is not the trigger's to overrule"
    );

    bed.teardown().await;
}

/// AC-4 across MAIN-482's refusals. A refusal ends a run before its agent ever
/// starts and never records an outcome, so it is a run that concluded nothing
/// like any other: it gives the card back through the SAME path, and it counts.
/// A node misconfigured the same way on every pass is precisely the failure
/// this stop exists for, so exempting it would have left the hole open on the
/// class most likely to repeat.
#[tokio::test]
async fn refusals_count_towards_the_stop_and_do_not_say_it_twice() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("hbrefuse").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let board = board_fixture(&bed.db(), tenant).await;
    let card = approved_card(&bed.db(), tenant, board, ws, user, "refuse me").await;
    let reason = "refusing to build on the default branch main";

    for attempt in 1..=MAX_FAILURES {
        let c = jobs::converge_builds(&state, tenant, user, ws, None)
            .await
            .expect("converge");
        assert_eq!(c.raised, 1, "attempt {attempt} was owed a run");
        let job = c.jobs[0].clone();
        jobs::transition(&state, tenant, job.id, "claimed")
            .await
            .expect("claimed");
        jobs::refuse(&state, tenant, job.id, reason)
            .await
            .expect("refuse");

        let row = state
            .tasks
            .get_row(tenant, card.id)
            .await
            .expect("read")
            .expect("card");
        assert_eq!(row.assignee_user_id, None, "the claim came back");
        assert_eq!(column_type(&state, tenant, card.id).await, "unstarted");
        let said = comments(&state, card.id).await;
        assert!(
            said.iter()
                .any(|c| c.contains(&format!("attempt {attempt} of {MAX_FAILURES}"))),
            "a refusal counts: {said:?}"
        );
        age_out_the_backoff(&bed, job.id).await;
    }

    assert!(
        labels(&state, card.id)
            .await
            .iter()
            .any(|l| l == "needs-human-review"),
        "three refusals reach the same stop three failures do"
    );
    let fourth = jobs::converge_builds(&state, tenant, user, ws, None)
        .await
        .expect("converge");
    assert_eq!(fourth.raised, 0, "and nothing is raised after it");

    // The refusal explains itself once, not twice: MAIN-482 comments the
    // reason, and the handback does not quote the same line back at it.
    let said = comments(&state, card.id).await;
    assert_eq!(
        said.iter().filter(|c| c.trim() == reason).count(),
        MAX_FAILURES as usize,
        "one reason comment per refusal, none of them echoed: {said:?}"
    );
    assert!(
        !said.iter().any(|c| c.contains("Last thing the run said")),
        "the handback does not repeat what the card was just told: {said:?}"
    );

    bed.teardown().await;
}

/// AC-6: an outcome ends the run of failures. Three spread across a successful
/// build never add up to an escalation.
#[tokio::test]
async fn an_outcome_between_failures_resets_the_count() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("hbreset").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let board = board_fixture(&bed.db(), tenant).await;
    let card = approved_card(&bed.db(), tenant, board, ws, user, "reset me").await;

    a_run_that_concludes_nothing(&bed, &state, tenant, user, ws, None).await;
    a_run_that_concludes_nothing(&bed, &state, tenant, user, ws, None).await;

    // A run that CONCLUDES something.
    let c = jobs::converge_builds(&state, tenant, user, ws, None)
        .await
        .expect("converge");
    let job = c.jobs[0].clone();
    jobs::transition(&state, tenant, job.id, "claimed")
        .await
        .expect("claimed");
    jobs::transition(&state, tenant, job.id, "running")
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
    jobs::transition(&state, tenant, job.id, "completed")
        .await
        .expect("completed");

    // A human amends the contract, which is what makes the card owed a run
    // again at all — and this one dies without an outcome.
    state
        .tasks
        .update_fields(
            tenant,
            card.id,
            nook_control::repo::tasks::TaskEdit {
                description: Some("## AC-1 — do the thing, differently".into()),
                ..Default::default()
            },
        )
        .await
        .expect("amend");
    a_run_that_concludes_nothing(&bed, &state, tenant, user, ws, None).await;

    assert!(
        !labels(&state, card.id)
            .await
            .iter()
            .any(|l| l == "needs-human-review"),
        "AC-6: two failures, an outcome, then one failure is not three strikes"
    );
    let said = comments(&state, card.id).await;
    assert!(
        said.last().expect("a comment").contains("attempt 1 of 3"),
        "AC-6: the count started again from the outcome: {said:?}"
    );

    bed.teardown().await;
}

/// AC-7: only the loop's own claim is ever released. A human who takes the card
/// over mid-run keeps it, and the card is left entirely alone — no strike, no
/// comment, no move.
#[tokio::test]
async fn a_human_who_takes_the_card_over_keeps_it() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("hbhuman").await;
    let (user, _) = bed.user(tenant, "member").await;
    let (human, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let board = board_fixture(&bed.db(), tenant).await;
    let card = approved_card(&bed.db(), tenant, board, ws, user, "mine now").await;

    let c = jobs::converge_builds(&state, tenant, user, ws, None)
        .await
        .expect("converge");
    let job = c.jobs[0].clone();
    jobs::transition(&state, tenant, job.id, "claimed")
        .await
        .expect("claimed");

    // A person takes the work over while the run is still alive.
    bed.db()
        .exec(
            "UPDATE tasks SET assignee_user_id = $2 WHERE id = $1",
            params![card.id, human],
        )
        .await
        .expect("takeover");

    jobs::transition(&state, tenant, job.id, "failed")
        .await
        .expect("failed");

    let row = state
        .tasks
        .get_row(tenant, card.id)
        .await
        .expect("read")
        .expect("card");
    assert_eq!(
        row.assignee_user_id,
        Some(human),
        "AC-7: a human's claim is not the loop's to release"
    );
    assert_eq!(
        column_type(&state, tenant, card.id).await,
        "started",
        "AC-7: nor is the card the loop's to move"
    );
    assert!(
        comments(&state, card.id).await.is_empty(),
        "AC-7: and there is nothing to explain — the loop took nothing back"
    );

    bed.teardown().await;
}

/// NG-2: a repair run never claimed the card, so it has nothing to hand back —
/// the card stays parked in In Review where the reviewer left it.
#[tokio::test]
async fn a_repair_run_that_concludes_nothing_leaves_its_card_alone() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("hbrepair").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let board = board_fixture(&bed.db(), tenant).await;
    let card = approved_card(&bed.db(), tenant, board, ws, user, "under repair").await;

    let job = jobs::raise_run(
        &state,
        tenant,
        user,
        ws,
        "build",
        &nook_control::services::work_source::WorkItem {
            key: -1,
            fingerprint: nook_control::services::work_source::repair_fingerprint("rej1"),
            label: "repair PR #7".into(),
            target_task_id: Some(card.id),
            claim_first: false,
        },
        None,
        false,
    )
    .await
    .expect("raise")
    .expect("a run");

    jobs::transition(&state, tenant, job.id, "claimed")
        .await
        .expect("claimed");
    jobs::transition(&state, tenant, job.id, "failed")
        .await
        .expect("failed");

    assert_eq!(
        column_type(&state, tenant, card.id).await,
        "unstarted",
        "the repair path moved nothing"
    );
    assert!(
        comments(&state, card.id).await.is_empty(),
        "and commented nothing"
    );

    bed.teardown().await;
}
