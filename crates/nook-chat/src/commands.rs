//! The chat command surface (MAIN-528): discovery, execution, and the three
//! commands that exist.
//!
//! Every command — `/help` included — executes HERE, server-side, and answers in
//! one shape. That is the whole contract: a client posts a name and some
//! argument text and renders what comes back, so no browser ever learns what a
//! command means, and a command added here reaches every client without one of
//! them shipping.

use axum::extract::{Path, State};
use axum::Json;
use nook_types::{ChatCommand, ChatCommandResult, RunChatCommand};
use uuid::Uuid;

use crate::repo::messages::NewMessage;
use crate::{AppState, Caller};
use nook_errors::ApiError;

/// The `/me` marker on a posted message. The only non-null `chat_messages.kind`
/// today (MAIN-528 AC-8).
pub(crate) const ACTION_KIND: &str = "action";

const SHRUG: &str = r"¯\_(ツ)_/¯";

/// One command as the server defines it. `ChatCommand` is the wire shape; this
/// is that plus the thing a client never gets — how to run it.
struct Spec {
    name: &'static str,
    args_hint: Option<&'static str>,
    description: &'static str,
}

/// THE command set (AC-1). Nothing else defines it: discovery returns this,
/// `/help` renders this, and execution dispatches on it — so a command cannot
/// exist in one of the three and not the others.
const COMMANDS: &[Spec] = &[
    Spec {
        name: "help",
        args_hint: None,
        description: "List the commands you can use here.",
    },
    Spec {
        name: "me",
        args_hint: Some("<text>"),
        description: "Post what you are doing as an action.",
    },
    Spec {
        name: "shrug",
        args_hint: Some("[text]"),
        description: "Post your text with a shrug on the end.",
    },
];

impl From<&Spec> for ChatCommand {
    fn from(s: &Spec) -> Self {
        ChatCommand {
            name: s.name.to_string(),
            args_hint: s.args_hint.map(str::to_string),
            description: s.description.to_string(),
        }
    }
}

fn catalog() -> Vec<ChatCommand> {
    COMMANDS.iter().map(ChatCommand::from).collect()
}

/// `/help`'s body, and the ONLY rendering of the set that exists — the client
/// composes nothing (AC-5).
fn help_text() -> String {
    let mut out = String::from("Commands you can use here:");
    for c in COMMANDS {
        out.push_str("\n/");
        out.push_str(c.name);
        if let Some(hint) = c.args_hint {
            out.push(' ');
            out.push_str(hint);
        }
        out.push_str(" — ");
        out.push_str(c.description);
    }
    out
}

/// The commands available to the caller in this channel (AC-1). Gated on the
/// posting rule, like execution — a caller who cannot run anything here is told
/// so by the same refusal rather than handed a menu.
pub async fn list(
    State(state): State<AppState>,
    caller: Caller,
    Path(channel_id): Path<Uuid>,
) -> Result<Json<Vec<ChatCommand>>, ApiError> {
    crate::channels::require_postable(&*state.channels, channel_id, &caller).await?;
    Ok(Json(catalog()))
}

