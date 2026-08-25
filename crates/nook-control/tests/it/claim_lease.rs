//! Claim leases and the claim reaper (MAIN-229).
//!
//! The property under test is a fence, not a feature: `claim_expires_at IS NULL`
//! means "no agent claim", and a card in that state is never moved, labelled or
//! examined however dead its session looks (AC-7). Everything else — requeue on a
//! dead worker, escalate past the cap, renew — is what the lease buys once it IS
//! set.
//!
//! Each test runs on its OWN private database (`nook_testkit::TestBed`), so the
//! reaper's global scans only ever see this test's rows.
//!
//! Needs Postgres: `DATABASE_URL` (`NOOK_REQUIRE_DB=1` in the suite).

use nook_control::services::claim_reaper;
use nook_control::state::AppState;
use nook_db::dialect::{time_math, type_mapping};
use nook_db::{params, Db, DbPool};
use nook_testkit::TestBed;
use nook_types::*;

/// A board with the three columns the lifecycle names, returned in
/// `(board, todo, in_progress, review)` order.
async fn board(db: &DbPool, tenant: TenantId) -> (BoardId, ColumnId, ColumnId, ColumnId) {
    let board = BoardId::new();
    db.exec(
        "INSERT INTO boards (id, tenant_id, name, key, provider) VALUES ($1,$2,'b',$3,'local')",
        params![
            board,
            tenant,
            format!("B{}", &board.0.simple().to_string()[26..32]).to_uppercase()
        ],
    )
    .await
    .expect("board");

    let mut ids = Vec::new();
    for (pos, (name, kind)) in [
        ("Todo", "unstarted"),
        ("In Progress", "started"),
        ("In Review", "review"),
    ]
    .iter()
    .enumerate()
    {
        let col = ColumnId::new();
        db.exec(
            "INSERT INTO board_columns (id, board_id, name, position, type) VALUES ($1,$2,$3,$4,$5)",
            params![col, board, name.to_string(), pos as i32, kind.to_string()],
        )
        .await
        .expect("column");
        ids.push(col);
    }
    (board, ids[0], ids[1], ids[2])
}

async fn task_in(
    db: &DbPool,
    tenant: TenantId,
    b: BoardId,
    col: ColumnId,
    creator: UserId,
) -> TaskId {
    let id = TaskId::new();
    db.exec(
        "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, type, created_by)
         VALUES ($1,$2,$3,$4,'t','task',$5)",
        params![id, tenant, b, col, creator],
    )
    .await
    .expect("task");
    id
}

/// A node whose `last_seen_at` is `secs_ago` seconds in the past.
async fn node_seen(db: &DbPool, tenant: TenantId, secs_ago: i64) -> NodeId {
    let id = NodeId::new();
    db.exec(
        &format!(
            "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status, last_seen_at)
             VALUES ($1,$2,$3,$4,'online', {ago})",
            ago = time_math(db.engine())
                .now_minus_scaled(&type_mapping(db.engine()).cast("$5", "bigint"), "1 second")
        ),
        params![
            id,
            tenant,
            format!("n-{}", id.0.simple()),
            format!("h-{}", id.0.simple()),
            secs_ago
        ],
    )
    .await
    .expect("node");
    id
}

async fn session(db: &DbPool, tenant: TenantId, node: NodeId, status: &str) -> SessionId {
    let id = SessionId::new();
    db.exec(
        "INSERT INTO sessions (id, tenant_id, node_id, runtime, status) VALUES ($1,$2,$3,'bash',$4)",
        params![id, tenant, node, status.to_string()],
    )
    .await
    .expect("session");
    id
}

/// Put a card in the state start-work leaves it in: In Progress, bound to a
/// session on a node, holding a lease that expires in `lease_in_secs`.
async fn leased(
    db: &DbPool,
    task: TaskId,
    node: NodeId,
    sess: SessionId,
    in_progress: ColumnId,
    assignee: UserId,
    lease_in_secs: i64,
) {
    db.exec(
        &format!(
            "UPDATE tasks SET column_id = $2, assigned_node_id = $3, session_id = $4,
                    assignee_user_id = $5,
                    claim_expires_at = {expiry}
             WHERE id = $1",
            expiry = time_math(db.engine())
                .now_plus_scaled(&type_mapping(db.engine()).cast("$6", "bigint"), "1 second")
        ),
        params![task, in_progress, node, sess, assignee, lease_in_secs],
    )
    .await
    .expect("lease");
}

struct Card {
    column: ColumnId,
    node: Option<NodeId>,
    session: Option<SessionId>,
    lease: Option<chrono::DateTime<chrono::Utc>>,
}

