//! MAIN-530: the command surface on the control plane's own agent surfaces —
//! a chat session and a loop run.
//!
//! The shapes are `nook-chat`'s, and they are the SAME types (`ChatCommand`,
//! `RunChatCommand`, `ChatCommandResult` from `nook-types`) rather than a
//! second pair that happens to serialize alike. This file naming them for both
//! surfaces is that fact asserted at compile time — there is nothing else to
//! import.
//!
//! What the tests carry:
//!
//! - Discovery and execution work on both surfaces, and a caller with no claim
//!   to either is refused exactly as sending to it refuses them (AC-2).
//! - `/status` answers from what is already recorded (NG-6), and a queued run's
//!   answer carries the reason the run view shows — while a running one does
//!   not fabricate a wait it is not in (AC-5).
//! - A chat session's `/status` gets the agent's working state right in both
//!   states (AC-4).
//!
//! Needs Postgres: `DATABASE_URL` (`NOOK_REQUIRE_DB=1` in the suite).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use nook_control::auth::{AuthCtx, Principal};
use nook_control::routes;
use nook_control::services::{jobs, session_chat, session_queries};
use nook_control::state::AppState;
use nook_control::ws::registry::NodeHandle;
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;
use tokio::sync::mpsc;
use uuid::Uuid;

fn user_ctx(user: UserId, tenant: TenantId) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: user,
        tenant_id: tenant,
        principal: Principal::User,
        cookie_session: false,
    }
}

struct Fixture {
    tenant: TenantId,
    user: UserId,
    node: NodeId,
    workspace: WorkspaceId,
    board: BoardId,
    /// The board's prefix — half of what a card's key is made of, the other
    /// half being `tasks.number`.
    board_key: String,
    column: ColumnId,
}

async fn fixture(bed: &TestBed) -> Fixture {
    let tenant = bed.tenant("m530").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let node = bed.node(tenant, person).await;
    let workspace = bed.workspace(tenant).await;
    // A CLONE, because a session created with no explicit path resolves the
    // workspace's clone on that node — a worktree-only row is not a default.
    let checkout = NodeWorkspaceId(Uuid::now_v7());
    bed.db()
        .exec(
            "INSERT INTO node_workspaces (id, tenant_id, node_id, workspace_id, path, kind, git_branch)
             VALUES ($1, $2, $3, $4, $5, 'clone', 'main-530-commands')",
            params![
                checkout,
                tenant,
                node,
                workspace,
                format!("/w/{}", checkout.0.simple())
            ],
        )
        .await
        .expect("checkout");

    let board = BoardId::new();
    let board_key = format!("B{}", &board.0.simple().to_string()[..6]).to_uppercase();
    bed.db()
        .exec(
            "INSERT INTO boards (id, tenant_id, name, key, provider) VALUES ($1,$2,'b',$3,'local')",
            params![board, tenant, board_key.clone()],
        )
        .await
        .expect("board");
    let column = ColumnId::new();
    bed.db()
        .exec(
            "INSERT INTO board_columns (id, board_id, name, position, type)
             VALUES ($1, $2, 'Todo', 0, 'unstarted')",
            params![column, board],
        )
        .await
        .expect("column");

    Fixture {
        tenant,
        user,
        node,
        workspace,
        board,
        board_key,
        column,
    }
}

/// A live channel for the node, so `create_session` and `post_message` reach
/// something instead of recording "that machine is not connected".
fn live_node(state: &AppState, f: &Fixture) -> mpsc::Receiver<nook_proto::ControlToNode> {
    let (tx, rx) = mpsc::channel(16);
    state.registry.register_node(
        f.node,
        NodeHandle {
            tenant_id: f.tenant,
            tx,
        },
    );
    rx
}

async fn chat_session(state: &AppState, f: &Fixture) -> Session {
    session_queries::create_session(
        state,
        f.tenant,
        Some(f.user),
        CreateSessionRequest {
            workspace_id: f.workspace,
            node_id: f.node,
            runtime: "claude".into(),
            name: None,
            path: None,
            interface: SessionInterface::Chat,
        },
    )
    .await
    .expect("chat session")
}

/// A card, and the KEY a person knows it by. `number` is set explicitly because
/// a key is `boards.key || '-' || tasks.number` — a row inserted without one has
/// no key at all, which is the fallback path rather than the one AC-5 is about.
async fn card(bed: &TestBed, f: &Fixture) -> (TaskId, String) {
    let id = TaskId::new();
    bed.db()
        .exec(
            "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, type, created_by, workspace_id, number)
             VALUES ($1,$2,$3,$4,'Add a greeting command','task',$5,$6,530)",
            params![id, f.tenant, f.board, f.column, f.user, f.workspace],
        )
        .await
        .expect("task");
    (id, format!("{}-530", f.board_key))
}