/// Execute a command as the caller (AC-2). Authorization is the posting rule and
/// nothing else, so a non-member is refused exactly as a post is.
pub async fn run(
    State(state): State<AppState>,
    caller: Caller,
    Path(channel_id): Path<Uuid>,
    Json(req): Json<RunChatCommand>,
) -> Result<Json<ChatCommandResult>, ApiError> {
    crate::channels::require_postable(&*state.channels, channel_id, &caller).await?;

    // A leading slash is accepted because that is what a person types; the name
    // on the wire is the bare word either way.
    let name = req.name.trim().trim_start_matches('/');
    let args = req.args.as_deref().unwrap_or("").trim();

    if !COMMANDS.iter().any(|c| c.name == name) {
        // AC-4: the refusal names what was asked for and where to look, and no
        // arm below has run, so nothing changed.
        return Err(ApiError::BadRequest(format!(
            "Unknown command /{name} — try /help"
        )));
    }

    match name {
        "help" => Ok(Json(ChatCommandResult {
            ephemeral: Some(help_text()),
            ..Default::default()
        })),
        "me" => {
            if args.is_empty() {
                return Ok(Json(ChatCommandResult {
                    ephemeral: Some("/me needs something to say — try /me waves.".into()),
                    ..Default::default()
                }));
            }
            post(
                &state,
                channel_id,
                &caller,
                args.to_string(),
                Some(ACTION_KIND),
            )
            .await
        }
        // No text is still a shrug — the argument is optional, which is what
        // `[text]` says.
        "shrug" => {
            let body = format!("{args} {SHRUG}");
            post(
                &state,
                channel_id,
                &caller,
                body.trim_start().to_string(),
                None,
            )
            .await
        }
        // Unreachable while every entry in COMMANDS has an arm above — and a
        // future one that forgets its arm gets this refusal rather than the
        // panic an `unreachable!` would give it.
        _ => Err(ApiError::BadRequest(format!(
            "Unknown command /{name} — try /help"
        ))),
    }
}

/// Post through the ordinary path — the same write, the same live delivery, the
/// same bus announcement a typed message gets (AC-3/AC-8).
async fn post(
    state: &AppState,
    channel_id: Uuid,
    caller: &Caller,
    body: String,
    kind: Option<&str>,
) -> Result<Json<ChatCommandResult>, ApiError> {
    let msg = crate::messages::deliver(
        state,
        NewMessage {
            channel_id,
            author_id: caller.user_id,
            tenant_id: caller.tenant_id,
            body,
            parent_message_id: None,
            kind: kind.map(str::to_string),
        },
    )
    .await?;
    Ok(Json(ChatCommandResult {
        posted_message_id: Some(msg.id),
        ..Default::default()
    }))
}

#[cfg(test)]
mod tests {
    use axum::extract::Query;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::Json;
    use nook_db::{params, Db};
    use nook_types::ChatServerMessage;

    use super::*;
    use crate::messages::{history, HistoryQuery};

    async fn state() -> Option<crate::testdb::ChatTest> {
        crate::testdb::chat_test("the chat command tests").await
    }

    fn caller(tenant: Uuid) -> Caller {
        Caller {
            user_id: Uuid::now_v7(),
            tenant_id: tenant,
            cookie_session: true,
        }
    }

    /// A channel row written directly — these tests run on a `chat`-only pool
    /// with no seeded users, exactly as the message tests do.
    async fn make_channel(state: &AppState, tenant: Uuid) -> Uuid {
        let id = Uuid::now_v7();
        state
            .db
            .exec(
                "INSERT INTO chat_channels (id, owner_type, owner_id, name, slug)
                 VALUES ($1, 'tenant', $2, 'general', 'general')",
                params![id, tenant],
            )
            .await
            .unwrap();
        id
    }

    async fn message_count(state: &AppState, channel: Uuid) -> i64 {
        state
            .db
            .query_scalar(
                "SELECT count(*) FROM chat_messages WHERE channel_id = $1",
                params![channel],
            )
            .await
            .unwrap()
    }

    async fn run_command(
        state: &crate::testdb::ChatTest,
        who: &Caller,
        channel: Uuid,
        name: &str,
        args: Option<&str>,
    ) -> Result<ChatCommandResult, ApiError> {
        run(
            State((*state).clone()),
            Caller {
                user_id: who.user_id,
                tenant_id: who.tenant_id,
                cookie_session: who.cookie_session,
            },
            Path(channel),
            Json(RunChatCommand {
                name: name.into(),
                args: args.map(str::to_string),
            }),
        )
        .await
        .map(|Json(r)| r)
    }

