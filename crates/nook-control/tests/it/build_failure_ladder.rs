//! The build-loop failure ladder: back off, hand it back, then stop (MAIN-386).
//!
//! What these pin: the count is a RUN of failures read off `loop_jobs` and not
//! a total (AC-1), the hold doubling at each rung (AC-2), the repair brief and
//! its label at two (AC-3), the stop and its notification at three (AC-4),
//! both resets (AC-5), and the manual trigger working on every rung (AC-6).
//!
//! Engine-neutral (MAIN-264): nothing here names a `sqlx` type.

use nook_control::services::build_ladder::{backoff_for, FIRST_BACKOFF, MAX_BACKOFF, MAX_FAILURES};
use nook_control::services::jobs;
use nook_control::services::kanban::{KanbanProvider, LocalBoardProvider};
use nook_control::services::run_reconcile::owed;
use nook_control::services::work_source::{BuildWork, WorkSource};
use nook_db::{params, Db, DbPool};
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

async fn board_fixture(db: &DbPool, tenant: TenantId) -> BoardId {
    let board = BoardId(Uuid::now_v7());
    db.exec(
        "INSERT INTO boards (id, tenant_id, name, key, provider) VALUES ($1,$2,'b','BL','local')",
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

/// One whole failing pass, with the hold aged out afterwards so the next pass
/// in a test is not merely waiting on the clock.
async fn a_failed_run(
    bed: &TestBed,
    state: &nook_control::state::AppState,
    tenant: TenantId,
    user: UserId,
    ws: WorkspaceId,
    only_task: Option<TaskId>,
) -> LoopJob {
    let job = a_failed_run_still_held(bed, state, tenant, user, ws, only_task).await;
    age_out(bed, job.id, MAX_BACKOFF + chrono::Duration::minutes(1)).await;
    job
}

/// The same pass, leaving the hold in force — what a test asking "is the card
/// held right now?" needs.
async fn a_failed_run_still_held(
    _bed: &TestBed,
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
    jobs::append_transcript(state, job.id, "system", "cargo build: linker not found")
        .await
        .expect("transcript");
    jobs::transition(state, tenant, job.id, "failed")
        .await
        .expect("failed");
    job
}

async fn age_out(bed: &TestBed, job: JobId, by: chrono::Duration) {
    bed.db()
        .exec(
            "UPDATE loop_jobs SET updated_at = $2 WHERE id = $1",
            params![job, chrono::Utc::now() - by],
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

/// AC-1: the count is the run of `failed` rows since the last run that
/// CONCLUDED something — a total would keep climbing across a working week.
#[tokio::test]
async fn the_count_is_a_run_of_failures_and_an_outcome_ends_it() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ladcount").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let board = board_fixture(&bed.db(), tenant).await;
    let card = approved_card(&bed.db(), tenant, board, ws, user, "count me").await;

    let streak = |task: TaskId| {
        let state = state.clone();
        async move { state.jobs.build_failure_streak(tenant, task).await.unwrap() }
    };
    assert!(streak(card.id).await.is_empty(), "nothing has run yet");

    let first = a_failed_run(&bed, &state, tenant, user, ws, None).await;
    assert_eq!(streak(card.id).await.len(), 1);
    let second = a_failed_run(&bed, &state, tenant, user, ws, None).await;
    let so_far = streak(card.id).await;
    assert_eq!(so_far.len(), 2, "consecutive failures accumulate");
    assert_eq!(
        so_far.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![second.id, first.id],
        "newest first — the escalation lists them in the order they happened"
    );

    // A run that CONCLUDES something ends the run of failures.
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

    assert!(
        streak(card.id).await.is_empty(),
        "AC-1/AC-5: the successful build reset the count"
    );

    bed.teardown().await;
}

/// AC-2: `5 min × 2^(n-1)`, capped at an hour — asked of the wakeup rule with
/// real rows, because the alternative is a test that sleeps for forty minutes.
#[tokio::test]
async fn each_failure_doubles_the_hold_up_to_an_hour() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ladback").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let board = board_fixture(&bed.db(), tenant).await;
    let card = approved_card(&bed.db(), tenant, board, ws, user, "hold me").await;

    let held_for = |expected: chrono::Duration| {
        let state = state.clone();
        async move {
            let source = BuildWork {
                tasks: state.tasks.as_ref(),
                tenant,
                viewer: user,
                demand: &state.review_demand,
                token: None,
                rejected_heads: Default::default(),
                conflicts: Default::default(),
                unblock_task: None,
            };
            let items = source.items(ws, None).await.expect("items");
            let heads = state.jobs.build_run_heads(tenant, ws).await.expect("heads");
            let attempted = heads
                .iter()
                .find(|h| h.attempted_at.is_some())
                .and_then(|h| h.attempted_at)
                .expect("an attempt to hold from");
            // A minute either side of the boundary: held before, owed after.
            let just_inside = attempted + expected - chrono::Duration::minutes(1);
            let just_after = attempted + expected + chrono::Duration::minutes(1);
            assert!(
                owed(&items, &heads, 1, just_inside).0.is_empty(),
                "still held {expected} after the failure"
            );
            assert_eq!(
                owed(&items, &heads, 1, just_after).0.len(),
                1,
                "owed once {expected} has passed"
            );
        }
    };

    // The card is only pickable again after each hold, so the run has to be
    // aged past the LONGEST one to set up the next rung.
    a_failed_run_still_held(&bed, &state, tenant, user, ws, None).await;
    held_for(FIRST_BACKOFF).await;

    let first = state
        .jobs
        .build_failure_streak(tenant, card.id)
        .await
        .unwrap()[0]
        .id;
    age_out(&bed, first, MAX_BACKOFF + chrono::Duration::minutes(1)).await;
    a_failed_run_still_held(&bed, &state, tenant, user, ws, None).await;
    held_for(chrono::Duration::minutes(10)).await;

    assert_eq!(backoff_for(3), chrono::Duration::minutes(20));
    assert_eq!(backoff_for(4), chrono::Duration::minutes(40));
    assert_eq!(backoff_for(5), MAX_BACKOFF, "AC-2: capped at an hour");

    bed.teardown().await;
}

/// AC-3: the second failure asks for a repair pass — a template comment
/// carrying the run, the reason and the transcript tail, and the board's
/// `loop-changes-requested` label. NG-1: no agent wrote a word of it.
#[tokio::test]
async fn the_second_failure_posts_a_template_brief_and_asks_for_a_repair() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ladtwo").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let board = board_fixture(&bed.db(), tenant).await;
    let card = approved_card(&bed.db(), tenant, board, ws, user, "brief me").await;

    a_failed_run(&bed, &state, tenant, user, ws, None).await;
    assert!(
        !labels(&state, card.id)
            .await
            .iter()
            .any(|l| l == "loop-changes-requested"),
        "AC-2: one failure is the hold and nothing else"
    );

    let second = a_failed_run(&bed, &state, tenant, user, ws, None).await;

    assert!(
        labels(&state, card.id)
            .await
            .iter()
            .any(|l| l == "loop-changes-requested"),
        "AC-3: the card is asking for a repair pass"
    );
    let said = comments(&state, card.id).await;
    let brief = said
        .iter()
        .find(|c| c.contains("asking for a repair pass"))
        .unwrap_or_else(|| panic!("AC-3: no brief was posted: {said:?}"));
    assert!(
        brief.contains(&second.id.to_string()),
        "the job id: {brief}"
    );
    assert!(brief.contains("2 in a row"), "the reason: {brief}");
    assert!(
        brief.contains("cargo build: linker not found"),
        "the transcript tail: {brief}"
    );
    assert!(
        !labels(&state, card.id)
            .await
            .iter()
            .any(|l| l == "needs-human-review"),
        "AC-3: two failures do not stop the card"
    );

    // NG-1/AC-3: no agent was invoked to write it. The only jobs that exist
    // are the two build runs the test drove.
    let runs = state
        .jobs
        .list_for_task(tenant, card.id)
        .await
        .expect("runs");
    assert_eq!(runs.len(), 2, "no run was raised to author the comment");
    assert!(runs.iter().all(|j| j.kind == "build"));

    bed.teardown().await;
}

/// AC-4: the third stops the card — `needs-human-review` on, the repair label
/// off, one comment naming all three runs, and a notification.
#[tokio::test]
async fn the_third_failure_stops_the_card_and_raises_a_notification() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ladthree").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let board = board_fixture(&bed.db(), tenant).await;
    let card = approved_card(&bed.db(), tenant, board, ws, user, "stop me").await;

    let mut runs = Vec::new();
    for _ in 0..MAX_FAILURES {
        runs.push(a_failed_run(&bed, &state, tenant, user, ws, None).await);
    }

    let on = labels(&state, card.id).await;
    assert!(
        on.iter().any(|l| l == "needs-human-review"),
        "AC-4: the card is handed to a human: {on:?}"
    );
    assert!(
        !on.iter().any(|l| l == "loop-changes-requested"),
        "AC-4: and is no longer asking for a repair: {on:?}"
    );

    let said = comments(&state, card.id).await;
    let stop = said
        .iter()
        .find(|c| c.contains("3 build runs for this card"))
        .unwrap_or_else(|| panic!("AC-4: no final comment: {said:?}"));
    for r in &runs {
        assert!(
            stop.contains(&r.id.to_string()),
            "AC-4: run {} is not named: {stop}",
            r.id
        );
    }

    let raised: Vec<(String, String)> = bed
        .db()
        .query_all(
            "SELECT level, title FROM notifications WHERE tenant_id = $1 AND kind = $2",
            params![tenant, "task.build_ladder"],
        )
        .await
        .expect("notifications");
    assert_eq!(raised.len(), 1, "AC-4: one notification: {raised:?}");
    assert_eq!(raised[0].0, "warning");

    // AC-4: excluded from auto-fire, by the label rather than by anything the
    // ladder has to remember.
    let fourth = jobs::converge_builds(&state, tenant, user, ws, None)
        .await
        .expect("converge");
    assert_eq!(fourth.raised, 0, "AC-4: nothing auto-fires after the stop");

    bed.teardown().await;
}

