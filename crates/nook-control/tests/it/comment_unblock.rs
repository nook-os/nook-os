//! MAIN-584: comment and unblock in one action, so a human ruling actually
//! restarts the card.
//!
//! The interesting half is not the labels — it is the DEDUPE. A `blocked`
//! outcome is a recorded outcome, and `card_fingerprint` is title+description
//! only, so a card the loop handed back stays unpickable after the label comes
//! off, after `agent-ready` goes back on, and after the manual trigger. The
//! first test walks exactly that, one step at a time, and is the one that fails
//! on the code this replaced.
//!
//! Engine-neutral (MAIN-264): nothing here names a `sqlx` type. Needs a
//! database: set `DATABASE_URL` (`NOOK_REQUIRE_DB=1` in the suite).

use axum::extract::{Path, State};
use axum::Json;
use nook_control::auth::{AuthCtx, Principal};
use nook_control::mcp_backend::McpBackend;
use nook_control::services::jobs;
use nook_control::services::kanban::{KanbanProvider, LocalBoardProvider};
use nook_control::services::work_source::card_fingerprint;
use nook_control::state::AppState;
use nook_db::{params, Db, DbPool};
use nook_mcp::{McpCaller, NookBackend};
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

const ESCALATIONS: [&str; 3] = ["blocked", "spec-blocked", "needs-human-review"];

fn auth(user: UserId, tenant: TenantId) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: user,
        tenant_id: tenant,
        principal: Principal::User,
        cookie_session: false,
    }
}

