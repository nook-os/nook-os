//! MAIN-502: a session created as a CHAT, and everything that must stay true
//! for the terminal beside it.
//!
//! The negatives carry the weight here, and they are the ones the card names:
//!
//! - **A client that says nothing gets a terminal** (AC-1). The field is new,
//!   so every caller written before it — the MCP tool, the CLI, a script —
//!   must be byte-for-byte unaffected. This is checked at the REQUEST level,
//!   not by passing `Terminal` explicitly, because "omitted" is the case that
//!   can regress.
//! - **A runtime that cannot stream cannot be a chat** (AC-2). Refused at
//!   creation, so nothing is written and no node is asked to start something
//!   it would fail.
//! - **A terminal session is unchanged** (AC-7): same row, same `StartSession`,
//!   and the chat endpoints refuse it rather than quietly recording into a
//!   conversation nothing will read.
//!
//! …plus the persistence AC-5 is actually about: the conversation lives in the
//! database, so it survives the node's channel going away and coming back,
//! which is what a reload, a reconnect and a second device all reduce to.
//!
//! Needs Postgres: `DATABASE_URL` (`NOOK_REQUIRE_DB=1` in the suite).

use nook_control::repo::sessions::{NewSession, NewSessionMessage};
use nook_control::services::{session_chat, session_queries};
use nook_control::ws::registry::NodeHandle;
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;
use tokio::sync::mpsc;
use uuid::Uuid;

struct Fixture {
    tenant: TenantId,
    user: UserId,
    node: NodeId,
    workspace: WorkspaceId,
}

async fn fixture(bed: &TestBed) -> Fixture {
    let tenant = bed.tenant("m502").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let node = bed.node(tenant, person).await;
    let workspace = bed.workspace(tenant).await;
    // A checkout for the workspace on that node, so `create_session` can
    // resolve a path rather than refusing before it reaches the interface.
    let checkout = NodeWorkspaceId(Uuid::now_v7());
    bed.db()
        .exec(
            "INSERT INTO node_workspaces (id, tenant_id, node_id, workspace_id, path, kind)
             VALUES ($1, $2, $3, $4, $5, 'clone')",
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
    Fixture {
        tenant,
        user,
        node,
        workspace,
    }
}

/// A live channel for the node, so `send_to_node` succeeds and the test can
/// read what the control plane actually put on the wire.
fn live_node(
    state: &nook_control::state::AppState,
    f: &Fixture,
) -> mpsc::Receiver<nook_proto::ControlToNode> {
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

fn request(f: &Fixture, runtime: &str, interface: SessionInterface) -> CreateSessionRequest {
    CreateSessionRequest {
        workspace_id: f.workspace,
        node_id: f.node,
        runtime: runtime.into(),
        name: None,
        path: None,
        interface,
    }
}

/// AC-1. The field is ABSENT from the JSON, which is the only way to test what
/// an existing client does — a struct literal naming `Terminal` would prove
/// serde's default is reachable, not that omitting it reaches it.
#[test]
fn a_request_that_omits_the_interface_is_a_terminal() {
    let req: CreateSessionRequest = serde_json::from_value(serde_json::json!({
        "workspace_id": Uuid::now_v7(),
        "node_id": Uuid::now_v7(),
        "runtime": "claude",
    }))
    .expect("the pre-MAIN-502 request body still deserializes");
    assert_eq!(
        req.interface,
        SessionInterface::Terminal,
        "an existing client gets exactly today's behaviour"
    );
    // And the stored form round-trips, since the column is plain text.
    assert_eq!(SessionInterface::Terminal.as_str(), "terminal");
    assert_eq!(SessionInterface::Chat.as_str(), "chat");
}

/// AC-2's negative, at the layer that can actually refuse: a runtime with no
/// streaming adapter is not offered chat by the picker, and is refused here if
/// something asks anyway.
#[tokio::test]
async fn a_runtime_that_cannot_stream_is_refused_as_a_chat() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;
    let _rx = live_node(&state, &f);

    let refused = session_queries::create_session(
        &state,
        f.tenant,
        Some(f.user),
        request(&f, "bash", SessionInterface::Chat),
    )
    .await;
    let err = refused.expect_err("bash cannot be a chat");
    let message = format!("{err:?}");
    assert!(
        message.contains("bash") && message.contains("chat"),
        "the refusal names the runtime and what it cannot be: {message}"
    );

    // Nothing was written: a refused create must not leave a session behind for
    // the reconciler or the UI to find.
    let rows: i64 = state
        .db
        .query_scalar(
            "SELECT count(*) FROM sessions WHERE tenant_id = $1",
            params![f.tenant],
        )
        .await
        .expect("count");
    assert_eq!(rows, 0, "the refusal happened before any row existed");

    // …and the SAME runtime is perfectly fine as a terminal (AC-7).
    session_queries::create_session(
        &state,
        f.tenant,
        Some(f.user),
        request(&f, "bash", SessionInterface::Terminal),
    )
    .await
    .expect("bash is a terminal, as it always was");

    bed.teardown().await;
}

/// AC-3 at this end: the node is TOLD which surface to start, on the same
/// message a terminal uses. Getting this wrong is how a chat session opens a
/// tmux nobody can see and a page renders an empty conversation forever.
#[tokio::test]
async fn the_start_instruction_carries_the_interface_both_ways() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;
    let mut rx = live_node(&state, &f);

    let chat = session_queries::create_session(
        &state,
        f.tenant,
        Some(f.user),
        request(&f, "claude", SessionInterface::Chat),
    )
    .await
    .expect("claude can be a chat");
    assert_eq!(chat.interface, SessionInterface::Chat, "stored on the row");

    match rx.try_recv().expect("the node was told to start it") {
        nook_proto::ControlToNode::StartSession { interface, .. } => {
            assert_eq!(interface, SessionInterface::Chat)
        }
        other => panic!("expected StartSession, got {other:?}"),
    }

    // AC-7: the terminal path is untouched, on the wire as well as in the row.
    let term = session_queries::create_session(
        &state,
        f.tenant,
        Some(f.user),
        request(&f, "claude", SessionInterface::Terminal),
    )
    .await
    .expect("claude is still a terminal when asked to be");
    assert_eq!(term.interface, SessionInterface::Terminal);
    match rx.try_recv().expect("the node was told to start it") {
        nook_proto::ControlToNode::StartSession { interface, .. } => {
            assert_eq!(interface, SessionInterface::Terminal)
        }
        other => panic!("expected StartSession, got {other:?}"),
    }

    bed.teardown().await;
}