/// AC-5: lifting the stop returns the card to auto-fire with a CLEAN count —
/// the next failure is the first rung again, not an instant re-escalation.
#[tokio::test]
async fn lifting_the_stop_returns_the_card_with_a_clean_count() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ladlift").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let board = board_fixture(&bed.db(), tenant).await;
    let card = approved_card(&bed.db(), tenant, board, ws, user, "lift me").await;

    for _ in 0..MAX_FAILURES {
        a_failed_run(&bed, &state, tenant, user, ws, None).await;
    }
    assert_eq!(
        state
            .jobs
            .build_failure_streak(tenant, card.id)
            .await
            .unwrap()
            .len(),
        MAX_FAILURES as usize
    );

    // Through the seam every surface removes a label by — the REST route, MCP
    // `remove_label`, a board automation — rather than the repo underneath it.
    // Wiring the reset to one of those three was the bug this pins.
    assert!(
        nook_control::services::tasks::detach_label(&state, tenant, card.id, "needs-human-review")
            .await
            .expect("lift"),
        "the label was there to remove"
    );

    assert!(
        state
            .jobs
            .build_failure_streak(tenant, card.id)
            .await
            .unwrap()
            .is_empty(),
        "AC-5: the three failures are answered"
    );

    // The next failure is rung ONE: no brief, no stop, just the hold.
    let fresh = a_failed_run(&bed, &state, tenant, user, ws, None).await;
    assert_eq!(fresh.kind, "build");
    let on = labels(&state, card.id).await;
    assert!(
        !on.iter().any(|l| l == "needs-human-review"),
        "AC-5: one failure after the reset is not a re-escalation: {on:?}"
    );
    assert!(
        !on.iter().any(|l| l == "loop-changes-requested"),
        "AC-5: nor the second rung: {on:?}"
    );

    bed.teardown().await;
}