async fn board_fixture(db: &DbPool, tenant: TenantId) -> BoardId {
    let board = BoardId(Uuid::now_v7());
    let key = format!("U{}", &board.0.simple().to_string()[..5]).to_uppercase();
    db.exec(
        "INSERT INTO boards (id, tenant_id, name, key, provider) VALUES ($1,$2,'b',$3,'local')",
        params![board, tenant, key],
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

async fn labels(state: &AppState, task: TaskId) -> Vec<String> {
    state
        .tasks
        .labels_of_task(task)
        .await
        .expect("labels")
        .into_iter()
        .map(|l| l.name)
        .collect()
}

async fn comments(state: &AppState, task: TaskId) -> Vec<String> {
    state
        .tasks
        .comments_of(task)
        .await
        .expect("comments")
        .into_iter()
        .map(|c| c.body_md)
        .collect()
}

/// The card as the database holds it — the stamp, the column and the claim in
/// one read, because AC-7 is as much about what did NOT move.
async fn row(state: &AppState, tenant: TenantId, task: TaskId) -> TaskItem {
    state
        .tasks
        .get_row(tenant, task)
        .await
        .expect("read")
        .expect("the card")
}

async fn post(
    state: &AppState,
    user: UserId,
    tenant: TenantId,
    task: TaskId,
    body: &str,
    clear_escalation: bool,
) -> nook_control::error::ApiResult<TaskComment> {
    nook_control::routes::task_detail::create_comment(
        State(state.clone()),
        auth(user, tenant),
        Path(task.to_string()),
        Json(CreateCommentRequest {
            body_md: body.into(),
            author_name: None,
            clear_escalation,
            request_changes: false,
        }),
    )
    .await
    .map(|j| j.0)
}

/// One whole build run that hands the card back with a question — the state
/// MAIN-454 was actually in.
async fn a_run_that_blocks(
    state: &AppState,
    tenant: TenantId,
    user: UserId,
    ws: WorkspaceId,
) -> LoopJob {
    let c = jobs::converge_builds(state, tenant, user, ws, None)
        .await
        .expect("converge");
    assert_eq!(c.raised, 1, "the approved card is owed its first run");
    let job = c.jobs[0].clone();
    jobs::transition(state, tenant, job.id, "claimed")
        .await
        .expect("claimed");
    jobs::transition(state, tenant, job.id, "running")
        .await
        .expect("running");
    jobs::record_build_outcome(
        state,
        tenant,
        job.id,
        &BuildOutcomeRequest {
            outcome: "blocked".into(),
            url: None,
            question: Some("Blocked: a ruling is needed before any code is written.".into()),
        },
    )
    .await
    .expect("outcome");
    jobs::transition(state, tenant, job.id, "completed")
        .await
        .expect("completed");
    job
}

/// AC-2, and the whole card: clearing the labels is not enough, and this walks
/// the three states one at a time so a regression says WHICH one came back.
#[tokio::test]
async fn a_ruling_re_arms_a_card_the_loop_handed_back() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("unbdedupe").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let board = board_fixture(&bed.db(), tenant).await;
    let card = approved_card(&bed.db(), tenant, board, ws, user, "rule on me").await;
    let fingerprint = card_fingerprint(&card.title, card.description.as_deref());

    let blocked_run = a_run_that_blocks(&state, tenant, user, ws).await;
    assert!(
        labels(&state, card.id).await.iter().any(|l| l == "blocked"),
        "the handback stopped the card"
    );
    assert_eq!(
        jobs::converge_builds(&state, tenant, user, ws, None)
            .await
            .expect("converge")
            .raised,
        0,
        "a stopped card is owed nothing"
    );

    // The naive fix, and the reason this card exists: re-arm the card by hand
    // and it is STILL never picked, because the dedupe reads a fingerprint no
    // ruling can move.
    nook_control::services::tasks::detach_label(&state, tenant, card.id, "blocked")
        .await
        .expect("detach");
    state
        .tasks
        .set_agent_ready(tenant, card.id, true)
        .await
        .expect("re-arm");
    assert_eq!(
        jobs::converge_builds(&state, tenant, user, ws, None)
            .await
            .expect("converge")
            .raised,
        0,
        "clearing the label and re-arming the card is NOT enough on its own"
    );

    // The one action.
    post(
        &state,
        user,
        tenant,
        card.id,
        "Build it as specified.",
        true,
    )
    .await
    .expect("comment and unblock");

    let c = jobs::converge_builds(&state, tenant, user, ws, None)
        .await
        .expect("converge");
    assert_eq!(c.raised, 1, "AC-2: the ruling re-armed the card");
    assert_eq!(
        c.jobs[0].build_fingerprint.as_deref(),
        Some(fingerprint.as_str()),
        "and the fingerprint is the one the blocked run already recorded"
    );
    assert_ne!(c.jobs[0].id, blocked_run.id, "a NEW run, not the old one");

    let after = row(&state, tenant, card.id).await;
    assert!(after.unblocked_at.is_some(), "AC-1/AC-4: the stamp is set");

    bed.teardown().await;
}

/// So the stamp does not permanently disable the dedupe for that card: the run
/// the ruling asked for concludes after it, and quiets the card again.
#[tokio::test]
async fn a_run_that_concluded_after_the_ruling_quiets_the_card_again() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("unbrequiet").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let board = board_fixture(&bed.db(), tenant).await;
    let card = approved_card(&bed.db(), tenant, board, ws, user, "quiet again").await;

    a_run_that_blocks(&state, tenant, user, ws).await;
    post(&state, user, tenant, card.id, "Carry on.", true)
        .await
        .expect("unblock");

    let c = jobs::converge_builds(&state, tenant, user, ws, None)
        .await
        .expect("converge");
    assert_eq!(c.raised, 1);
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

    assert_eq!(
        jobs::converge_builds(&state, tenant, user, ws, None)
            .await
            .expect("converge")
            .raised,
        0,
        "the stamp re-arms the card ONCE, not forever"
    );

    bed.teardown().await;
}

