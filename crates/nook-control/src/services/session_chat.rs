//! A chat session's conversation (MAIN-502).
//!
//! Every line of a chat session — what a person said, what the agent said, and
//! every permission it stopped to ask about — is written HERE, in the control
//! plane's database, before it reaches anyone. That placement is the whole of
//! AC-5: the node holds the agent process and nothing else, so a reload, a
//! reconnect, a node restart and a second device all read the same rows rather
//! than each catching a different slice of a stream.
//!
//! The two directions are deliberately asymmetric, for the reason
//! `drive_streaming` spells out: a HUMAN turn is recorded by the end that
//! accepted it (here, on the REST call), never on the runtime's echo of it —
//! recording both is what made a steering message appear twice and read as the
//! agent parroting the person.

use nook_types::*;

use crate::error::{ApiError, ApiResult};
use crate::repo::sessions::NewSessionMessage;
use crate::state::AppState;

/// The whole conversation, oldest first.
pub async fn messages(state: &AppState, session: SessionId) -> ApiResult<Vec<SessionMessage>> {
    state.sessions.messages(session).await
}

/// Say something to a chat session's agent: record it, then deliver it.
///
/// Recorded FIRST, and the record stands whether or not the node is reachable.
/// A message that vanished because the machine was briefly offline would be
/// the one thing a conversation may not do — and the row is also what the
/// sender's own page renders, so the send is confirmed by the server's copy
/// rather than by an optimistic one.
pub async fn post_message(
    state: &AppState,
    session: &Session,
    body: &str,
) -> ApiResult<SessionMessage> {
    require_chat(session)?;
    let body = body.trim();
    if body.is_empty() {
        return Err(ApiError::BadRequest("a message needs some text".into()));
    }
    let msg = state
        .sessions
        .append_message(NewSessionMessage::line(session.id, "human", body))
        .await?;
    let delivered = state.registry.send_to_node(
        session.node_id,
        nook_proto::ControlToNode::ChatMessage {
            session_id: session.id,
            text: body.to_string(),
        },
    );
    if !delivered {
        // Said plainly, in the conversation, rather than swallowed: the person
        // can see their message is in the log and that the agent has not got
        // it, which is the difference between "it is thinking" and "it never
        // heard you".
        state
            .sessions
            .append_message(NewSessionMessage::line(
                session.id,
                "system",
                "that machine is not connected right now — the agent has not received this yet",
            ))
            .await?;
    }
    announce(state, session);
    Ok(msg)
}

/// Answer an outstanding permission request.
///
/// The database decides whether this answer counts: `decide_permission` writes
/// only while `decision IS NULL`, so a second answer — the other device, a
/// double-click, a reload that re-posts — reaches the node no more than once.
/// Two different verdicts for one blocked tool is exactly the race a chat on a
/// phone and a laptop invites.
pub async fn decide_permission(
    state: &AppState,
    session: &Session,
    request_id: &str,
    allow: bool,
    remember: bool,
) -> ApiResult<()> {
    require_chat(session)?;
    if !state
        .sessions
        .decide_permission(session.id, request_id, allow)
        .await?
    {
        return Err(ApiError::Conflict(
            "that permission request has already been answered".into(),
        ));
    }
    state.registry.send_to_node(
        session.node_id,
        nook_proto::ControlToNode::ChatPermissionDecision {
            session_id: session.id,
            request_id: request_id.to_string(),
            allow,
            // Not persisted here, deliberately: "this session" means this agent
            // process, and the node's own set dies with it. A row outliving the
            // process it described would grant a tool in a session nobody
            // decided anything about (MAIN-620 AC-3).
            remember,
        },
    );
    announce(state, session);
    Ok(())
}

/// The only two roles a node may report a line under.
///
/// `role` is a four-value contract the UI switches on, and the other two are
/// not the node's to send: `human` is what a person typed, and `permission` is
/// minted here by [`permission_request`] with the id an answer is addressed to.
/// A node emitting either — through a bug or a compromised token — would put a
/// line in the log attributed to the person, or a permission row with no
/// `permission_request_id`, which renders as an unanswerable prompt with no
/// button that could ever settle it. Anything unrecognised becomes an ordinary
/// agent line: still visible, never mis-attributed (MAIN-502).
fn node_role(reported: &str) -> &str {
    match reported {
        "agent" | "system" => reported,
        _ => "agent",
    }
}

/// A line the node reported — the agent's own words, or its lifecycle notes.
///
/// Gated on the session actually being ON that node, the same anti-spoof rule
/// `jobs::transcript_from_node` applies: a node token must not be able to write
/// into another machine's conversation.
pub async fn message_from_node(
    state: &AppState,
    tenant: TenantId,
    node: NodeId,
    session_id: SessionId,
    role: &str,
    body: &str,
) -> ApiResult<()> {
    let Some(session) = owned_by(state, tenant, node, session_id).await? else {
        return Ok(());
    };
    state
        .sessions
        .append_message(NewSessionMessage::line(session.id, node_role(role), body))
        .await?;
    announce(state, &session);
    Ok(())
}

/// The agent is blocked on a tool and wants an answer (AC-6).
///
/// Written as an ordinary message with the request id on it, because that is
/// what it is: a turn in the conversation that happens to carry two buttons.
/// It is durable for the same reason every other line is — a person who
/// reloads, or who picks up their phone, must find the agent still waiting and
/// still answerable, not a blocked process with nothing on screen about it.
pub async fn permission_from_node(
    state: &AppState,
    tenant: TenantId,
    node: NodeId,
    session_id: SessionId,
    request_id: &str,
    tool_name: &str,
    description: &str,
) -> ApiResult<()> {
    let Some(session) = owned_by(state, tenant, node, session_id).await? else {
        return Ok(());
    };
    state
        .sessions
        .append_message(NewSessionMessage {
            session: session.id,
            role: "permission".into(),
            body: description.to_string(),
            permission_request_id: Some(request_id.to_string()),
            tool_name: Some(tool_name.to_string()),
        })
        .await?;
    announce(state, &session);
    Ok(())
}

/// The session, if it is on this node in this tenant. Anything else is dropped
/// with a warning rather than applied.
async fn owned_by(
    state: &AppState,
    tenant: TenantId,
    node: NodeId,
    session_id: SessionId,
) -> ApiResult<Option<Session>> {
    let session = state.sessions.by_id_unscoped(session_id).await?;
    match session {
        Some(s) if s.node_id == node && s.tenant_id == tenant => Ok(Some(s)),
        _ => {
            tracing::warn!(
                session = %session_id.0, node = %node.0,
                "node reported chat for a session it does not run — dropped"
            );
            Ok(None)
        }
    }
}

/// "What you have is stale" — the nudge every chat surface refetches on.
fn announce(state: &AppState, session: &Session) {
    state.registry.publish(
        session.tenant_id,
        nook_proto::UiEvent::SessionMessage {
            session_id: session.id,
        },
    );
}

/// Refuse the chat endpoints on a terminal session.
///
/// AC-7 in one line: a terminal session has no agent listening on a structured
/// channel, so a message posted to one would be recorded into a conversation
/// nothing will ever read and nothing will ever answer. Better to say so.
fn require_chat(session: &Session) -> ApiResult<()> {
    if session.interface != SessionInterface::Chat {
        return Err(ApiError::BadRequest(
            "that session is a terminal — type into it instead".into(),
        ));
    }
    Ok(())
}
