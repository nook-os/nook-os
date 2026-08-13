//! The per-workspace build-loop switch and its sweep (MAIN-385).
//!
//! What these pin: the switch is OFF until a person turns it on and nothing
//! fires from a workspace that has not been (AC-1/NG-4); an auto-fired run is
//! requested by the ENABLER and not by whoever tripped the trigger (AC-2); the
//! sweep evaluates every enabled workspace of a loops-on tenant and no others
//! (AC-5); each AC-6 event evaluates the workspace at once; the settings
//! endpoint reads and writes all three states of what it documents (AC-8); and
//! a second pass over a covered workspace creates nothing (AC-7).
//!
//! AC-4's placement half lives in `build_loop_pin`, which is a SEPARATE binary
//! because driving `select_executor` means driving `eligible_loop_executors`,
//! and that query does not run on SQLite yet (MAIN-546). Splitting it is what
//! keeps everything here covered by the SQLite leg instead of parking nine
//! tests to excuse two.
//!
//! The pick contract itself — which cards count as available — is
//! `builds_converge`'s and `work_source`'s, deliberately not re-proved here.
//!
//! Engine-neutral (MAIN-264): nothing here names a `sqlx` type.

use nook_control::auth::{AuthCtx, Principal};
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

/// Turn the workspace's build loop on, recording `enabler` as the identity its
/// runs are requested by — the repository write the endpoint performs.
async fn enable(state: &AppState, tenant: TenantId, ws: WorkspaceId, enabler: UserId) {
    state
        .workspaces
        .set_build_loop(tenant, ws, true, None, Some(enabler))
        .await
        .expect("enable")
        .expect("workspace");
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

/// Wait for the spawned evaluation an AC-6 event fires. A nudge is deliberately
/// off the caller's critical path, so a test has to wait for it — bounded, and
/// the assertion is the caller's.
async fn settled_build_jobs(bed: &TestBed, ws: WorkspaceId) -> Vec<(Uuid, Uuid)> {
    for _ in 0..50 {
        let jobs = build_jobs(bed, ws).await;
        if !jobs.is_empty() {
            return jobs;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    Vec::new()
}

/// Put a label in the tenant's vocabulary, so the label endpoint can resolve
/// it by name. `create_task` interns the ones it is handed; a label nobody has
/// used yet has to be created first, exactly as a person would.
async fn define_label(bed: &TestBed, tenant: TenantId, name: &str) {
    bed.db()
        .exec(
            "INSERT INTO labels (id, tenant_id, name, color) VALUES ($1,$2,$3,'#f0a000')",
            params![Uuid::now_v7(), tenant, name],
        )
        .await
        .expect("label");
}

fn user_ctx(user: UserId, tenant: TenantId) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: user,
        tenant_id: tenant,
        principal: Principal::User,
        cookie_session: false,
    }
}

/// AC-1 / NG-4: a workspace nobody enabled fires nothing, however ready the
/// board is and however on the tenant's loops are. This is the whole of "off
/// by default, including on upgrade" — every existing workspace reads exactly
/// like this one.
#[tokio::test]
async fn a_workspace_nobody_enabled_never_fires() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("blsoff").await;
    let (user, _) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let (board, _todo, _doing) = board_fixture(&bed.db(), tenant).await;
    approved_card(&bed.db(), tenant, board, ws, user).await;
    loops_on(&state, tenant).await;

    build_loop::pass(&state, None).await.expect("sweep");
    assert!(
        build_jobs(&bed, ws).await.is_empty(),
        "the tenant's loops are on and the card is ready — the workspace switch is the gate"
    );

    bed.teardown().await;
}