async fn card(db: &DbPool, id: TaskId) -> Card {
    let row: (
        ColumnId,
        Option<NodeId>,
        Option<SessionId>,
        Option<chrono::DateTime<chrono::Utc>>,
    ) = db
        .query_one(
            "SELECT column_id, assigned_node_id, session_id, claim_expires_at
               FROM tasks WHERE id = $1",
            params![id],
        )
        .await
        .expect("card");
    Card {
        column: row.0,
        node: row.1,
        session: row.2,
        lease: row.3,
    }
}

async fn labels_of(db: &DbPool, id: TaskId) -> Vec<String> {
    let rows: Vec<(String,)> = db
        .query_all(
            "SELECT l.name FROM task_labels tl JOIN labels l ON l.id = tl.label_id
              WHERE tl.task_id = $1 ORDER BY l.name",
            params![id],
        )
        .await
        .expect("labels");
    rows.into_iter().map(|(n,)| n).collect()
}

async fn comments_of(db: &DbPool, id: TaskId) -> String {
    let rows: Vec<(String,)> = db
        .query_all(
            "SELECT body_md FROM task_comments WHERE task_id = $1 ORDER BY created_at",
            params![id],
        )
        .await
        .expect("comments");
    rows.into_iter()
        .map(|(b,)| b)
        .collect::<Vec<_>>()
        .join("\n")
}

async fn event_kinds(db: &DbPool, tenant: TenantId) -> Vec<String> {
    let rows: Vec<(String,)> = db
        .query_all(
            "SELECT kind FROM events WHERE tenant_id = $1 ORDER BY occurred_at",
            params![tenant],
        )
        .await
        .expect("events");
    rows.into_iter().map(|(k,)| k).collect()
}

async fn notifications(db: &DbPool, tenant: TenantId) -> Vec<(String, String)> {
    db.query_all(
        "SELECT level, title FROM notifications WHERE tenant_id = $1",
        params![tenant],
    )
    .await
    .expect("notifications")
}

/// An `AppState` whose claim cap is `max_claim_secs` — the knob AC-6 says must
/// be configuration rather than a constant.
async fn state_with_cap(bed: &TestBed, max_claim_secs: u64) -> AppState {
    let mut cfg = bed.config();
    cfg.max_claim_secs = max_claim_secs;
    AppState::new(bed.db(), cfg, None).await
}

// ── AC-2: only the agent claim / start-work path mints a lease ──────────────

#[tokio::test]
async fn the_agent_claim_path_leases_and_a_hand_move_does_not() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("lease").await;
    let (user, _p) = bed.user(tenant, "owner").await;
    let (b, todo, in_progress, review) = board(&bed.db(), tenant).await;
    let state = bed.app_state().await;

    // Claiming INTO `started` is an agent taking work: it carries a lease.
    let claimed = task_in(&bed.db(), tenant, b, todo, user).await;
    nook_control::routes::task_query::claim_inner(
        &state,
        tenant,
        user,
        &claimed.to_string(),
        Some("started".into()),
    )
    .await
    .expect("claim");
    let c = card(&bed.db(), claimed).await;
    assert_eq!(c.column, in_progress);
    assert!(c.lease.is_some(), "an agent claim carries a lease (AC-2)");

    // A plain column move into the SAME column does not — this is the human
    // dragging a card, and it must stay outside the reaper's fence.
    let dragged = task_in(&bed.db(), tenant, b, todo, user).await;
    nook_control::services::taskwork::move_task(&state, tenant, dragged, "In Progress")
        .await
        .expect("move");
    let d = card(&bed.db(), dragged).await;
    assert_eq!(d.column, in_progress);
    assert!(
        d.lease.is_none(),
        "a hand-moved card is never leased (AC-2, AC-7)"
    );

    // And leaving `started` gives the lease back.
    nook_control::services::taskwork::move_task(&state, tenant, claimed, "In Review")
        .await
        .expect("move out");
    let c = card(&bed.db(), claimed).await;
    assert_eq!(c.column, review);
    assert!(c.lease.is_none(), "leaving started clears the lease (AC-2)");

    bed.teardown().await;
}