/// AC-6, inside the hold: the reconciler waits, a person does not. The hold is
/// the one gate whose entire content is "wait", and waiting is exactly what
/// clicking *Build this ticket* declines.
#[tokio::test]
async fn the_manual_trigger_ignores_the_hold() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ladhold").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let board = board_fixture(&bed.db(), tenant).await;
    let card = approved_card(&bed.db(), tenant, board, ws, user, "force me").await;

    a_failed_run_still_held(&bed, &state, tenant, user, ws, None).await;

    let swept = jobs::converge_builds(&state, tenant, user, ws, None)
        .await
        .expect("sweep");
    assert_eq!(swept.raised, 0, "AC-2: the sweep is held");

    let forced = jobs::converge_builds(&state, tenant, user, ws, Some(card.id))
        .await
        .expect("manual");
    assert_eq!(forced.raised, 1, "AC-6: the person is not");

    bed.teardown().await;
}

/// AC-6 after the STOP, and twice. The first cut made the count the
/// permission, which worked exactly once: the forced run spends the count, so
/// the second click found an empty count, the label still on, and nothing
/// raised. Naming the card is the whole permission.
#[tokio::test]
async fn the_manual_trigger_works_after_the_stop_more_than_once() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ladforce").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let board = board_fixture(&bed.db(), tenant).await;
    let card = approved_card(&bed.db(), tenant, board, ws, user, "force me twice").await;

    for _ in 0..MAX_FAILURES {
        a_failed_run(&bed, &state, tenant, user, ws, None).await;
    }
    assert!(
        labels(&state, card.id)
            .await
            .iter()
            .any(|l| l == "needs-human-review"),
        "the ladder stopped the card"
    );

    for click in 1..=2 {
        let forced = jobs::converge_builds(&state, tenant, user, ws, Some(card.id))
            .await
            .expect("manual");
        assert_eq!(
            forced.raised, 1,
            "AC-6: click {click} must raise a run despite the stop"
        );
        let job = forced.jobs[0].clone();
        jobs::transition(&state, tenant, job.id, "claimed")
            .await
            .expect("claimed");
        jobs::transition(&state, tenant, job.id, "failed")
            .await
            .expect("failed");
        age_out(&bed, job.id, MAX_BACKOFF + chrono::Duration::minutes(1)).await;
    }

    // …and the stop is still the loop's, not lifted by the person forcing a
    // run past it: nothing AUTO-fires, which is the whole of what it gates.
    let swept = jobs::converge_builds(&state, tenant, user, ws, None)
        .await
        .expect("sweep");
    assert_eq!(swept.raised, 0, "AC-4: a forced run does not lift the stop");

    bed.teardown().await;
}