async fn run_session_command(
    state: &AppState,
    ctx: AuthCtx,
    session: SessionId,
    name: &str,
) -> Result<ChatCommandResult, nook_errors::ApiError> {
    routes::sessions::run_command(
        State(state.clone()),
        ctx,
        Path(session),
        Json(RunChatCommand {
            name: name.into(),
            args: None,
        }),
    )
    .await
    .map(|Json(r)| r)
}

async fn run_job_command(
    state: &AppState,
    ctx: AuthCtx,
    job: JobId,
    name: &str,
) -> Result<ChatCommandResult, nook_errors::ApiError> {
    routes::jobs::run_command(
        State(state.clone()),
        ctx,
        Path(job),
        Json(RunChatCommand {
            name: name.into(),
            args: None,
        }),
    )
    .await
    .map(|Json(r)| r)
}

/// The status text, or a panic naming what came back instead — every `/status`
/// assertion below wants the same thing.
fn ephemeral(result: ChatCommandResult) -> String {
    assert!(
        result.posted_message_id.is_none(),
        "NG-4: a command answer is never persisted"
    );
    result.ephemeral.expect("the command answered ephemerally")
}

/// AC-1/AC-2/AC-3 on a chat session: the set comes from the server, `/help`
/// renders it, and a caller from another tenant is refused — on discovery as
/// well as execution, so a stranger is never handed a menu of things they
/// cannot run.
#[tokio::test]
async fn a_session_serves_the_set_and_refuses_a_stranger() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;
    let _rx = live_node(&state, &f);
    let session = chat_session(&state, &f).await;

    let Json(commands) = routes::sessions::commands(
        State(state.clone()),
        user_ctx(f.user, f.tenant),
        Path(session.id),
    )
    .await
    .expect("a member sees the set");
    let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["help", "status"]);
    assert!(
        commands.iter().all(|c| !c.description.is_empty()),
        "every command describes itself: {commands:?}"
    );

    // AC-3: `/help` lists this surface's own commands and says what happens to
    // slash text that is not one of them.
    let help = ephemeral(
        run_session_command(&state, user_ctx(f.user, f.tenant), session.id, "help")
            .await
            .expect("/help runs"),
    );
    for name in ["/help", "/status"] {
        assert!(help.contains(name), "{name} missing from {help}");
    }
    assert!(
        help.contains("sent to the agent exactly as you typed it"),
        "AC-3: the passthrough rule is stated: {help}"
    );

    // AC-2: someone with no claim to the session, on both endpoints.
    let stranger = user_ctx(UserId(Uuid::now_v7()), TenantId(Uuid::now_v7()));
    let listed = routes::sessions::commands(State(state.clone()), stranger, Path(session.id))
        .await
        .expect_err("a stranger cannot list");
    assert_eq!(listed.into_response().status(), StatusCode::FORBIDDEN);
    let ran = run_session_command(
        &state,
        user_ctx(UserId(Uuid::now_v7()), TenantId(Uuid::now_v7())),
        session.id,
        "status",
    )
    .await
    .expect_err("a stranger cannot execute");
    assert_eq!(ran.into_response().status(), StatusCode::FORBIDDEN);

    // AC-7's server half: a name the set does not carry is refused rather than
    // guessed at, which is what makes the client's "not a command, so it is a
    // message" rule safe.
    let unknown = run_session_command(&state, user_ctx(f.user, f.tenant), session.id, "nook-spec")
        .await
        .expect_err("an unknown command is refused");
    assert_eq!(unknown.into_response().status(), StatusCode::BAD_REQUEST);

    bed.teardown().await;
}

/// AC-4: `/status` names where the session runs and what state it is in, and
/// gets the agent's working state right in BOTH states — which is the whole
/// question somebody asks it.
#[tokio::test]
async fn session_status_reports_the_place_and_the_agents_turn() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;
    let mut rx = live_node(&state, &f);
    let session = chat_session(&state, &f).await;
    let _ = rx.try_recv();

    let idle = ephemeral(
        run_session_command(&state, user_ctx(f.user, f.tenant), session.id, "status")
            .await
            .expect("/status runs"),
    );
    let node_name: String = state
        .db
        .query_scalar("SELECT name FROM nodes WHERE id = $1", params![f.node])
        .await
        .expect("node name");
    let workspace_name: String = state
        .db
        .query_scalar(
            "SELECT name FROM workspaces WHERE id = $1",
            params![f.workspace],
        )
        .await
        .expect("workspace name");
    assert!(idle.contains(&node_name), "the node is named: {idle}");
    assert!(
        idle.contains(&workspace_name),
        "the workspace is named: {idle}"
    );
    assert!(
        idle.contains("main-530-commands"),
        "the checkout's branch is named: {idle}"
    );
    assert!(
        idle.contains(&session.status),
        "the session's state is named: {idle}"
    );
    assert!(
        !idle.contains("Agent: working"),
        "a session nobody has spoken to is not working: {idle}"
    );

    // A turn the agent has not answered IS the agent working.
    session_chat::post_message(&state, &session, "add a greeting command")
        .await
        .expect("the message is accepted");
    let working = ephemeral(
        run_session_command(&state, user_ctx(f.user, f.tenant), session.id, "status")
            .await
            .expect("/status runs"),
    );
    assert!(
        working.contains("Agent: working"),
        "mid-turn, /status says so: {working}"
    );

    // …and once it has answered, it is the person's turn again.
    session_chat::message_from_node(
        &state,
        f.tenant,
        f.node,
        session.id,
        "agent",
        "Done — added `greet`.",
    )
    .await
    .expect("the agent's line is recorded");
    let answered = ephemeral(
        run_session_command(&state, user_ctx(f.user, f.tenant), session.id, "status")
            .await
            .expect("/status runs"),
    );
    assert!(
        !answered.contains("Agent: working"),
        "the turn is over: {answered}"
    );

    bed.teardown().await;
}

