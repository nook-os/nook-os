//! The command surface the AGENT surfaces share (MAIN-530).
//!
//! A chat session and a loop run are the control plane's, not `nook-chat`'s, so
//! the two endpoints a client already knows how to call had nothing behind them
//! here. This is that behind: the same request and response shapes
//! (`ChatCommand`, `RunChatCommand`, `ChatCommandResult` — one definition, in
//! `nook-types`), so one dumb frontend serves three backends.
//!
//! What differs between the two surfaces is a single sentence — what `/status`
//! reports — which is why [`run`] takes it as a future rather than this module
//! learning what a session or a run is. Everything else, including the set
//! itself and the unknown-name refusal, is here once.

use std::future::Future;

use nook_errors::{ApiError, ApiResult};
use nook_types::{ChatCommand, ChatCommandResult, RunChatCommand};

/// One command as the server defines it — the wire shape plus nothing, because
/// what these two do is decided by the caller's `status` future and by
/// [`help_text`].
struct Spec {
    name: &'static str,
    description: &'static str,
}

/// THE set (AC-3). Discovery returns it, `/help` renders it, and execution
/// dispatches on it, so a command cannot exist in one of the three and not the
/// others.
///
/// Neither takes an argument, which is why no `args_hint` is modelled: an
/// agent surface's commands ask about the surface, and there is nothing to ask
/// them about. NG-1 is the other half of that — nothing here stops, cancels or
/// interrupts anything.
const COMMANDS: &[Spec] = &[
    Spec {
        name: "help",
        description: "List the commands you can use here.",
    },
    Spec {
        name: "status",
        description: "Say what this is doing right now.",
    },
];

pub fn catalog() -> Vec<ChatCommand> {
    COMMANDS
        .iter()
        .map(|s| ChatCommand {
            name: s.name.to_string(),
            args_hint: None,
            description: s.description.to_string(),
        })
        .collect()
}

/// `/help`'s body, and the only rendering of the set that exists — the client
/// composes nothing.
///
/// The closing sentence is AC-3's: a person typing `/nook-spec …` at an agent
/// must be able to read, here, that it reaches the agent as they typed it
/// rather than being swallowed as a bad command.
fn help_text() -> String {
    let mut out = String::from("Commands you can use here:");
    for c in COMMANDS {
        out.push_str("\n/");
        out.push_str(c.name);
        out.push_str(" — ");
        out.push_str(c.description);
    }
    out.push_str("\n\nAnything else starting with / is not a command here — it is sent to the agent exactly as you typed it.");
    out
}

/// Execute a command, given the one thing this module cannot know: what
/// `/status` says on this surface.
///
/// `status` is a future and not a string so a `/help` costs no database work —
/// the surfaces build their sentence from several joins.
pub async fn run<F, Fut>(req: &RunChatCommand, status: F) -> ApiResult<ChatCommandResult>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = ApiResult<String>>,
{
    // A leading slash is accepted because that is what a person types; the name
    // on the wire is the bare word either way — the same tolerance nook-chat's
    // surface has.
    let name = req.name.trim().trim_start_matches('/');

    let text = match name {
        "help" => help_text(),
        "status" => status().await?,
        // The refusal names what was asked for and where to look, and no arm
        // above has run, so nothing changed.
        _ => {
            return Err(ApiError::BadRequest(format!(
                "Unknown command /{name} — try /help"
            )))
        }
    };

    // Both answers are for the caller's eyes only (NG-4): a `/status` is never
    // part of the session's conversation or the run's transcript, so nothing is
    // posted and `posted_message_id` stays empty.
    Ok(ChatCommandResult {
        ephemeral: Some(text),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn run_named(name: &str) -> ApiResult<ChatCommandResult> {
        run(
            &RunChatCommand {
                name: name.into(),
                args: None,
            },
            || async { Ok("Run: running".to_string()) },
        )
        .await
    }

    /// AC-3: `/help` renders the set discovery returns — neither can gain a
    /// command the other has not — and says what unrecognised slash text does.
    #[tokio::test]
    async fn help_renders_the_set_and_the_passthrough_rule() {
        let text = run_named("help")
            .await
            .expect("/help runs")
            .ephemeral
            .expect("/help answers ephemerally");
        for c in catalog() {
            assert!(
                text.contains(&format!("/{}", c.name)) && text.contains(&c.description),
                "{} missing from {text}",
                c.name
            );
        }
        assert!(
            text.contains("sent to the agent exactly as you typed it"),
            "AC-3: /help states the passthrough rule: {text}"
        );
    }

    /// NG-4: neither command posts. A surface that persisted one would put a
    /// `/status` in the transcript somebody later reads as the run's own work.
    #[tokio::test]
    async fn nothing_is_posted() {
        for name in ["help", "status", "/status"] {
            let result = run_named(name).await.expect("runs");
            assert!(
                result.posted_message_id.is_none(),
                "{name} posted something"
            );
            assert!(result.ephemeral.is_some(), "{name} answered nothing");
        }
    }

    /// An unknown name is a 400 carrying the sentence that says where to look,
    /// and the status future is never awaited — nothing was looked up for a
    /// command that does not exist.
    #[tokio::test]
    async fn an_unknown_command_is_refused_without_running_anything() {
        let mut asked = false;
        let err = run(
            &RunChatCommand {
                name: "stop".into(),
                args: None,
            },
            || {
                asked = true;
                async { Ok(String::new()) }
            },
        )
        .await
        .expect_err("an unknown command is refused");
        assert!(
            matches!(&err, ApiError::BadRequest(m) if m == "Unknown command /stop — try /help")
        );
        assert!(!asked, "the status lookup did not run");
    }
}