/// The stop reaches the REPAIR lane too. A card the ladder gave up on must not
/// keep firing repair passes off its recorded PR — which is a second source of
/// work for the same card, and the one that would have gone on all night.
#[tokio::test]
async fn the_stop_excludes_the_card_from_the_repair_lane_as_well() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ladrepair").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let board = board_fixture(&bed.db(), tenant).await;
    let card = approved_card(&bed.db(), tenant, board, ws, user, "repair me").await;
    bed.db()
        .exec(
            "UPDATE tasks SET pr_url = $2 WHERE id = $1",
            params![card.id, "https://github.com/acme/repo/pull/7"],
        )
        .await
        .expect("record a PR");

    assert_eq!(
        state
            .tasks
            .tasks_with_pr(tenant, ws, None)
            .await
            .expect("with pr")
            .len(),
        1,
        "the card is in the repair lane to begin with"
    );

    state
        .tasks
        .attach_label(tenant, card.id, "needs-human-review")
        .await
        .expect("stop");

    assert!(
        state
            .tasks
            .tasks_with_pr(tenant, ws, None)
            .await
            .expect("with pr")
            .is_empty(),
        "AC-4: and out of it once the ladder stops"
    );
    assert_eq!(
        state
            .tasks
            .tasks_with_pr(tenant, ws, Some(card.id))
            .await
            .expect("with pr")
            .len(),
        1,
        "AC-6: unless a person names it to the manual trigger"
    );

    bed.teardown().await;
}