#[tokio::test]
async fn start_works_stamp_leases_and_an_unclaim_clears_it() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("lease").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let node = bed.node(tenant, person).await;
    let (b, todo, in_progress, _review) = board(&bed.db(), tenant).await;
    let state = bed.app_state().await;
    let task = task_in(&bed.db(), tenant, b, todo, user).await;
    let sess = session(&bed.db(), tenant, node, "running").await;

    // The exact stamp `start_work` writes once its worktree and session exist —
    // the node round trip it does first cannot be stood up in a unit test, so
    // this exercises the statement, not the transport.
    state
        .tasks
        .record_started_work(
            task,
            nook_control::repo::tasks::StartedWork {
                workspace_id: ws.0,
                node_id: node,
                branch: "b".into(),
                worktree_path: "/w".into(),
                session_id: Some(sess.0),
                column_id: in_progress,
                checkout_id: None,
                claim_ttl_secs: 3600,
            },
        )
        .await
        .expect("start work");
    assert!(card(&bed.db(), task).await.lease.is_some());

    // An explicit unclaim gives the card back, lease and all.
    state
        .tasks
        .release_assignment(task, tenant)
        .await
        .expect("release");
    assert!(
        card(&bed.db(), task).await.lease.is_none(),
        "an unclaim clears the lease (AC-2)"
    );

    bed.teardown().await;
}

// ── AC-3: session liveness is the renewer ───────────────────────────────────

#[tokio::test]
async fn a_leased_card_whose_session_died_is_requeued() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("reap").await;
    let (user, _p) = bed.user(tenant, "owner").await;
    let (b, todo, in_progress, _r) = board(&bed.db(), tenant).await;
    let live_node = node_seen(&bed.db(), tenant, 0).await;
    let state = bed.app_state().await;

    let task = task_in(&bed.db(), tenant, b, todo, user).await;
    let dead_session = session(&bed.db(), tenant, live_node, "exited").await;
    leased(
        &bed.db(),
        task,
        live_node,
        dead_session,
        in_progress,
        user,
        3600,
    )
    .await;

    let reaped = claim_reaper::reap_lapsed_claims(&state, 120)
        .await
        .expect("reap");
    assert_eq!(reaped, 1);

    let c = card(&bed.db(), task).await;
    assert_eq!(c.column, todo, "requeued to To-Do (AC-3)");
    assert!(c.node.is_none() && c.session.is_none() && c.lease.is_none());
    assert!(
        comments_of(&bed.db(), task).await.contains("claim lapsed"),
        "the card says what happened to it (AC-6)"
    );
    assert!(event_kinds(&bed.db(), tenant)
        .await
        .contains(&"task.claim_lapsed".to_string()));
    assert!(
        notifications(&bed.db(), tenant)
            .await
            .iter()
            .any(|(level, _)| level == "warning"),
        "and a human is told (AC-6)"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn a_leased_card_on_a_dark_node_is_requeued_and_the_grace_is_honoured() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("reap").await;
    let (user, _p) = bed.user(tenant, "owner").await;
    let (b, todo, in_progress, _r) = board(&bed.db(), tenant).await;
    // Unseen for 300s, with a session that still claims to be running.
    let dark = node_seen(&bed.db(), tenant, 300).await;
    let state = bed.app_state().await;
    let task = task_in(&bed.db(), tenant, b, todo, user).await;
    let sess = session(&bed.db(), tenant, dark, "running").await;
    leased(&bed.db(), task, dark, sess, in_progress, user, 3600).await;

    // A grace WIDER than the gap sees nothing to reap — the window is the knob,
    // not a constant.
    assert_eq!(
        claim_reaper::reap_lapsed_claims(&state, 600)
            .await
            .expect("reap"),
        0
    );
    assert_eq!(card(&bed.db(), task).await.column, in_progress);

    assert_eq!(
        claim_reaper::reap_lapsed_claims(&state, 120)
            .await
            .expect("reap"),
        1
    );
    let c = card(&bed.db(), task).await;
    assert_eq!(c.column, todo);
    assert!(comments_of(&bed.db(), task).await.contains("node-offline"));

    bed.teardown().await;
}

#[tokio::test]
async fn a_live_session_on_a_live_node_is_left_alone_however_long_it_runs() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("reap").await;
    let (user, _p) = bed.user(tenant, "owner").await;
    let (b, _todo, in_progress, _r) = board(&bed.db(), tenant).await;
    let live = node_seen(&bed.db(), tenant, 0).await;
    let state = bed.app_state().await;
    let task = task_in(&bed.db(), tenant, b, in_progress, user).await;
    let sess = session(&bed.db(), tenant, live, "running").await;
    leased(&bed.db(), task, live, sess, in_progress, user, 3600).await;

    assert_eq!(
        claim_reaper::reap_lapsed_claims(&state, 120)
            .await
            .expect("reap"),
        0,
        "a silent 40-minute compile is not a failure (AC-3)"
    );
    let c = card(&bed.db(), task).await;
    assert_eq!(c.column, in_progress);
    assert!(c.lease.is_some());

    bed.teardown().await;
}