/// AC-2 / AC-5 / AC-7: the sweep fires exactly one run for an enabled
/// workspace, requested by the ENABLER rather than by the card's author — the
/// identity that decides which nodes are candidates at all — and a second pass
/// over the now-covered workspace creates nothing.
#[tokio::test]
async fn the_sweep_fires_one_run_as_the_enabler_and_then_holds() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("blsfire").await;
    let (author, _) = bed.user(tenant, "member").await;
    let (enabler, _) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let (board, _todo, _doing) = board_fixture(&bed.db(), tenant).await;
    let card = approved_card(&bed.db(), tenant, board, ws, author).await;
    loops_on(&state, tenant).await;
    enable(&state, tenant, ws, enabler).await;

    build_loop::pass(&state, None).await.expect("sweep");
    let jobs = build_jobs(&bed, ws).await;
    assert_eq!(jobs.len(), 1, "one available card, one run");
    assert_eq!(
        jobs[0].1, enabler.0,
        "an auto-fired run is requested by whoever enabled the loop — node \
         ownership keys on that person, so this is what makes it placeable"
    );

    // AC-3's board mechanics still apply: the card was claimed as the enabler.
    let row = state
        .tasks
        .get_row(tenant, card)
        .await
        .expect("row")
        .expect("card");
    assert_eq!(row.assignee_user_id, Some(enabler));

    build_loop::pass(&state, None).await.expect("second sweep");
    assert_eq!(
        build_jobs(&bed, ws).await.len(),
        1,
        "AC-7: a sweep over an already-covered workspace creates nothing"
    );

    bed.teardown().await;
}

/// AC-5: the tenant switch gates the sweep like every other loop consumer.
/// Off means no jobs; the work is not lost, it fires when the switch flips.
#[tokio::test]
async fn the_tenant_switch_gates_the_sweep_and_loses_nothing() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("blstenant").await;
    let (user, _) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let (board, _todo, _doing) = board_fixture(&bed.db(), tenant).await;
    approved_card(&bed.db(), tenant, board, ws, user).await;
    enable(&state, tenant, ws, user).await;

    build_loop::pass(&state, None).await.expect("sweep");
    assert!(
        build_jobs(&bed, ws).await.is_empty(),
        "the workspace is enabled but the tenant's loops are off"
    );

    loops_on(&state, tenant).await;
    build_loop::pass(&state, None).await.expect("sweep again");
    assert_eq!(
        build_jobs(&bed, ws).await.len(),
        1,
        "flipping the tenant switch on fires the work that was waiting"
    );

    bed.teardown().await;
}

/// AC-1: enabled with no recorded enabler fires nothing. Only a hand-written
/// UPDATE reaches that row, and a run requested by nobody resolves to no
/// person — so no node is ever eligible and it would queue forever.
#[tokio::test]
async fn an_enabled_workspace_with_no_enabler_fires_nothing() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("blsnoone").await;
    let (user, _) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let (board, _todo, _doing) = board_fixture(&bed.db(), tenant).await;
    approved_card(&bed.db(), tenant, board, ws, user).await;
    loops_on(&state, tenant).await;
    state
        .workspaces
        .set_build_loop(tenant, ws, true, None, None)
        .await
        .expect("enable")
        .expect("workspace");

    build_loop::pass(&state, None).await.expect("sweep");
    assert!(build_jobs(&bed, ws).await.is_empty());

    bed.teardown().await;
}

