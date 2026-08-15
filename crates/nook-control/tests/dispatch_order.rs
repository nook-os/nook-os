//! Which queued loop job gets a freed executor (MAIN-509).
//!
//! Placement used to be a race between durable-queue work items, which made it
//! accidental LIFO: an unplaceable job is re-armed after a delay while a freshly
//! raised one is delivered at once, so the newest arrival won whichever slot
//! happened to free. These tests are about the rule that replaced it — card
//! priority, then how long the job has waited — and they reproduce the observed
//! shape rather than the easy one: the losers were never stalled, they were
//! re-evaluated every 30 seconds and beaten every time.
//!
//! Needs Postgres: `DATABASE_URL` (`NOOK_REQUIRE_DB=1` in the suite).

use chrono::{DateTime, Duration, Utc};
use nook_control::services::jobs;
use nook_control::state::AppState;
use nook_db::{params, Db};
use nook_types::*;
use serde_json::json;
use uuid::Uuid;

use nook_testkit::TestBed;

/// A board with one column, reused by every card in a test.
async fn board(bed: &TestBed, tenant: TenantId) -> (BoardId, ColumnId) {
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
             VALUES ($1,$2,'Todo',0,'unstarted')",
            params![col, board],
        )
        .await
        .expect("column");
    (board, col)
}

/// A card at a given board priority: `1` urgent … `4` low, `0` unset.
async fn card(
    bed: &TestBed,
    tenant: TenantId,
    (board, col): (BoardId, ColumnId),
    creator: UserId,
    priority: i32,
) -> TaskId {
    let task = TaskId::new();
    bed.db()
        .exec(
            "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, type, created_by, priority)
             VALUES ($1,$2,$3,$4,'t','task',$5,$6)",
            params![task, tenant, board, col, creator, priority],
        )
        .await
        .expect("task");
    task
}

/// A queued job of `kind`, stamped with the moment it was raised — the age half
/// of the ordering is the whole point, so no test may leave it to the default.
async fn queued_at(
    bed: &TestBed,
    tenant: TenantId,
    user: UserId,
    target: TaskId,
    kind: &str,
    created: DateTime<Utc>,
) -> JobId {
    let id = JobId::new();
    bed.db()
        .exec(
            "INSERT INTO loop_jobs (id, tenant_id, kind, target_task_id, requested_by, state,
                                    created_at, updated_at)
             VALUES ($1,$2,$3,$4,$5,'queued',$6,$6)",
            params![id, tenant, kind, target, user, created],
        )
        .await
        .expect("job");
    id
}

/// An online node owned by `person`, authorized for the loop runtime, holding
/// `capacity` jobs at once and accepting only `spec` and `decompose` work.
async fn node(
    bed: &TestBed,
    tenant: TenantId,
    person: Uuid,
    capacity: u32,
    kinds: &[&str],
) -> NodeId {
    let id = NodeId::new();
    bed.db()
        .exec(
            "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status, owner_person_id,
                                capabilities)
             VALUES ($1,$2,$3,$4,'online',$5,$6)",
            params![
                id,
                tenant,
                format!("n-{}", id.0.simple()),
                format!("h-{}", id.0.simple()),
                person,
                json!({
                    "loop_kinds": kinds,
                    "max_loop_jobs": capacity,
                    "runtime_auth": [
                        { "id": "claude", "label": "Claude Code",
                          "runtime": "claude", "state": "authorized" }
                    ]
                })
            ],
        )
        .await
        .expect("node");
    id
}

/// The run holding the node's only slot concludes — what "an executor frees"
/// is, in the only terms placement reads.
async fn free_the_executor(bed: &TestBed, job: JobId) {
    bed.db()
        .exec(
            "UPDATE loop_jobs SET state = 'completed' WHERE id = $1",
            params![job],
        )
        .await
        .expect("free");
}

/// Run one dispatch pass and name the single job it placed.
async fn placed_one(state: &AppState, tenant: TenantId) -> Option<JobId> {
    let placed = jobs::place_queued_in_order(state, tenant)
        .await
        .expect("pass");
    assert!(
        placed.len() <= 1,
        "one free slot must yield at most one placement, got {}",
        placed.len()
    );
    placed.first().map(|j| j.id)
}

struct Bench {
    state: AppState,
    tenant: TenantId,
    user: UserId,
    board: (BoardId, ColumnId),
}

/// A tenant with one single-slot node, and that slot already taken — the
/// at-capacity fleet every one of these tests starts from.
async fn bench(bed: &TestBed, hint: &str) -> (Bench, JobId) {
    let tenant = bed.tenant(hint).await;
    let (user, person) = bed.user(tenant, "owner").await;
    let b = board(bed, tenant).await;
    let node = node(bed, tenant, person, 1, &["spec", "decompose"]).await;

    let incumbent = queued_at(
        bed,
        tenant,
        user,
        card(bed, tenant, b, user, 0).await,
        "spec",
        Utc::now() - Duration::hours(2),
    )
    .await;
    bed.db()
        .exec(
            "UPDATE loop_jobs SET state = 'running', executor_node_id = $2 WHERE id = $1",
            params![incumbent, node],
        )
        .await
        .expect("occupy");

    (
        Bench {
            state: bed.app_state().await,
            tenant,
            user,
            board: b,
        },
        incumbent,
    )
}