/// AC-4, AC-7 and AC-13, per label: whichever escalation is on the card comes
/// off, the card is re-armed and stamped, the ruling is stored — and nothing
/// moved on the board.
#[tokio::test]
async fn every_escalation_label_is_cleared_and_the_card_re_armed() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("unbeach").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let board = board_fixture(&bed.db(), tenant).await;

    for stop in ESCALATIONS {
        let card = approved_card(&bed.db(), tenant, board, ws, user, stop).await;
        // A card a human left in progress, claimed — the shape AC-7 protects.
        let started =
            nook_control::services::tasks::column_of_type(state.tasks.as_ref(), board, "started")
                .await
                .expect("started column");
        state
            .tasks
            .update_fields(
                tenant,
                card.id,
                nook_control::repo::tasks::TaskEdit {
                    column_id: Some(started.0),
                    assignee_user_id: Some(user.0),
                    ..Default::default()
                },
            )
            .await
            .expect("park it in progress");
        state
            .tasks
            .attach_label(tenant, card.id, stop)
            .await
            .expect("stop");
        state
            .tasks
            .set_agent_ready(tenant, card.id, false)
            .await
            .expect("and off the queue");

        post(&state, user, tenant, card.id, "Ruled: carry on.", true)
            .await
            .unwrap_or_else(|e| panic!("unblock a {stop} card: {e:?}"));

        let on = labels(&state, card.id).await;
        assert!(
            !on.iter().any(|l| l == stop),
            "AC-4: {stop} is gone: {on:?}"
        );
        assert!(
            on.iter().any(|l| l == "agent-ready"),
            "AC-4: re-armed: {on:?}"
        );
        let after = row(&state, tenant, card.id).await;
        assert!(after.unblocked_at.is_some(), "AC-4: stamped");
        assert_eq!(after.column_id, started, "AC-7: the column did not move");
        assert_eq!(
            after.assignee_user_id,
            Some(user),
            "AC-7: the claim did not change"
        );
        assert!(
            comments(&state, card.id)
                .await
                .iter()
                .any(|c| c == "Ruled: carry on."),
            "the ruling is on the card"
        );
    }

    bed.teardown().await;
}

/// One call, whatever the card is carrying — two escalations do not need two.
#[tokio::test]
async fn two_escalation_labels_come_off_in_one_call() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("unbtwo").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let board = board_fixture(&bed.db(), tenant).await;
    let card = approved_card(&bed.db(), tenant, board, ws, user, "doubly stopped").await;
    for stop in ["blocked", "needs-human-review"] {
        state
            .tasks
            .attach_label(tenant, card.id, stop)
            .await
            .expect("stop");
    }

    post(&state, user, tenant, card.id, "Ruled.", true)
        .await
        .expect("unblock");

    let on = labels(&state, card.id).await;
    assert!(
        !on.iter().any(|l| ESCALATIONS.contains(&l.as_str())),
        "both came off in one call: {on:?}"
    );

    bed.teardown().await;
}

/// AC-5: the ruling that released the card is always ON the card, so an unblock
/// with nothing to read is refused — and refused before anything is written.
#[tokio::test]
async fn an_unblock_with_no_body_writes_nothing() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("unbempty").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let board = board_fixture(&bed.db(), tenant).await;
    let card = approved_card(&bed.db(), tenant, board, ws, user, "no ruling").await;
    state
        .tasks
        .attach_label(tenant, card.id, "blocked")
        .await
        .expect("stop");

    for empty in ["", "   \n\t "] {
        let err = post(&state, user, tenant, card.id, empty, true)
            .await
            .expect_err("an unblock needs a ruling");
        assert!(
            matches!(err, nook_control::error::ApiError::BadRequest(ref m) if m.contains("ruling")),
            "AC-5: a 400 naming the rule, got {err:?}"
        );
    }

    assert!(comments(&state, card.id).await.is_empty(), "no comment");
    assert!(
        labels(&state, card.id).await.iter().any(|l| l == "blocked"),
        "no label change"
    );
    assert!(
        row(&state, tenant, card.id).await.unblocked_at.is_none(),
        "no stamp"
    );

    bed.teardown().await;
}