/// AC-6, event one: `agent-ready` is the human's "go", and it evaluates the
/// workspace immediately rather than at the next sweep. Driven through the
/// label endpoint, so the wiring is what is under test.
#[tokio::test]
async fn labelling_a_card_agent_ready_evaluates_at_once() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("blslabel").await;
    let (author, _) = bed.user(tenant, "member").await;
    let (enabler, _) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let (board, _todo, _doing) = board_fixture(&bed.db(), tenant).await;
    loops_on(&state, tenant).await;
    enable(&state, tenant, ws, enabler).await;
    define_label(&bed, tenant, "agent-ready").await;

    // Created WITHOUT the label, then labelled: the label is the event.
    let provider = LocalBoardProvider {
        repo: std::sync::Arc::new(nook_control::repo::tasks::DbTaskRepository::new(bed.db())),
    };
    let card = provider
        .create_task(
            tenant,
            board,
            Some(author),
            CreateTaskRequest {
                title: "label me".into(),
                description: Some("## AC-1".into()),
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
    assert!(build_jobs(&bed, ws).await.is_empty(), "nothing owed yet");

    let _ = nook_control::routes::labels::add(
        axum::extract::State(state.clone()),
        user_ctx(author, tenant),
        axum::extract::Path((card.id.0.to_string(), "agent-ready".to_string())),
    )
    .await
    .expect("label");

    let jobs = settled_build_jobs(&bed, ws).await;
    assert_eq!(jobs.len(), 1, "the label fired an evaluation");
    assert_eq!(
        jobs[0].1, enabler.0,
        "AC-2: fired as the enabler, not as the person who applied the label"
    );

    bed.teardown().await;
}

/// AC-6, event two: `loop-changes-requested` — MAIN-384's output, the signal
/// that a PR needs a repair pass — is an evaluation occasion too. What the
/// evaluation then finds is `converge_builds`' business; that it happens at
/// all is this card's.
#[tokio::test]
async fn labelling_loop_changes_requested_evaluates_at_once() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("blsrepair").await;
    let (author, _) = bed.user(tenant, "member").await;
    let (enabler, _) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let (board, _todo, _doing) = board_fixture(&bed.db(), tenant).await;
    let card = approved_card(&bed.db(), tenant, board, ws, author).await;
    loops_on(&state, tenant).await;
    enable(&state, tenant, ws, enabler).await;
    define_label(&bed, tenant, "loop-changes-requested").await;

    let _ = nook_control::routes::labels::add(
        axum::extract::State(state.clone()),
        user_ctx(author, tenant),
        axum::extract::Path((card.0.to_string(), "loop-changes-requested".to_string())),
    )
    .await
    .expect("label");

    assert_eq!(
        settled_build_jobs(&bed, ws).await.len(),
        1,
        "the repair label evaluated the workspace"
    );

    bed.teardown().await;
}

/// AC-6, event three: a card arriving in an unstarted column — released,
/// dragged back out of In Progress — is work becoming available, and every
/// mover funnels through `on_column_change`.
#[tokio::test]
async fn a_card_reaching_an_unstarted_column_evaluates_at_once() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("blsmove").await;
    let (author, _) = bed.user(tenant, "member").await;
    let (enabler, _) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let (board, todo, doing) = board_fixture(&bed.db(), tenant).await;
    let card = approved_card(&bed.db(), tenant, board, ws, author).await;
    loops_on(&state, tenant).await;
    enable(&state, tenant, ws, enabler).await;

    nook_control::services::triggers::on_column_change(&state, tenant, card, board, doing, todo)
        .await;

    assert_eq!(
        settled_build_jobs(&bed, ws).await.len(),
        1,
        "reaching the unstarted column evaluated the workspace"
    );

    bed.teardown().await;
}

// ── AC-8: the settings endpoint ──────────────────────────────────────────────

async fn get_settings(
    state: &AppState,
    auth: AuthCtx,
    ws: WorkspaceId,
) -> nook_types::BuildLoopSettings {
    nook_control::routes::workspaces::get_build_loop_settings(
        axum::extract::State(state.clone()),
        auth,
        axum::extract::Path(ws),
    )
    .await
    .expect("get settings")
    .0
}

async fn put_settings(
    state: &AppState,
    auth: AuthCtx,
    ws: WorkspaceId,
    body: serde_json::Value,
) -> nook_types::BuildLoopSettings {
    let req = serde_json::from_value(body).expect("request body");
    nook_control::routes::workspaces::set_build_loop_settings(
        axum::extract::State(state.clone()),
        auth,
        axum::extract::Path(ws),
        axum::Json(req),
    )
    .await
    .expect("put settings")
    .0
}