/// AC-1/AC-2 on a loop run, and AC-5's negative: a RUNNING run's `/status`
/// invents no wait. The reason column can still hold the sentence from before
/// it was placed, and reporting that would describe a gate this run has
/// already passed.
#[tokio::test]
async fn run_status_names_the_run_and_invents_no_wait() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;
    let (target, key) = card(&bed, &f).await;

    let detail = jobs::create(
        &state,
        f.tenant,
        f.user,
        CreateLoopJobRequest {
            kind: "spec".into(),
            target_task_id: target.to_string(),
            seed: None,
        },
    )
    .await
    .expect("job");
    let job = detail.job.id;

    let Json(commands) =
        routes::jobs::commands(State(state.clone()), user_ctx(f.user, f.tenant), Path(job))
            .await
            .expect("a member sees the set");
    assert_eq!(
        commands.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        vec!["help", "status"],
        "the SAME set the session surface offers"
    );

    // Claimed and running on this node, with the pre-placement reason still on
    // the row — exactly the shape that would produce a fabricated wait.
    state
        .db
        .exec(
            "UPDATE loop_jobs
                SET state = 'running', executor_node_id = $2, queued_reason = 'no eligible executor'
              WHERE id = $1",
            params![job, f.node],
        )
        .await
        .expect("place the job");

    let text = ephemeral(
        run_job_command(&state, user_ctx(f.user, f.tenant), job, "status")
            .await
            .expect("/status runs"),
    );
    let node_name: String = state
        .db
        .query_scalar("SELECT name FROM nodes WHERE id = $1", params![f.node])
        .await
        .expect("node name");
    assert!(text.contains("running"), "the job state is named: {text}");
    assert!(text.contains(&node_name), "the node is named: {text}");
    assert!(text.contains(&key), "the card is named by key: {text}");
    assert!(
        !text.contains("no eligible executor"),
        "a running run reports no wait: {text}"
    );

    // AC-2 on this surface too.
    let stranger = user_ctx(UserId(Uuid::now_v7()), TenantId(Uuid::now_v7()));
    routes::jobs::commands(State(state.clone()), stranger, Path(job))
        .await
        .expect_err("a stranger cannot list a run's commands");
    run_job_command(
        &state,
        user_ctx(UserId(Uuid::now_v7()), TenantId(Uuid::now_v7())),
        job,
        "status",
    )
    .await
    .expect_err("a stranger cannot execute them");

    bed.teardown().await;
}

/// AC-5: a QUEUED run's `/status` carries the typed queued reason — the same
/// sentence the run view puts beside the state pill, so the answer to "why is
/// this not moving" is the one already on screen rather than a second wording.
#[tokio::test]
async fn a_queued_run_reports_why_it_is_waiting() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;
    let (target, _key) = card(&bed, &f).await;

    let detail = jobs::create(
        &state,
        f.tenant,
        f.user,
        CreateLoopJobRequest {
            kind: "spec".into(),
            target_task_id: target.to_string(),
            seed: None,
        },
    )
    .await
    .expect("job");
    let job = detail.job.id;
    state
        .db
        .exec(
            "UPDATE loop_jobs SET queued_reason = 'no node carries the label ‘gpu’' WHERE id = $1",
            params![job],
        )
        .await
        .expect("record the gate");

    let text = ephemeral(
        run_job_command(&state, user_ctx(f.user, f.tenant), job, "status")
            .await
            .expect("/status runs"),
    );
    assert!(text.contains("queued"), "the state is named: {text}");
    assert!(
        text.contains("no node carries the label ‘gpu’"),
        "the queued reason is carried through verbatim: {text}"
    );
    assert!(
        text.contains("not placed on a machine yet"),
        "an unplaced run says so rather than naming a machine: {text}"
    );

    bed.teardown().await;
}