/// NG-4: a question can still be asked on a stopped card.
#[tokio::test]
async fn an_ordinary_comment_does_not_unblock() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("unbplain").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let board = board_fixture(&bed.db(), tenant).await;
    let card = approved_card(&bed.db(), tenant, board, ws, user, "still stopped").await;
    state
        .tasks
        .attach_label(tenant, card.id, "blocked")
        .await
        .expect("stop");

    post(
        &state,
        user,
        tenant,
        card.id,
        "What did you mean by AC-2?",
        false,
    )
    .await
    .expect("comment");

    assert_eq!(comments(&state, card.id).await.len(), 1, "it was stored");
    assert!(
        labels(&state, card.id).await.iter().any(|l| l == "blocked"),
        "and the card is still stopped"
    );
    assert!(
        row(&state, tenant, card.id).await.unblocked_at.is_none(),
        "and unstamped"
    );

    bed.teardown().await;
}

/// AC-8: the caller asked for an end state, not for a transition.
#[tokio::test]
async fn an_unblock_on_an_unstopped_card_still_re_arms_it() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("unbidem").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let board = board_fixture(&bed.db(), tenant).await;
    let card = approved_card(&bed.db(), tenant, board, ws, user, "never stopped").await;
    state
        .tasks
        .set_agent_ready(tenant, card.id, false)
        .await
        .expect("off the queue");

    for n in 1..=2 {
        post(&state, user, tenant, card.id, "Ruled again.", true)
            .await
            .unwrap_or_else(|e| panic!("call {n}: {e:?}"));
        let on = labels(&state, card.id).await;
        assert!(on.iter().any(|l| l == "agent-ready"), "re-armed: {on:?}");
        assert!(
            row(&state, tenant, card.id).await.unblocked_at.is_some(),
            "stamped"
        );
    }
    assert_eq!(
        comments(&state, card.id).await.len(),
        2,
        "each call still records its own ruling"
    );

    bed.teardown().await;
}

/// AC-6: the ladder reset is a property of the ESCALATION, not of the one label
/// the ladder happens to raise. Fails on the code this replaces, where only
/// `needs-human-review` reset anything.
#[tokio::test]
async fn lifting_any_escalation_resets_the_ladder() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("unbladder").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let board = board_fixture(&bed.db(), tenant).await;

    for stop in ESCALATIONS {
        let card = approved_card(&bed.db(), tenant, board, ws, user, stop).await;
        state
            .tasks
            .attach_label(tenant, card.id, stop)
            .await
            .expect("stop");
        nook_control::services::tasks::detach_label(&state, tenant, card.id, stop)
            .await
            .expect("lift");
        let cleared: Option<chrono::DateTime<chrono::Utc>> = bed
            .db()
            .query_scalar_opt(
                "SELECT build_ladder_cleared_at FROM tasks WHERE id = $1",
                params![card.id],
            )
            .await
            .expect("read")
            .flatten();
        assert!(
            cleared.is_some(),
            "AC-6: lifting {stop} answers the card's failures"
        );
    }

    bed.teardown().await;
}