    /// AC-1: the set comes from the server, and discovery is gated exactly as
    /// posting is — a caller from another tenant gets the posting refusal, not a
    /// menu of things they cannot run.
    #[tokio::test]
    async fn discovery_lists_the_set_and_refuses_a_stranger() {
        let Some(state) = state().await else { return };
        let tenant = Uuid::now_v7();
        let channel = make_channel(&state, tenant).await;

        let Json(commands) = list(State(state.clone()), caller(tenant), Path(channel))
            .await
            .expect("a member sees the set");
        let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["help", "me", "shrug"]);
        assert!(
            commands.iter().all(|c| !c.description.is_empty()),
            "every command describes itself: {commands:?}"
        );
        assert_eq!(commands[1].args_hint.as_deref(), Some("<text>"));

        let refused = list(State(state.clone()), caller(Uuid::now_v7()), Path(channel))
            .await
            .expect_err("a non-member is refused");
        assert_eq!(refused.into_response().status(), StatusCode::FORBIDDEN);

        state.teardown().await;
    }

    /// AC-5: `/help` is a server command like any other — it renders the same
    /// set discovery returns, and writes nothing.
    #[tokio::test]
    async fn help_answers_ephemerally_and_posts_nothing() {
        let Some(state) = state().await else { return };
        let tenant = Uuid::now_v7();
        let channel = make_channel(&state, tenant).await;
        let me = caller(tenant);

        let result = run_command(&state, &me, channel, "help", None)
            .await
            .expect("/help runs");
        let text = result.ephemeral.expect("/help answers ephemerally");
        for name in ["/help", "/me <text>", "/shrug [text]"] {
            assert!(text.contains(name), "{name} missing from {text}");
        }
        assert!(result.posted_message_id.is_none(), "/help posts nothing");
        assert_eq!(message_count(&state, channel).await, 0);

        state.teardown().await;
    }

    /// AC-6: the suffix is appended on the SERVER, and what lands is an ordinary
    /// message — no kind.
    #[tokio::test]
    async fn shrug_posts_an_ordinary_message_with_the_suffix() {
        let Some(state) = state().await else { return };
        let tenant = Uuid::now_v7();
        let channel = make_channel(&state, tenant).await;
        let me = caller(tenant);

        let result = run_command(&state, &me, channel, "shrug", Some("ok fine"))
            .await
            .expect("/shrug runs");
        let id = result.posted_message_id.expect("/shrug posts");
        assert!(result.ephemeral.is_none());

        let (body, kind): (String, Option<String>) = state
            .db
            .query_one(
                "SELECT body, kind FROM chat_messages WHERE id = $1",
                params![id],
            )
            .await
            .unwrap();
        assert_eq!(body, r"ok fine ¯\_(ツ)_/¯");
        assert!(kind.is_none(), "a shrug is an ordinary message");

        // The text is optional: a bare shrug is still a shrug.
        let bare = run_command(&state, &me, channel, "shrug", None)
            .await
            .expect("/shrug with no text runs");
        let bare_body: String = state
            .db
            .query_scalar(
                "SELECT body FROM chat_messages WHERE id = $1",
                params![bare.posted_message_id.unwrap()],
            )
            .await
            .unwrap();
        assert_eq!(bare_body, r"¯\_(ツ)_/¯");

        state.teardown().await;
    }

    /// AC-8: `/me` stores `kind='action'` and rides the live fan-out exactly as
    /// an ordinary message does — a second member's socket taps the same
    /// firehose, so a frame here is a frame there.
    #[tokio::test]
    async fn me_posts_an_action_and_is_delivered_live() {
        let Some(state) = state().await else { return };
        let tenant = Uuid::now_v7();
        let channel = make_channel(&state, tenant).await;
        let me = caller(tenant);

        // The other member's stream, opened before the command runs.
        let mut watcher = state.registry.subscribe_all();

        let result = run_command(&state, &me, channel, "me", Some("  deploys the thing  "))
            .await
            .expect("/me runs");
        let id = result.posted_message_id.expect("/me posts");

        let (body, kind): (String, Option<String>) = state
            .db
            .query_one(
                "SELECT body, kind FROM chat_messages WHERE id = $1",
                params![id],
            )
            .await
            .unwrap();
        assert_eq!(body, "deploys the thing");
        assert_eq!(kind.as_deref(), Some(ACTION_KIND));

        let frame = watcher.try_recv().expect("one frame delivered live");
        let ChatServerMessage::Message(live) = frame else {
            panic!("expected a Message frame, got {frame:?}");
        };
        assert_eq!(live.id, id);
        assert_eq!(
            live.kind.as_deref(),
            Some(ACTION_KIND),
            "the kind rides the websocket payload"
        );

        state.teardown().await;
    }

    /// AC-8: `/me` with nothing to say refuses to the caller alone and writes no
    /// row — an empty action is not a message.
    #[tokio::test]
    async fn me_with_no_args_refuses_ephemerally_and_posts_nothing() {
        let Some(state) = state().await else { return };
        let tenant = Uuid::now_v7();
        let channel = make_channel(&state, tenant).await;
        let me = caller(tenant);

        for args in [None, Some("   ")] {
            let result = run_command(&state, &me, channel, "me", args)
                .await
                .expect("an empty /me is answered, not an error");
            assert!(result.ephemeral.is_some(), "the caller is told why");
            assert!(result.posted_message_id.is_none());
        }
        assert_eq!(message_count(&state, channel).await, 0);

        state.teardown().await;
    }

    /// AC-4: an unknown name is a 400 carrying the documented sentence in the
    /// standard error body, and nothing is written.
    #[tokio::test]
    async fn an_unknown_command_is_a_400_that_changes_nothing() {
        let Some(state) = state().await else { return };
        let tenant = Uuid::now_v7();
        let channel = make_channel(&state, tenant).await;
        let me = caller(tenant);

        let err = run_command(&state, &me, channel, "foo", Some("whatever"))
            .await
            .expect_err("an unknown command is refused");
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        assert_eq!(
            String::from_utf8(body.to_vec()).unwrap(),
            r#"{"error":"Unknown command /foo — try /help"}"#
        );
        assert_eq!(message_count(&state, channel).await, 0);

        state.teardown().await;
    }

    /// AC-2: execution is gated on the posting rule, so a caller with no claim
    /// to the channel is refused before any command runs.
    #[tokio::test]
    async fn a_stranger_cannot_execute() {
        let Some(state) = state().await else { return };
        let tenant = Uuid::now_v7();
        let channel = make_channel(&state, tenant).await;
        let stranger = caller(Uuid::now_v7());

        let err = run_command(&state, &stranger, channel, "shrug", Some("hi"))
            .await
            .expect_err("a non-member is refused");
        assert_eq!(err.into_response().status(), StatusCode::FORBIDDEN);
        assert_eq!(message_count(&state, channel).await, 0);

        state.teardown().await;
    }

    /// AC-9: the kind a client saw live is the kind it sees after a reload —
    /// through history and across a keyset page boundary.
    #[tokio::test]
    async fn kind_survives_history_and_its_keyset_pages() {
        let Some(state) = state().await else { return };
        let tenant = Uuid::now_v7();
        let channel = make_channel(&state, tenant).await;
        let me = caller(tenant);

        let action = run_command(&state, &me, channel, "me", Some("waves"))
            .await
            .unwrap()
            .posted_message_id
            .unwrap();
        let shrug = run_command(&state, &me, channel, "shrug", Some("ok"))
            .await
            .unwrap()
            .posted_message_id
            .unwrap();

        // Page one is the newest message; the action is behind the cursor.
        let Json(page1) = history(
            State(state.clone()),
            caller(tenant),
            Path(channel),
            Query(HistoryQuery {
                before: None,
                limit: Some(1),
            }),
        )
        .await
        .unwrap();
        assert_eq!(page1.messages[0].id, shrug);
        assert!(page1.messages[0].kind.is_none());

        let Json(page2) = history(
            State(state.clone()),
            caller(tenant),
            Path(channel),
            Query(HistoryQuery {
                before: page1.next_cursor,
                limit: Some(1),
            }),
        )
        .await
        .unwrap();
        assert_eq!(page2.messages[0].id, action);
        assert_eq!(page2.messages[0].kind.as_deref(), Some(ACTION_KIND));

        state.teardown().await;
    }
}
