//! The two ways a `queued` loop job ends without ever running (MAIN-496): its
//! target card reaches a terminal column, or its `queued_reason` stands
//! unchanged past the threshold. Both negatives are here too, and they are the
//! load-bearing half — a rule that cancels a job about to be placed is a
//! regression, not a fix.
//!
//! **Deliberately dialect-clean, unlike its sibling `job_reaper`.** Every row is
//! aged by binding a `DateTime<Utc>` computed in Rust rather than by
//! `now() - interval`, and every statement here is portable SQL — so this
//! binary runs on BOTH legs. That matters more than usual: `job_reaper` is
//! excluded from the SQLite leg (MAIN-268), and without this file the two
//! `WITH … UPDATE … FROM … RETURNING` statements these endings are built on
//! would ship to the SQLite engine with nothing having ever executed them
//! there, on a path that runs every thirty seconds.

use nook_control::services::jobs;
use nook_db::{params, Db};
use nook_types::*;

use nook_testkit::TestBed;

fn ago(secs: i64) -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now() - chrono::Duration::seconds(secs)
}

/// A board + column + a team-visible card, in a column of the given type —
/// which is what tells a live card from a finished one (AC-1's whole rule).
async fn card_in(
    bed: &TestBed,
    tenant: TenantId,
    creator: UserId,
    column_type: &str,
) -> (TaskId, BoardId) {
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
             VALUES ($1,$2,'Triage',0,$3)",
            params![col, board, column_type],
        )
        .await
        .expect("column");
    let task = TaskId::new();
    bed.db()
        .exec(
            "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, type, created_by)
             VALUES ($1,$2,$3,$4,'t','task',$5)",
            params![task, tenant, board, col, creator],
        )
        .await
        .expect("task");
    (task, board)
}

async fn card(bed: &TestBed, tenant: TenantId, creator: UserId, column_type: &str) -> TaskId {
    card_in(bed, tenant, creator, column_type).await.0
}

/// A card of `workspace` carrying a recorded PR and a board number — the shape
/// `tasks_with_pr` sources REPAIR items from.
async fn card_with_pr(
    bed: &TestBed,
    tenant: TenantId,
    creator: UserId,
    workspace: WorkspaceId,
    column_type: &str,
    number: i32,
) -> TaskId {
    let task = card(bed, tenant, creator, column_type).await;
    bed.db()
        .exec(
            "UPDATE tasks SET workspace_id = $2, number = $3, pr_url = $4 WHERE id = $1",
            params![
                task,
                workspace.0,
                number,
                format!("https://github.com/o/r/pull/{number}")
            ],
        )
        .await
        .expect("card with a PR");
    task
}

/// A `queued` build job on `target`, with no executor. The two clocks are set
/// apart because they answer different questions: `created_secs_ago` is the
/// whole wait, `reason_secs_ago` is how long the reason has stood — and only
/// the second is what the starvation threshold measures.
async fn queued_job(
    bed: &TestBed,
    tenant: TenantId,
    user: UserId,
    target: TaskId,
    reason: Option<&str>,
    created_secs_ago: i64,
    reason_secs_ago: i64,
) -> JobId {
    let id = JobId::new();
    bed.db()
        .exec(
            "INSERT INTO loop_jobs
                (id, tenant_id, kind, target_task_id, requested_by, state, queued_reason,
                 created_at, updated_at)
             VALUES ($1,$2,'build',$3,$4,'queued',$5,$6,$7)",
            params![
                id,
                tenant,
                target,
                user,
                reason,
                ago(created_secs_ago),
                ago(reason_secs_ago)
            ],
        )
        .await
        .expect("queued job");
    id
}

async fn job_state(bed: &TestBed, id: JobId) -> String {
    bed.db()
        .query_scalar("SELECT state FROM loop_jobs WHERE id = $1", params![id])
        .await
        .expect("state")
}

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

async fn labels_of(bed: &TestBed, task: TaskId) -> Vec<String> {
    bed.db()
        .query_scalar_all(
            "SELECT l.name FROM task_labels tl JOIN labels l ON l.id = tl.label_id
              WHERE tl.task_id = $1",
            params![task],
        )
        .await
        .expect("labels")
}