/// AC-10 and AC-11: the same capability over MCP, and the comment it makes
/// reaches the activity feed — which this door never did.
#[tokio::test]
async fn an_unblock_over_mcp_restarts_the_card_and_is_auditable() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    // `comment_task` acts in the CALLER's tenant (MAIN-592), so an ordinary
    // tenant of this bed's own is the real door — it used to have to be the
    // instance's first tenant, which is precisely the bug that fixed.
    let tenant = bed.tenant("mcpunblock").await;
    let (user, person) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let board = board_fixture(&bed.db(), tenant).await;
    let card = approved_card(&bed.db(), tenant, board, ws, user, "mcp ruling").await;
    state
        .tasks
        .attach_label(tenant, card.id, "blocked")
        .await
        .expect("stop");

    let backend = McpBackend {
        state: state.clone(),
    };
    let caller = McpCaller {
        person_id: person,
        user_id: user,
        tenant_id: tenant,
    };
    backend
        .comment_task(
            caller.clone(),
            card.id.to_string(),
            "Ruled over MCP.".into(),
            Some("an agent".into()),
            true,
        )
        .await
        .expect("comment and unblock over mcp");

    let on = labels(&state, card.id).await;
    assert!(!on.iter().any(|l| l == "blocked"), "AC-10: cleared: {on:?}");
    assert!(on.iter().any(|l| l == "agent-ready"), "AC-10: re-armed");
    assert!(
        row(&state, tenant, card.id).await.unblocked_at.is_some(),
        "AC-10: stamped"
    );

    let kinds = event_kinds(&bed, tenant).await;
    assert!(
        kinds.iter().any(|k| k == "task.comment.created"),
        "AC-11: the comment reached the feed: {kinds:?}"
    );
    assert!(
        kinds.iter().any(|k| k == "task.unblocked"),
        "AC-13: and so did the restart: {kinds:?}"
    );

    // AC-5 holds on this door too, and refuses before anything is written.
    assert!(
        backend
            .comment_task(caller, card.id.to_string(), "  ".into(), None, true)
            .await
            .is_err(),
        "an unblock over MCP still needs a ruling"
    );

    bed.teardown().await;
}

/// AC-13: the event names the labels that actually came off, which is what
/// tells a human's restart from an agent removing one label at a time.
#[tokio::test]
async fn the_unblock_event_names_what_it_cleared() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("unbevent").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let board = board_fixture(&bed.db(), tenant).await;
    let card = approved_card(&bed.db(), tenant, board, ws, user, "say what you did").await;
    state
        .tasks
        .attach_label(tenant, card.id, "spec-blocked")
        .await
        .expect("stop");

    post(&state, user, tenant, card.id, "Ruled.", true)
        .await
        .expect("unblock");

    let payload: serde_json::Value = bed
        .db()
        .query_scalar(
            "SELECT payload FROM events
              WHERE tenant_id = $1 AND kind = 'task.unblocked'
              ORDER BY occurred_at DESC LIMIT 1",
            params![tenant],
        )
        .await
        .expect("the unblock event");
    assert_eq!(
        payload["cleared"],
        serde_json::json!(["spec-blocked"]),
        "the event says which stop was lifted: {payload}"
    );

    bed.teardown().await;
}

/// Tenant isolation on the changed route: another tenant's card is not even
/// resolvable, so the flag cannot reach it.
#[tokio::test]
async fn another_tenants_card_cannot_be_unblocked() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let mine = bed.tenant("unbmine").await;
    let theirs = bed.tenant("unbtheirs").await;
    let (me, _) = bed.user(mine, "member").await;
    let (them, _) = bed.user(theirs, "member").await;
    let ws = bed.workspace(theirs).await;
    let state = bed.app_state().await;
    let board = board_fixture(&bed.db(), theirs).await;
    let card = approved_card(&bed.db(), theirs, board, ws, them, "not yours").await;
    state
        .tasks
        .attach_label(theirs, card.id, "blocked")
        .await
        .expect("stop");

    let err = post(&state, me, mine, card.id, "Ruled.", true)
        .await
        .expect_err("another tenant's card");
    assert!(
        matches!(err, nook_control::error::ApiError::NotFound),
        "got {err:?}"
    );
    assert!(
        labels(&state, card.id).await.iter().any(|l| l == "blocked"),
        "and it is still stopped"
    );
    assert!(
        row(&state, theirs, card.id).await.unblocked_at.is_none(),
        "and unstamped"
    );

    bed.teardown().await;
}

async fn event_kinds(bed: &TestBed, tenant: TenantId) -> Vec<String> {
    bed.db()
        .query_all::<(String,)>(
            "SELECT kind FROM events WHERE tenant_id = $1",
            params![tenant],
        )
        .await
        .expect("events")
        .into_iter()
        .map(|(k,)| k)
        .collect()
}