/// A `failed` row that RECORDED an outcome is not an attempt against its own
/// card. `reap_stale_executors` fails any claimed-or-running job whose node
/// went stale with no outcome guard, so a run that concluded and then lost its
/// node in the seconds before reaching `completed` leaves exactly that row —
/// and counting it would restart the streak at one instead of zero.
#[tokio::test]
async fn a_failed_run_that_recorded_an_outcome_answers_the_streak_without_joining_it() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ladreap").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let board = board_fixture(&bed.db(), tenant).await;
    let card = approved_card(&bed.db(), tenant, board, ws, user, "reap me").await;

    a_failed_run(&bed, &state, tenant, user, ws, None).await;
    a_failed_run(&bed, &state, tenant, user, ws, None).await;
    assert_eq!(
        state
            .jobs
            .build_failure_streak(tenant, card.id)
            .await
            .unwrap()
            .len(),
        2
    );

    // A run that concludes something and is then reaped before it can reach
    // `completed`: state `failed`, outcome recorded.
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
    jobs::transition(&state, tenant, job.id, "failed")
        .await
        .expect("reaped");

    assert!(
        state
            .jobs
            .build_failure_streak(tenant, card.id)
            .await
            .unwrap()
            .is_empty(),
        "it answered the two before it, and did not count as a third"
    );
    let on = labels(&state, card.id).await;
    assert!(
        !on.iter().any(|l| l == "needs-human-review"),
        "and reached no rung of its own: {on:?}"
    );

    bed.teardown().await;
}

/// AC-4 is about what THREE FAILURES cost the card, not about who put the stop
/// label on. A card already carrying `needs-human-review` — the claim reaper's,
/// the merge sweep's, a person's — still loses `loop-changes-requested` and
/// still gets its final comment when the ladder tops out, or the two labels sit
/// on it saying opposite things.
#[tokio::test]
async fn the_stop_still_reports_when_the_label_was_already_on() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ladalready").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let board = board_fixture(&bed.db(), tenant).await;
    let card = approved_card(&bed.db(), tenant, board, ws, user, "already stopped").await;

    a_failed_run(&bed, &state, tenant, user, ws, None).await;
    a_failed_run(&bed, &state, tenant, user, ws, None).await;
    assert!(
        labels(&state, card.id)
            .await
            .iter()
            .any(|l| l == "loop-changes-requested"),
        "rung two asked for a repair"
    );

    // Somebody else escalates the card for their own reason.
    state
        .tasks
        .attach_label(tenant, card.id, "needs-human-review")
        .await
        .expect("their escalation");

    // The third failure, raised directly: the label has already taken the card
    // out of both lanes, and the manual trigger — the one way back in — clears
    // the ladder on the way, which is the opposite of the state under test.
    let job = jobs::raise_run(
        &state,
        tenant,
        user,
        ws,
        "build",
        &nook_control::services::work_source::WorkItem {
            key: i64::from(card.number.expect("a numbered card")),
            fingerprint: nook_control::services::work_source::card_fingerprint(
                &card.title,
                card.description.as_deref(),
            ),
            label: "already stopped".into(),
            target_task_id: Some(card.id),
            claim_first: false,
            unblocked_at: None,
            conflict_base: None,
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
        state
            .jobs
            .build_failure_streak(tenant, card.id)
            .await
            .unwrap()
            .len(),
        MAX_FAILURES as usize,
        "the card is at the top of the ladder"
    );
    nook_control::services::build_ladder::on_run_failed(&state, tenant, &job, card.id).await;

    let on = labels(&state, card.id).await;
    assert!(
        !on.iter().any(|l| l == "loop-changes-requested"),
        "AC-4: the repair label comes off however the stop got there: {on:?}"
    );
    let said = comments(&state, card.id).await;
    assert!(
        said.iter().any(|c| c.contains("build runs for this card")),
        "AC-4: and the final comment is still posted: {said:?}"
    );

    bed.teardown().await;
}