async fn comments_of(bed: &TestBed, task: TaskId) -> Vec<String> {
    bed.db()
        .query_scalar_all(
            "SELECT body_md FROM task_comments WHERE task_id = $1",
            params![task],
        )
        .await
        .expect("comments")
}

#[tokio::test]
async fn a_queued_job_whose_card_is_finished_is_canceled() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ending").await;
    let (user, _p) = bed.user(tenant, "owner").await;
    let done = card(&bed, tenant, user, "completed").await;
    let dropped = card(&bed, tenant, user, "canceled").await;
    // Freshly queued, well inside any threshold: the closed card is the whole
    // rule (AC-1), so neither age nor a recorded reason is required.
    let on_done = queued_job(&bed, tenant, user, done, None, 0, 0).await;
    let on_dropped = queued_job(
        &bed,
        tenant,
        user,
        dropped,
        Some("no eligible executor"),
        0,
        0,
    )
    .await;
    let state = bed.app_state().await;

    let n = jobs::cancel_queued_on_finished_cards(&state, tenant)
        .await
        .expect("scan");
    assert_eq!(n, 2, "both terminal column types end their queued run");

    for id in [on_done, on_dropped] {
        // `canceled`, never a new `queued -> failed` (AC-4): the run never
        // happened, and `failed` is what the build failure ladder reads.
        assert_eq!(job_state(&bed, id).await, "canceled");
        let t = transcript_text(&bed, id).await;
        assert!(
            t.contains("target card reached a terminal column"),
            "the reason is on the job: {t:?}"
        );
    }
    // Idempotent across replicas: the second scan finds nothing in the guard set.
    assert_eq!(
        jobs::cancel_queued_on_finished_cards(&state, tenant)
            .await
            .expect("scan 2"),
        0
    );

    bed.teardown().await;
}

#[tokio::test]
async fn a_queued_job_on_an_open_card_below_the_threshold_is_untouched() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ending").await;
    let (user, _p) = bed.user(tenant, "owner").await;
    let open = card(&bed, tenant, user, "started").await;
    let waiting = queued_job(
        &bed,
        tenant,
        user,
        open,
        Some("no eligible executor"),
        60,
        60,
    )
    .await;
    let state = bed.app_state().await;

    assert_eq!(
        jobs::cancel_queued_on_finished_cards(&state, tenant)
            .await
            .expect("finished scan"),
        0,
        "an open card's run is not pointless"
    );
    assert_eq!(
        jobs::escalate_starved_queued(&state, tenant, 1_800)
            .await
            .expect("starve scan"),
        0,
        "a minute of waiting is not starvation (AC-6)"
    );
    assert_eq!(job_state(&bed, waiting).await, "queued");
    assert!(comments_of(&bed, open).await.is_empty());
    assert!(labels_of(&bed, open).await.is_empty(), "and unmarked");

    bed.teardown().await;
}