// ── AC-7: the board-safety guarantee ────────────────────────────────────────

#[tokio::test]
async fn an_unleased_card_in_progress_is_never_touched() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("safety").await;
    let (user, _p) = bed.user(tenant, "owner").await;
    let (b, _todo, in_progress, _r) = board(&bed.db(), tenant).await;
    // Every reason to reap it EXCEPT the one that matters: a dead session on a
    // node that went dark hours ago — and no lease.
    let dark = node_seen(&bed.db(), tenant, 100_000).await;
    let dead = session(&bed.db(), tenant, dark, "error").await;
    let state = bed.app_state().await;
    let task = task_in(&bed.db(), tenant, b, in_progress, user).await;
    bed.db()
        .exec(
            "UPDATE tasks SET assigned_node_id = $2, session_id = $3 WHERE id = $1",
            params![task, dark, dead],
        )
        .await
        .expect("hand-placed");

    for _ in 0..3 {
        assert_eq!(
            claim_reaper::reap_lapsed_claims(&state, 0)
                .await
                .expect("reap"),
            0
        );
        assert_eq!(
            claim_reaper::escalate_capped_claims(&state)
                .await
                .expect("escalate"),
            0
        );
    }

    let c = card(&bed.db(), task).await;
    assert_eq!(c.column, in_progress, "not moved");
    assert_eq!(c.node, Some(dark), "not cleared");
    assert_eq!(c.session, Some(dead));
    assert!(c.lease.is_none());
    assert!(labels_of(&bed.db(), task).await.is_empty(), "not labelled");
    assert!(comments_of(&bed.db(), task).await.is_empty());

    bed.teardown().await;
}

// ── AC-4: the cap escalates, it does not yank ───────────────────────────────

#[tokio::test]
async fn a_claim_past_the_cap_with_a_live_session_is_escalated_not_moved() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("cap").await;
    let (user, _p) = bed.user(tenant, "owner").await;
    let (b, _todo, in_progress, _r) = board(&bed.db(), tenant).await;
    let live = node_seen(&bed.db(), tenant, 0).await;
    let state = bed.app_state().await;
    let task = task_in(&bed.db(), tenant, b, in_progress, user).await;
    let sess = session(&bed.db(), tenant, live, "running").await;
    // A lease that expired a minute ago, on a worker that still looks fine.
    leased(&bed.db(), task, live, sess, in_progress, user, -60).await;

    assert_eq!(
        claim_reaper::reap_lapsed_claims(&state, 120)
            .await
            .expect("reap"),
        0,
        "the cap is not a kill switch"
    );
    assert_eq!(
        claim_reaper::escalate_capped_claims(&state)
            .await
            .expect("escalate"),
        1
    );

    let c = card(&bed.db(), task).await;
    assert_eq!(c.column, in_progress, "left exactly where it was (AC-4)");
    assert_eq!(c.node, Some(live));
    assert!(c.lease.is_some());
    assert_eq!(labels_of(&bed.db(), task).await, vec!["needs-human-review"]);
    assert!(notifications(&bed.db(), tenant)
        .await
        .iter()
        .any(|(level, _)| level == "warning"));

    // A second replica's pass finds the label already there and stays quiet.
    assert_eq!(
        claim_reaper::escalate_capped_claims(&state)
            .await
            .expect("escalate again"),
        0,
        "escalated exactly once (AC-6)"
    );
    assert_eq!(labels_of(&bed.db(), task).await.len(), 1);

    bed.teardown().await;
}

// ── AC-6: one replica acts, whatever the count ──────────────────────────────

#[tokio::test]
async fn two_reaper_passes_requeue_a_lapsed_card_exactly_once() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("once").await;
    let (user, _p) = bed.user(tenant, "owner").await;
    let (b, todo, in_progress, _r) = board(&bed.db(), tenant).await;
    let live = node_seen(&bed.db(), tenant, 0).await;
    let state = bed.app_state().await;
    let task = task_in(&bed.db(), tenant, b, todo, user).await;
    let dead = session(&bed.db(), tenant, live, "error").await;
    leased(&bed.db(), task, live, dead, in_progress, user, 3600).await;

    assert_eq!(
        claim_reaper::reap_lapsed_claims(&state, 120)
            .await
            .expect("first"),
        1
    );
    assert_eq!(
        claim_reaper::reap_lapsed_claims(&state, 120)
            .await
            .expect("second"),
        0,
        "the cleared lease is what stops the second replica (AC-6)"
    );
    assert_eq!(
        comments_of(&bed.db(), task)
            .await
            .matches("claim lapsed")
            .count(),
        1,
        "one requeue, one comment"
    );

    bed.teardown().await;
}