/// AC-5. The conversation is server-side, so it survives the node's channel
/// disappearing and coming back — which is what a reload, a reconnect and
/// opening the session on a second device all reduce to at this layer.
///
/// The reconnect is simulated by DROPPING the node's channel and registering a
/// fresh one, exactly as a real reconnect does: the control plane keeps no
/// per-connection copy of the conversation, so nothing can be lost with it.
#[tokio::test]
async fn the_conversation_survives_a_reconnect() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;
    let mut rx = live_node(&state, &f);

    let session = session_queries::create_session(
        &state,
        f.tenant,
        Some(f.user),
        request(&f, "claude", SessionInterface::Chat),
    )
    .await
    .expect("chat session");
    let _ = rx.try_recv();

    session_chat::post_message(&state, &session, "add a greeting command")
        .await
        .expect("the message is accepted");
    match rx.try_recv().expect("it reached the node") {
        nook_proto::ControlToNode::ChatMessage { text, .. } => {
            assert_eq!(text, "add a greeting command")
        }
        other => panic!("expected ChatMessage, got {other:?}"),
    }
    session_chat::message_from_node(
        &state,
        f.tenant,
        f.node,
        session.id,
        "agent",
        "Reading the code.",
    )
    .await
    .expect("the agent's line is recorded");

    // The connection goes away entirely — the node restarted, the browser was
    // closed, the phone was in a pocket.
    drop(rx);
    let _rx2 = live_node(&state, &f);

    let after = session_chat::messages(&state, session.id)
        .await
        .expect("the conversation is still there");
    let said: Vec<(&str, &str)> = after
        .iter()
        .map(|m| (m.role.as_str(), m.body.as_str()))
        .collect();
    assert_eq!(
        said,
        vec![
            ("human", "add a greeting command"),
            ("agent", "Reading the code."),
        ],
        "both sides of the exchange survive, in order, with no duplicate of the \
         human turn"
    );

    bed.teardown().await;
}