/// AC-8: the switch is reachable over the API, and turning it on records the
/// CALLER as the identity its runs will be requested by — the endpoint's half
/// of AC-2.
#[tokio::test]
async fn the_settings_endpoint_turns_the_loop_on_and_records_the_caller() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("blsapi").await;
    let (user, _) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    let before = get_settings(&state, user_ctx(user, tenant), ws).await;
    assert!(!before.enabled, "off is what a workspace starts as");
    assert_eq!(before.enabled_by, None);
    assert_eq!(before.concurrency, 1, "unset reads as the default of one");

    let after = put_settings(
        &state,
        user_ctx(user, tenant),
        ws,
        json!({ "enabled": true }),
    )
    .await;
    assert!(after.enabled);
    assert_eq!(after.enabled_by, Some(user));
    assert!(
        get_settings(&state, user_ctx(user, tenant), ws)
            .await
            .enabled,
        "and it is what the next read returns"
    );

    bed.teardown().await;
}

/// AC-8: `null` CLEARS. This is the one the endpoint's documentation, the CLI
/// help and a dark pin's queued reason all promise — `--node none` releases a
/// pin, `--concurrency unset` returns the ceiling to the default — and it is
/// the one a plain `Option` field silently turns into a no-op, because serde
/// hands a JSON null to the option itself.
#[tokio::test]
async fn null_clears_the_pin_and_the_ceiling() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("blsnull").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let node = bed.node(tenant, person).await;
    let state = bed.app_state().await;
    let name: String = bed
        .db()
        .query_scalar("SELECT name FROM nodes WHERE id = $1", params![node])
        .await
        .expect("node name");

    let set = put_settings(
        &state,
        user_ctx(user, tenant),
        ws,
        json!({ "enabled": true, "node": name, "concurrency": 3 }),
    )
    .await;
    assert_eq!(set.node_id, Some(node), "pinned by name");
    assert_eq!(set.node_name.as_deref(), Some(name.as_str()));
    assert_eq!(set.concurrency, 3);

    let cleared = put_settings(
        &state,
        user_ctx(user, tenant),
        ws,
        json!({ "node": null, "concurrency": null }),
    )
    .await;
    assert_eq!(cleared.node_id, None, "null unpins");
    assert_eq!(cleared.node_name, None);
    assert_eq!(cleared.concurrency, 1, "null returns the ceiling to unset");
    assert!(cleared.enabled, "and says nothing about the switch");

    bed.teardown().await;
}

/// AC-8: an ABSENT field leaves that setting alone, which is what makes
/// `{"enabled": false}` a complete request. Absent and null are the two states
/// a caller must be able to tell apart, so this is the other half of the test
/// above.
#[tokio::test]
async fn an_absent_field_changes_nothing_about_that_setting() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("blsabsent").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let node = bed.node(tenant, person).await;
    let state = bed.app_state().await;

    put_settings(
        &state,
        user_ctx(user, tenant),
        ws,
        json!({ "enabled": true, "node": node.0.to_string(), "concurrency": 2 }),
    )
    .await;

    let paused = put_settings(
        &state,
        user_ctx(user, tenant),
        ws,
        json!({ "enabled": false }),
    )
    .await;
    assert!(!paused.enabled);
    assert_eq!(paused.node_id, Some(node), "the pin survives a pause");
    assert_eq!(paused.concurrency, 2, "and so does the ceiling");
    assert_eq!(
        paused.enabled_by,
        Some(user),
        "who enabled it stays answerable after somebody pauses it"
    );

    bed.teardown().await;
}

/// AC-8: a pin naming a node this tenant does not have is refused by name,
/// rather than stored and discovered at placement time.
#[tokio::test]
async fn an_unknown_node_is_refused_rather_than_pinned() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("blsunknown").await;
    let (user, _) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    let req = serde_json::from_value(json!({ "node": "no-such-machine" })).expect("body");
    let refused = nook_control::routes::workspaces::set_build_loop_settings(
        axum::extract::State(state.clone()),
        user_ctx(user, tenant),
        axum::extract::Path(ws),
        axum::Json(req),
    )
    .await;
    assert!(refused.is_err(), "an unknown node is not a pin");
    assert_eq!(
        get_settings(&state, user_ctx(user, tenant), ws)
            .await
            .node_id,
        None,
        "and the refusal stored nothing"
    );

    bed.teardown().await;
}