// ── AC-5: renew is a seam for the holder ────────────────────────────────────

#[tokio::test]
async fn renew_extends_for_the_holder_and_refuses_everyone_else() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("renew").await;
    let (holder, person) = bed.user(tenant, "owner").await;
    let (other, _p2) = bed.user(tenant, "member").await;
    let (b, todo, in_progress, _r) = board(&bed.db(), tenant).await;
    let node = bed.node(tenant, person).await;
    let live = node_seen(&bed.db(), tenant, 0).await;
    let state = state_with_cap(&bed, 7200).await;

    let task = task_in(&bed.db(), tenant, b, todo, holder).await;
    let sess = session(&bed.db(), tenant, live, "running").await;
    leased(&bed.db(), task, node, sess, in_progress, holder, 60).await;
    let before = card(&bed.db(), task).await.lease.expect("leased");

    let renewed = nook_control::services::taskwork::renew_claim(
        &state,
        tenant,
        holder,
        nook_control::auth::Principal::User,
        task,
    )
    .await
    .expect("the holder renews");
    assert!(
        renewed.claim_expires_at.expect("still leased") > before,
        "the cap moves out (AC-5)"
    );

    // The node the work sits on may renew for itself; another node may not.
    nook_control::services::taskwork::renew_claim(
        &state,
        tenant,
        holder,
        nook_control::auth::Principal::Node(node),
        task,
    )
    .await
    .expect("node-self renews");
    let stranger = bed.node(tenant, person).await;
    let err = nook_control::services::taskwork::renew_claim(
        &state,
        tenant,
        holder,
        nook_control::auth::Principal::Node(stranger),
        task,
    )
    .await
    .expect_err("another node cannot");
    assert!(matches!(
        err,
        nook_control::error::ApiError::ForbiddenMsg(_)
    ));

    let err = nook_control::services::taskwork::renew_claim(
        &state,
        tenant,
        other,
        nook_control::auth::Principal::User,
        task,
    )
    .await
    .expect_err("a non-holder cannot");
    assert!(matches!(
        err,
        nook_control::error::ApiError::ForbiddenMsg(_)
    ));

    bed.teardown().await;
}

#[tokio::test]
async fn renew_refuses_an_unleased_or_unclaimed_card() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("renew").await;
    let (user, _p) = bed.user(tenant, "owner").await;
    let (b, todo, in_progress, _r) = board(&bed.db(), tenant).await;
    let state = bed.app_state().await;

    // Claimed, In Progress, but by hand — no lease to renew, and minting one
    // here would hand the reaper a card no agent ever claimed.
    let unleased = task_in(&bed.db(), tenant, b, in_progress, user).await;
    bed.db()
        .exec(
            "UPDATE tasks SET assignee_user_id = $2 WHERE id = $1",
            params![unleased, user],
        )
        .await
        .expect("assign");
    let err = nook_control::services::taskwork::renew_claim(
        &state,
        tenant,
        user,
        nook_control::auth::Principal::User,
        unleased,
    )
    .await
    .expect_err("unleased");
    assert!(matches!(err, nook_control::error::ApiError::Conflict(_)));
    assert!(card(&bed.db(), unleased).await.lease.is_none());

    let unclaimed = task_in(&bed.db(), tenant, b, todo, user).await;
    let err = nook_control::services::taskwork::renew_claim(
        &state,
        tenant,
        user,
        nook_control::auth::Principal::User,
        unclaimed,
    )
    .await
    .expect_err("unclaimed");
    assert!(matches!(err, nook_control::error::ApiError::BadRequest(_)));

    bed.teardown().await;
}

// ── AC-6: the cap comes from configuration ──────────────────────────────────

#[tokio::test]
async fn the_lease_a_claim_mints_comes_from_max_claim_secs() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("cfg").await;
    let (user, _p) = bed.user(tenant, "owner").await;
    let (b, todo, _in_progress, _r) = board(&bed.db(), tenant).await;
    let state = state_with_cap(&bed, 60).await;

    let task = task_in(&bed.db(), tenant, b, todo, user).await;
    nook_control::routes::task_query::claim_inner(
        &state,
        tenant,
        user,
        &task.to_string(),
        Some("started".into()),
    )
    .await
    .expect("claim");

    let lease = card(&bed.db(), task).await.lease.expect("leased");
    let ttl = (lease - chrono::Utc::now()).num_seconds();
    assert!(
        (0..=60).contains(&ttl),
        "a 60s cap gives a 60s lease, not the 4h default: {ttl}s"
    );

    bed.teardown().await;
}