/// MAIN-502. A node cannot put words in the person's mouth, or mint a
/// permission prompt nothing could answer.
///
/// `role` is a four-value contract the UI switches on, and only two of them are
/// the node's to send. `human` would attribute a machine's line to the person;
/// `permission` would render a prompt with no `permission_request_id`, so its
/// buttons could never settle anything. Both collapse to an ordinary agent
/// line — visible, and honestly attributed.
#[tokio::test]
async fn a_node_may_only_report_agent_or_system_lines() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;
    let mut rx = live_node(&state, &f);

    let session = session_queries::create_session(
        &state,
        f.tenant,
        Some(f.user),
        request(&f, "claude", SessionInterface::Chat),
    )
    .await
    .expect("chat session");
    let _ = rx.try_recv();

    for (reported, body) in [
        ("agent", "reading the code"),
        ("system", "agent session 1234"),
        ("human", "please delete production"),
        ("permission", "Bash: rm -rf /"),
        ("", "no role at all"),
    ] {
        session_chat::message_from_node(&state, f.tenant, f.node, session.id, reported, body)
            .await
            .expect("the line is accepted");
    }

    let roles: Vec<(String, String)> = session_chat::messages(&state, session.id)
        .await
        .expect("the conversation is readable")
        .into_iter()
        .map(|m| (m.role, m.body))
        .collect();
    assert_eq!(
        roles,
        vec![
            ("agent".into(), "reading the code".to_string()),
            ("system".into(), "agent session 1234".to_string()),
            ("agent".into(), "please delete production".to_string()),
            ("agent".into(), "Bash: rm -rf /".to_string()),
            ("agent".into(), "no role at all".to_string()),
        ],
        "the two legitimate roles pass through untouched; everything else is an \
         agent line, so nothing a node says is ever attributed to the person and \
         no unanswerable permission row can be minted from off the machine"
    );

    bed.teardown().await;
}

/// MAIN-502. What `Register`'s reconcile does to a chat session, stated.
///
/// The reconnect test above drops the node's channel and registers a fresh one,
/// but never runs the reconcile that a real `Register` performs — which is how
/// the orphan it implies went unnoticed. A chat has no tmux by design, so it is
/// ALWAYS missing from the node's list and the sweep always ends the row.
///
/// That is the behaviour, not a bug to route around: the node kills its chat
/// agents when the connection ends (`sessions::Manager::kill_live_chats`), so
/// the exit this records is true rather than a row disagreeing with a process
/// still running on the machine. The conversation is the part that survives —
/// it is rows here, never the agent — so the history is still readable and a
/// restart begins a fresh agent, which is NG-2's model anyway.
#[tokio::test]
async fn a_reconnect_ends_the_chat_session_and_keeps_the_conversation() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;
    let mut rx = live_node(&state, &f);

    let session = session_queries::create_session(
        &state,
        f.tenant,
        Some(f.user),
        request(&f, "claude", SessionInterface::Chat),
    )
    .await
    .expect("chat session");
    let _ = rx.try_recv();

    session_chat::message_from_node(&state, f.tenant, f.node, session.id, "agent", "On it.")
        .await
        .expect("the agent's line is recorded");

    // The node reconnects and lists the tmux sessions it really has. A chat is
    // not among them and never can be — that is what makes it a chat.
    state
        .nodes
        .expire_sessions_missing_from_tmux(f.node, &[])
        .await
        .expect("the reconcile runs");

    let after = state
        .sessions
        .by_id_unscoped(session.id)
        .await
        .expect("the row is readable")
        .expect("the row is still there");
    assert_eq!(
        after.status, "exited",
        "a chat session does not survive its connection — and the node killed \
         the agent to match, so this row is the truth rather than a process \
         still running unreachably on the machine"
    );

    let said: Vec<String> = session_chat::messages(&state, session.id)
        .await
        .expect("the conversation is readable")
        .into_iter()
        .map(|m| m.body)
        .collect();
    assert!(
        said.contains(&"On it.".to_string()),
        "the conversation outlives the agent — it is rows, not process state, \
         which is the whole of AC-5; got {said:?}"
    );

    bed.teardown().await;
}