impl Bench {
    /// A queued job on a fresh card of the given priority, raised `mins_ago`.
    async fn raise(&self, bed: &TestBed, priority: i32, mins_ago: i64) -> JobId {
        let target = card(bed, self.tenant, self.board, self.user, priority).await;
        queued_at(
            bed,
            self.tenant,
            self.user,
            target,
            "spec",
            Utc::now() - Duration::minutes(mins_ago),
        )
        .await
    }
}

/// AC-1/AC-5/AC-6, the incident itself: three jobs that have been waiting, a
/// fourth raised at the very instant the executor frees, and the oldest wins.
///
/// The fresh job's zero-delay advantage is what this is about. It still reaches
/// dispatch first — nothing about the queue changed — it just no longer places
/// itself when it gets there.
#[tokio::test]
async fn the_oldest_queued_job_wins_the_freed_executor() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (b, incumbent) = bench(&bed, "lifo").await;

    let waited_42 = b.raise(&bed, 0, 42).await;
    let _waited_42b = b.raise(&bed, 0, 42).await;
    let _waited_28 = b.raise(&bed, 0, 28).await;

    // The slot frees and a new card is promoted in the same moment — which is
    // no coincidence: a run concluding is exactly what raises the next one.
    free_the_executor(&bed, incumbent).await;
    let just_now = b.raise(&bed, 0, 0).await;

    let winner = placed_one(&b.state, b.tenant).await;
    assert_eq!(
        winner,
        Some(waited_42),
        "the executor goes to the longest wait, not the newest arrival"
    );
    assert_ne!(winner, Some(just_now));

    bed.teardown().await;
}

/// AC-2: with work arriving on every tick, an old job is still placed. The
/// losers in the incident were re-evaluated every 30 seconds and beaten every
/// time, so "eventually runs on an idle node" is not the property — this is.
#[tokio::test]
async fn a_continuous_stream_of_new_jobs_cannot_starve_an_old_one() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (b, mut holding) = bench(&bed, "starve").await;

    let backlog = [
        b.raise(&bed, 0, 90).await,
        b.raise(&bed, 0, 75).await,
        b.raise(&bed, 0, 60).await,
    ];

    for (tick, expected) in backlog.iter().enumerate() {
        // Every tick raises a rival and frees exactly one slot.
        let rival = b.raise(&bed, 0, 0).await;
        free_the_executor(&bed, holding).await;

        let winner = placed_one(&b.state, b.tenant).await;
        assert_eq!(
            winner,
            Some(*expected),
            "tick {tick}: the backlog drains oldest-first even as new work arrives"
        );
        assert_ne!(winner, Some(rival));
        holding = winner.expect("a placement");
    }

    bed.teardown().await;
}

/// AC-3: card priority is honoured. `!!` urgent jobs losing to a `↑` high one —
/// what actually happened — cannot recur.
#[tokio::test]
async fn urgent_cards_are_not_beaten_by_a_high_one() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (b, incumbent) = bench(&bed, "prio").await;

    let urgent_older = b.raise(&bed, 1, 42).await;
    let _urgent_newer = b.raise(&bed, 1, 28).await;
    let high_newest = b.raise(&bed, 2, 1).await;
    // An unset priority sorts LAST, not first: nobody having said is not a
    // claim that the work matters least, but it cannot outrank what was said.
    let _unset_oldest = b.raise(&bed, 0, 300).await;

    free_the_executor(&bed, incumbent).await;

    let winner = placed_one(&b.state, b.tenant).await;
    assert_eq!(
        winner,
        Some(urgent_older),
        "the oldest urgent card, ahead of both the high one and an older unset one"
    );
    assert_ne!(winner, Some(high_newest));

    bed.teardown().await;
}

/// AC-4: this is fairness, not FIFO. An urgent hotfix raised seconds ago takes
/// the executor ahead of a backlog that has been waiting all afternoon.
#[tokio::test]
async fn an_urgent_arrival_goes_ahead_of_an_older_backlog() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (b, incumbent) = bench(&bed, "hotfix").await;

    let _low_and_ancient = b.raise(&bed, 4, 240).await;
    let _medium_and_old = b.raise(&bed, 3, 120).await;
    free_the_executor(&bed, incumbent).await;
    let hotfix = b.raise(&bed, 1, 0).await;

    assert_eq!(
        placed_one(&b.state, b.tenant).await,
        Some(hotfix),
        "an urgent card does not wait behind a backlog it outranks"
    );

    bed.teardown().await;
}