#[tokio::test]
async fn a_starved_queued_job_is_canceled_and_escalated() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ending").await;
    let (user, _p) = bed.user(tenant, "owner").await;
    let target = card(&bed, tenant, user, "started").await;
    let state = bed.app_state().await;
    state
        .tasks
        .set_agent_ready(tenant, target, true)
        .await
        .expect("agent-ready");

    let reason = "no eligible executor: every eligible node is at its loop-job capacity";
    // Queued five hours, but this sentence has only stood for two. The threshold
    // measured the two, so that is the figure the escalation must quote — the
    // five would assert something about the reason nobody checked.
    let starved = queued_job(&bed, tenant, user, target, Some(reason), 18_000, 7_200).await;

    let n = jobs::escalate_starved_queued(&state, tenant, 1_800)
        .await
        .expect("scan");
    assert_eq!(n, 1);
    assert_eq!(job_state(&bed, starved).await, "canceled");

    // AC-3: without these, convergence raises the very same run again next pass
    // — cancel, enqueue, cancel, forever. `agent-ready` is what `fresh_items`
    // reads; `blocked` is what removes the card from the REPAIR candidate set,
    // which never consults `agent-ready` at all.
    let labels = labels_of(&bed, target).await;
    assert!(
        !labels.iter().any(|l| l == "agent-ready"),
        "the escalation strips agent-ready: {labels:?}"
    );
    assert!(
        labels.iter().any(|l| l == "blocked"),
        "and marks the card blocked: {labels:?}"
    );

    let comments = comments_of(&bed, target).await;
    assert_eq!(comments.len(), 1, "one comment, on the card: {comments:?}");
    assert!(
        comments[0].contains("stood unchanged for 2h") && comments[0].contains(reason),
        "it names how long the REASON stood, and why: {:?}",
        comments[0]
    );
    assert!(
        !comments[0].contains("5h"),
        "not the whole queued wait, which this rule never measured: {:?}",
        comments[0]
    );
    assert!(transcript_text(&bed, starved).await.contains(reason));

    assert_eq!(
        jobs::escalate_starved_queued(&state, tenant, 1_800)
            .await
            .expect("scan 2"),
        0,
        "a job is escalated once, however many replicas scan"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn a_queued_job_whose_reason_keeps_changing_is_not_escalated() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ending").await;
    let (user, _p) = bed.user(tenant, "owner").await;
    // Two cards, because 0050's index allows one live build run per card.
    let stuck_card = card(&bed, tenant, user, "started").await;
    let moving_card = card(&bed, tenant, user, "started").await;
    let state = bed.app_state().await;

    // Both queued two hours; one is stuck on the same sentence, the other is
    // moving through gates toward placement.
    let stuck = queued_job(
        &bed,
        tenant,
        user,
        stuck_card,
        Some("at capacity"),
        7_200,
        7_200,
    )
    .await;
    let moving = queued_job(
        &bed,
        tenant,
        user,
        moving_card,
        Some("at capacity"),
        7_200,
        7_200,
    )
    .await;
    state
        .jobs
        .set_queued_reason(
            moving,
            "waiting for the node holding this card's worktree",
            Some(QueuedReason::PinnedNodeUnavailable {
                node_name: "builder-2".into(),
            }),
        )
        .await
        .expect("progress");
    // And a re-write of the SAME sentence is not progress: it must not reset
    // the clock, which is why `set_queued_reason` is a no-op when nothing moved.
    assert_eq!(
        state
            .jobs
            .set_queued_reason(stuck, "at capacity", Some(QueuedReason::AtCapacity))
            .await
            .expect("repeat"),
        0,
        "re-writing an unchanged reason touches no row"
    );

    let n = jobs::escalate_starved_queued(&state, tenant, 1_800)
        .await
        .expect("scan");
    assert_eq!(n, 1, "only the job that stopped moving is escalated (AC-6)");
    assert_eq!(job_state(&bed, stuck).await, "canceled");
    assert_eq!(job_state(&bed, moving).await, "queued");

    bed.teardown().await;
}

#[tokio::test]
async fn a_queued_job_with_no_reason_yet_is_never_starved() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ending").await;
    let (user, _p) = bed.user(tenant, "owner").await;
    let target = card(&bed, tenant, user, "started").await;
    // Old, but dispatch has never looked at it — loops may simply have been off
    // the whole time, and a job nobody tried to place has not starved.
    let unseen = queued_job(&bed, tenant, user, target, None, 7_200, 7_200).await;
    let state = bed.app_state().await;

    assert_eq!(
        jobs::escalate_starved_queued(&state, tenant, 1_800)
            .await
            .expect("scan"),
        0
    );
    assert_eq!(job_state(&bed, unseen).await, "queued");

    bed.teardown().await;
}

