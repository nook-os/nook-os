//! AC-4's placement half: the build loop's node PIN (MAIN-385).
//!
//! Its own binary, and the reason is the SQLite leg. Both tests here drive
//! `jobs::select_executor`, which reads `eligible_loop_executors` — the query
//! that reads `json_each(…) e`'s fields off the alias and fails on SQLite with
//! `no such column: e` (MAIN-546, and the reason `executor_selection`,
//! `dispatch_order` and `workspace_build_loop_status` are already excluded).
//! Leaving these beside the rest of MAIN-385's tests would have excluded that
//! whole binary; here, one line in `scripts/sqlite-ci-allowlist.txt` excuses
//! exactly the two tests that cannot pass yet, and MAIN-546 deletes it with
//! the other three.
//!
//! Engine-neutral otherwise (MAIN-264): nothing here names a `sqlx` type.

use nook_control::services::build_loop;
use nook_control::services::kanban::{KanbanProvider, LocalBoardProvider};
use nook_control::state::AppState;
use nook_db::{params, Db, DbPool};
use nook_testkit::TestBed;
use nook_types::*;
use serde_json::json;
use uuid::Uuid;

/// A board with the columns a card is picked from and claimed into.
async fn board_fixture(db: &DbPool, tenant: TenantId) -> (BoardId, ColumnId, ColumnId) {
    let board = BoardId(Uuid::now_v7());
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
    let mut cols = Vec::new();
    for (i, (name, ty)) in [("Todo", "unstarted"), ("Doing", "started")]
        .iter()
        .enumerate()
    {
        let id = ColumnId(Uuid::now_v7());
        db.exec(
            "INSERT INTO board_columns (id, board_id, name, position, type)
             VALUES ($1, $2, $3, $4, $5)",
            params![id, board, *name, i as i32, *ty],
        )
        .await
        .expect("column");
        cols.push(id);
    }
    (board, cols[0], cols[1])
}

/// An `agent-ready`, unassigned card on the board's unstarted column: the one
/// shape the fresh pick reads.
async fn approved_card(
    db: &DbPool,
    tenant: TenantId,
    board: BoardId,
    ws: WorkspaceId,
    author: UserId,
) -> TaskId {
    let provider = LocalBoardProvider {
        repo: std::sync::Arc::new(nook_control::repo::tasks::DbTaskRepository::new(db.clone())),
    };
    provider
        .create_task(
            tenant,
            board,
            Some(author),
            CreateTaskRequest {
                title: "build me".into(),
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
        .id
}

async fn loops_on(state: &AppState, tenant: TenantId) {
    nook_control::services::loops::set(&*state.settings, tenant, true)
        .await
        .expect("loops on");
}

async fn build_jobs(bed: &TestBed, ws: WorkspaceId) -> Vec<(Uuid, Uuid)> {
    bed.db()
        .query_all::<(Uuid, Uuid)>(
            "SELECT id, requested_by FROM loop_jobs WHERE kind = 'build' AND workspace_id = $1",
            params![ws],
        )
        .await
        .expect("jobs")
}

/// A node that may take build work: the kind declared, the runtime authorized,
/// and the `role=build` label the build wall requires (MAIN-383).
async fn build_node(bed: &TestBed, tenant: TenantId, owner: Uuid, status: &str) -> NodeId {
    let id = NodeId::new();
    bed.db()
        .exec(
            "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status, owner_person_id,
                                capabilities, labels)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8)",
            params![
                id,
                tenant,
                format!("n-{}", id.0.simple()),
                format!("h-{}", id.0.simple()),
                status,
                owner,
                json!({
                    "loop_kinds": ["build"],
                    "runtime_auth": [
                        { "id": "claude", "label": "Claude Code",
                          "runtime": "claude", "state": "authorized" }
                    ]
                }),
                json!({ "role": "build" })
            ],
        )
        .await
        .expect("node");
    id
}

async fn node_name(bed: &TestBed, id: NodeId) -> String {
    bed.db()
        .query_scalar("SELECT name FROM nodes WHERE id = $1", params![id])
        .await
        .expect("node name")
}