/// A job that cannot be placed for a reason of its OWN must not hand its wait
/// to everything behind it. The pass runs down the order; it does not stop at
/// the first refusal.
#[tokio::test]
async fn an_unplaceable_job_does_not_block_the_ones_behind_it() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("blocked").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let b = board(&bed, tenant).await;
    // The one node accepts `spec` work and nothing else.
    let _node = node(&bed, tenant, person, 1, &["spec"]).await;
    let state = bed.app_state().await;

    let unplaceable = queued_at(
        &bed,
        tenant,
        user,
        card(&bed, tenant, b, user, 1).await,
        "decompose",
        Utc::now() - Duration::minutes(90),
    )
    .await;
    let behind_it = queued_at(
        &bed,
        tenant,
        user,
        card(&bed, tenant, b, user, 1).await,
        "spec",
        Utc::now() - Duration::minutes(30),
    )
    .await;

    assert_eq!(
        placed_one(&state, tenant).await,
        Some(behind_it),
        "the runnable job takes the executor the blocked one cannot use"
    );
    assert_eq!(
        state
            .jobs
            .get(tenant, unplaceable)
            .await
            .expect("read")
            .expect("job")
            .state,
        "queued",
        "and the blocked one keeps waiting, with its own reason"
    );

    bed.teardown().await;
}

/// MAIN-329: an `investigate` run is not in the dispatch order at all.
///
/// No node can advertise the kind, so the row would sit at the head of the
/// order forever with a reason that never moves — and `DISPATCH_PASS_LIMIT`
/// documents that such a head is never lifted by the window itself. One per
/// accepted support email, all sorted into the unset-priority bucket ahead of
/// every newer job, is how a tenant stops placing work entirely. They are
/// therefore excluded from `queued_in_dispatch_order`, not merely refused by
/// `select_executor`.
///
/// `DISPATCH_PASS_LIMIT` of them are raised here, each older than the runnable
/// job, so the assertion fails on a build that only refuses them.
#[tokio::test]
async fn investigate_runs_are_not_in_the_dispatch_order() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("investigate").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let b = board(&bed, tenant).await;
    let _node = node(&bed, tenant, person, 1, &["spec"]).await;
    let state = bed.app_state().await;

    let mut seeded = Vec::new();
    for i in 0..jobs::DISPATCH_PASS_LIMIT {
        seeded.push(
            queued_at(
                &bed,
                tenant,
                user,
                card(&bed, tenant, b, user, 0).await,
                "investigate",
                Utc::now() - Duration::hours(2) - Duration::seconds(i as i64),
            )
            .await,
        );
    }
    let runnable = queued_at(
        &bed,
        tenant,
        user,
        card(&bed, tenant, b, user, 0).await,
        "spec",
        Utc::now() - Duration::minutes(1),
    )
    .await;

    let order = state
        .jobs
        .queued_in_dispatch_order(tenant)
        .await
        .expect("order");
    assert_eq!(
        order,
        vec![runnable],
        "only the job a node could actually take is offered the executor"
    );
    assert_eq!(
        placed_one(&state, tenant).await,
        Some(runnable),
        "and the window is not consumed by the seeded runs ahead of it"
    );
    for id in seeded {
        assert_eq!(
            state
                .jobs
                .get(tenant, id)
                .await
                .expect("read")
                .expect("job")
                .state,
            "queued",
            "the seeded run is untouched — skipped, not canceled"
        );
    }

    bed.teardown().await;
}

/// The pass fills every free slot it can, in order — one occasion places as
/// much as capacity allows rather than one job per delivery.
#[tokio::test]
async fn a_pass_fills_all_the_capacity_that_is_free() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("fill").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let b = board(&bed, tenant).await;
    let _node = node(&bed, tenant, person, 2, &["spec"]).await;
    let state = bed.app_state().await;

    let first = queued_at(
        &bed,
        tenant,
        user,
        card(&bed, tenant, b, user, 0).await,
        "spec",
        Utc::now() - Duration::minutes(60),
    )
    .await;
    let second = queued_at(
        &bed,
        tenant,
        user,
        card(&bed, tenant, b, user, 0).await,
        "spec",
        Utc::now() - Duration::minutes(30),
    )
    .await;
    let third = queued_at(
        &bed,
        tenant,
        user,
        card(&bed, tenant, b, user, 0).await,
        "spec",
        Utc::now() - Duration::minutes(5),
    )
    .await;

    let placed: Vec<JobId> = jobs::place_queued_in_order(&state, tenant)
        .await
        .expect("pass")
        .into_iter()
        .map(|j| j.id)
        .collect();
    assert_eq!(placed, vec![first, second], "both slots, oldest first");
    assert_eq!(
        state
            .jobs
            .get(tenant, third)
            .await
            .expect("read")
            .expect("job")
            .state,
        "queued",
        "and the third waits for the next slot rather than overfilling the node"
    );

    bed.teardown().await;
}