#[tokio::test]
async fn a_queued_ending_never_reaches_another_tenants_board() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    // The reaper runs across tenants, so each scan is scoped to the one whose
    // `loops.enabled` the caller checked. A tenant with loops OFF must not have
    // its runs canceled and its cards marked up because a neighbour has loops
    // on — MAIN-239 promises such a job simply waits for the switch.
    let (ours, theirs) = (bed.tenant("mine").await, bed.tenant("theirs").await);
    let (us, _p) = bed.user(ours, "owner").await;
    let (them, _p2) = bed.user(theirs, "owner").await;
    let our_done = card(&bed, ours, us, "completed").await;
    let their_done = card(&bed, theirs, them, "completed").await;
    let their_open = card(&bed, theirs, them, "started").await;
    let ours_doomed = queued_job(&bed, ours, us, our_done, None, 0, 0).await;
    let theirs_doomed = queued_job(&bed, theirs, them, their_done, None, 0, 0).await;
    let theirs_starved = queued_job(
        &bed,
        theirs,
        them,
        their_open,
        Some("at capacity"),
        7_200,
        7_200,
    )
    .await;
    let state = bed.app_state().await;

    assert_eq!(
        jobs::cancel_queued_on_finished_cards(&state, ours)
            .await
            .expect("finished scan"),
        1
    );
    assert_eq!(
        jobs::escalate_starved_queued(&state, ours, 1_800)
            .await
            .expect("starve scan"),
        0
    );
    assert_eq!(job_state(&bed, ours_doomed).await, "canceled");
    assert_eq!(job_state(&bed, theirs_doomed).await, "queued");
    assert_eq!(job_state(&bed, theirs_starved).await, "queued");
    assert!(comments_of(&bed, their_open).await.is_empty());
    assert!(labels_of(&bed, their_open).await.is_empty());

    bed.teardown().await;
}

#[tokio::test]
async fn an_escalated_card_leaves_the_repair_candidate_set() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ending").await;
    let (user, _p) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    // Three cards with a recorded PR, which is the whole of what a repair item
    // is sourced from — `agent-ready` is never consulted there, so the strip
    // alone would have left every one of these cycling forever.
    let escalated = card_with_pr(&bed, tenant, user, ws, "review", 1).await;
    let finished = card_with_pr(&bed, tenant, user, ws, "completed", 2).await;
    let live = card_with_pr(&bed, tenant, user, ws, "review", 3).await;
    let starved = queued_job(
        &bed,
        tenant,
        user,
        escalated,
        Some("at capacity"),
        7_200,
        7_200,
    )
    .await;

    assert_eq!(
        jobs::escalate_starved_queued(&state, tenant, 1_800)
            .await
            .expect("scan"),
        1
    );
    assert_eq!(job_state(&bed, starved).await, "canceled");

    let candidates: Vec<TaskId> = state
        .tasks
        .tasks_with_pr(tenant, ws)
        .await
        .expect("repair candidates")
        .into_iter()
        .map(|(id, _, _)| id)
        .collect();
    assert_eq!(
        candidates,
        vec![live],
        "only the untouched in-review card is still owed a repair run"
    );
    assert!(
        !candidates.contains(&escalated),
        "the escalated card is out — `blocked` is what stops the repair cycle"
    );
    assert!(
        !candidates.contains(&finished),
        "and a finished card is out, which is what makes AC-1's cancel final"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn a_finished_card_is_not_re_picked_even_carrying_agent_ready() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ending").await;
    let (user, _p) = bed.user(tenant, "owner").await;
    let done = card(&bed, tenant, user, "completed").await;
    let open = card(&bed, tenant, user, "unstarted").await;
    let state = bed.app_state().await;
    for c in [done, open] {
        state
            .tasks
            .set_agent_ready(tenant, c, true)
            .await
            .expect("agent-ready");
    }

    // AC-3 says AC-1 needs no strip because a completed card is not re-enqueued.
    // This is that claim checked rather than assumed — and against the SAME
    // parameters `BuildWork::fresh_items` passes, not a copy of them, so the
    // test cannot keep passing about a query nothing runs.
    let picked = state
        .tasks
        .pick_tasks(
            tenant,
            user,
            nook_control::services::work_source::fresh_pick_params(None),
        )
        .await
        .expect("pick");
    let ids: Vec<TaskId> = picked.iter().map(|t| t.id).collect();
    assert!(ids.contains(&open), "the open card is pickable: {ids:?}");
    assert!(
        !ids.contains(&done),
        "a card in a completed column is never picked, label or not: {ids:?}"
    );

    bed.teardown().await;
}