/// AC-6. A permission request is a durable row with no decision on it, and the
/// answer is written exactly once.
///
/// The "agent blocks" half is the runtime's own contract — it stops reading
/// until a `control_response` arrives, which `job_adapter`'s tests pin on the
/// wire. What THIS end must guarantee is that the request survives long enough
/// to be answered, and that a second answer never reaches the node: two
/// verdicts for one blocked tool is precisely what a chat open on a phone and
/// a laptop invites.
#[tokio::test]
async fn a_permission_waits_to_be_answered_and_is_answered_once() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;
    let mut rx = live_node(&state, &f);

    let session = session_queries::create_session(
        &state,
        f.tenant,
        Some(f.user),
        request(&f, "claude", SessionInterface::Chat),
    )
    .await
    .expect("chat session");
    let _ = rx.try_recv();

    session_chat::permission_from_node(
        &state,
        f.tenant,
        f.node,
        session.id,
        "req-1",
        "Bash",
        "rm -rf build/",
    )
    .await
    .expect("the request is recorded");

    let pending = session_chat::messages(&state, session.id)
        .await
        .expect("messages");
    let ask = pending
        .iter()
        .find(|m| m.role == "permission")
        .expect("the request is in the conversation, not off to one side");
    assert_eq!(ask.permission_request_id.as_deref(), Some("req-1"));
    assert_eq!(ask.tool_name.as_deref(), Some("Bash"));
    assert_eq!(ask.body, "rm -rf build/");
    assert!(
        ask.decision.is_none(),
        "outstanding — this is what puts the buttons on screen"
    );

    session_chat::decide_permission(&state, &session, "req-1", false)
        .await
        .expect("the first answer lands");
    match rx.try_recv().expect("the verdict reached the node") {
        nook_proto::ControlToNode::ChatPermissionDecision {
            request_id, allow, ..
        } => {
            assert_eq!(request_id, "req-1");
            assert!(!allow, "denied");
        }
        other => panic!("expected ChatPermissionDecision, got {other:?}"),
    }

    // The second device clicks Allow a moment later. It must change nothing —
    // and above all must not send a contradicting verdict to an agent that has
    // already been refused and moved on.
    let second = session_chat::decide_permission(&state, &session, "req-1", true).await;
    assert!(second.is_err(), "a settled request cannot be re-answered");
    assert!(
        rx.try_recv().is_err(),
        "nothing further was sent to the node"
    );

    let settled = session_chat::messages(&state, session.id)
        .await
        .expect("messages");
    let ask = settled
        .iter()
        .find(|m| m.role == "permission")
        .expect("still there");
    assert_eq!(
        ask.decision.as_deref(),
        Some("deny"),
        "the FIRST answer stands, and scrolling back says what was decided"
    );

    bed.teardown().await;
}

/// AC-7, the other half: the chat endpoints refuse a terminal session outright.
///
/// Recording into a terminal's conversation would be worse than an error —
/// nothing on that machine is listening on a structured channel, so the message
/// would sit in a log no agent will ever read and no reply will ever come.
#[tokio::test]
async fn a_terminal_session_refuses_the_chat_endpoints() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;
    let _rx = live_node(&state, &f);

    let term = session_queries::create_session(
        &state,
        f.tenant,
        Some(f.user),
        request(&f, "claude", SessionInterface::Terminal),
    )
    .await
    .expect("terminal session");

    assert!(
        session_chat::post_message(&state, &term, "hello")
            .await
            .is_err(),
        "a terminal is typed into, not messaged"
    );
    assert!(
        session_chat::decide_permission(&state, &term, "req-1", true)
            .await
            .is_err(),
        "a terminal has no permission exchange to answer"
    );
    assert!(
        session_chat::messages(&state, term.id)
            .await
            .expect("reading is harmless")
            .is_empty(),
        "and nothing was written by the refusals"
    );

    bed.teardown().await;
}

/// A node may only write into a conversation for a session it is actually
/// running — the same anti-spoof rule the loop's transcript has.
#[tokio::test]
async fn a_node_cannot_write_into_another_machines_conversation() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;
    let _rx = live_node(&state, &f);

    let session = state
        .sessions
        .create(NewSession {
            tenant: f.tenant,
            workspace_id: Some(f.workspace),
            node_id: f.node,
            name: "chat".into(),
            runtime: "claude".into(),
            created_by: Some(f.user),
            checkout_id: None,
            managed: false,
            managed_purpose: ManagedPurpose::Access,
            managed_shard: 0,
            managed_shards: 1,
            interface: SessionInterface::Chat,
        })
        .await
        .expect("session");

    // The second argument is the ROLE (`owner` | `admin` | `member`), not a
    // name — a plain member is the right shape for "somebody else's machine".
    let (_other_user, other_person) = bed.user(f.tenant, "member").await;
    let other_node = bed.node(f.tenant, other_person).await;

    session_chat::message_from_node(
        &state,
        f.tenant,
        other_node,
        session.id,
        "agent",
        "I am not this session's machine",
    )
    .await
    .expect("dropped, never fatal to the connection");

    assert!(
        session_chat::messages(&state, session.id)
            .await
            .expect("messages")
            .is_empty(),
        "a node token cannot inject into a session it does not run"
    );

    // …and the session's OWN node is recorded normally, so the guard is a guard
    // and not a wall.
    state
        .sessions
        .append_message(NewSessionMessage::line(session.id, "system", "started"))
        .await
        .expect("append");
    assert_eq!(
        session_chat::messages(&state, session.id)
            .await
            .expect("messages")
            .len(),
        1
    );

    bed.teardown().await;
}