/// AC-4: the pin is the only candidate. An idle, eligible alternative exists
/// and is not used — placement is where the enabler said, or nowhere.
#[tokio::test]
async fn a_pinned_node_is_the_only_candidate() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("blspin").await;
    let (enabler, person) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let (board, _todo, _doing) = board_fixture(&bed.db(), tenant).await;
    approved_card(&bed.db(), tenant, board, ws, enabler).await;
    loops_on(&state, tenant).await;

    let pin = build_node(&bed, tenant, person, "online").await;
    let _other = build_node(&bed, tenant, person, "online").await;
    state
        .workspaces
        .set_build_loop(tenant, ws, true, Some(pin), Some(enabler))
        .await
        .expect("enable")
        .expect("workspace");

    build_loop::pass(&state, None).await.expect("sweep");
    let jobs = build_jobs(&bed, ws).await;
    assert_eq!(jobs.len(), 1);
    let placed = nook_control::services::jobs::select_executor(&state, tenant, JobId(jobs[0].0))
        .await
        .expect("select");
    assert_eq!(placed.state, "claimed");
    assert_eq!(
        placed.executor_node_id,
        Some(pin),
        "the pinned node, not the other perfectly good one"
    );

    bed.teardown().await;
}

/// AC-4: a pin that is offline or ineligible WAITS. It never fails over — the
/// job stays queued, and the reason names the node and the way out.
#[tokio::test]
async fn a_pinned_node_that_is_dark_leaves_the_job_queued_with_a_reason() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("blsdark").await;
    let (enabler, person) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let (board, _todo, _doing) = board_fixture(&bed.db(), tenant).await;
    approved_card(&bed.db(), tenant, board, ws, enabler).await;
    loops_on(&state, tenant).await;

    let dark = build_node(&bed, tenant, person, "offline").await;
    // A perfectly good alternative that must NOT be used.
    let _alive = build_node(&bed, tenant, person, "online").await;
    state
        .workspaces
        .set_build_loop(tenant, ws, true, Some(dark), Some(enabler))
        .await
        .expect("enable")
        .expect("workspace");

    build_loop::pass(&state, None).await.expect("sweep");
    let jobs = build_jobs(&bed, ws).await;
    assert_eq!(jobs.len(), 1);
    let held = nook_control::services::jobs::select_executor(&state, tenant, JobId(jobs[0].0))
        .await
        .expect("select");
    assert_eq!(held.state, "queued", "it waits rather than moving machine");
    assert_eq!(held.executor_node_id, None);
    assert_eq!(
        held.queued_reason_kind,
        Some(QueuedReason::PinnedNodeUnavailable {
            node_name: node_name(&bed, dark).await
        }),
        "the typed gate carries the node, so a client need not parse the sentence"
    );
    let reason = held.queued_reason.unwrap_or_default();
    assert!(
        reason.contains("pinned build-loop node"),
        "the reason names WHICH pin is holding it: {reason}"
    );
    assert!(
        reason.contains("unpin"),
        "and the way out, so a dark pin is not a dead end: {reason}"
    );

    bed.teardown().await;
}

/// The pin is the LOOP's statement about where its runs go, so it is read only
/// while the loop is on. A repo somebody paused keeps its pin — resuming must
/// not lose it — but a stale pin on a since-retired machine must not quietly
/// confine the builds a person enqueues by hand, whose only way out would be
/// the unpin they have no reason to think is needed.
#[tokio::test]
async fn a_pin_on_a_switched_off_loop_confines_nothing() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("blsoffpin").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let (board, _todo, _doing) = board_fixture(&bed.db(), tenant).await;
    approved_card(&bed.db(), tenant, board, ws, user).await;
    loops_on(&state, tenant).await;

    let dark = build_node(&bed, tenant, person, "offline").await;
    let alive = build_node(&bed, tenant, person, "online").await;
    // Enabled with the pin, so the loop raises a run for the card...
    state
        .workspaces
        .set_build_loop(tenant, ws, true, Some(dark), Some(user))
        .await
        .expect("enable")
        .expect("workspace");
    build_loop::pass(&state, None).await.expect("sweep");
    let jobs = build_jobs(&bed, ws).await;
    assert_eq!(jobs.len(), 1);

    // ...and then switched off, the pin left behind exactly as a pause leaves it.
    state
        .workspaces
        .set_build_loop(tenant, ws, false, Some(dark), Some(user))
        .await
        .expect("disable")
        .expect("workspace");

    let placed = nook_control::services::jobs::select_executor(&state, tenant, JobId(jobs[0].0))
        .await
        .expect("select");
    assert_eq!(
        placed.executor_node_id,
        Some(alive),
        "a stale pin on a paused loop does not hold the job on a dark machine"
    );

    bed.teardown().await;
}
