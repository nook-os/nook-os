//! How a loop job's agent is actually run (MAIN-240).
//!
//! Until now there was one answer: launch the runtime in tmux, type at it with
//! `send_keys -l`, and scrape the PTY for the transcript. That works, and it is
//! the root of the loop's chat problems — the "agent is typing" signal has to be
//! inferred, message boundaries are reconstructed from screen output, and the
//! transcript carries ANSI noise that every reader has to strip. For a
//! conversation the control plane fully drives, a structured channel is
//! strictly better.
//!
//! So a job now picks an adapter by what its runtime can do:
//!
//! - [`Adapter::Streaming`] — the runtime speaks a structured protocol on
//!   stdin/stdout. Claude Code does: `-p --input-format stream-json
//!   --output-format stream-json` (verified against the pinned 2.1.220). Human
//!   turns are *written* as JSON; assistant text, tool calls and turn
//!   boundaries are *read* as JSON. No PTY, no key-typing, no scraping.
//! - [`Adapter::Tmux`] — everything else, unchanged. This is a fallback, not a
//!   deprecation (NG-1): interactive terminal sessions and any runtime without
//!   a streaming mode keep the existing path exactly as it is.
//!
//! This module owns the protocol — spawning, framing, and turning events into
//! transcript entries — and deliberately not the workspace/worktree lifecycle,
//! which stays in `loop_job`. That split is also what makes AC-6 cheap later:
//! moving the process behind a per-job container boundary changes how
//! [`spawn`] starts it, and nothing else.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use serde::Serialize;

/// Which execution strategy a runtime gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Adapter {
    /// Structured stdin/stdout streaming (no terminal).
    Streaming,
    /// The historical tmux + PTY path.
    Tmux,
}

/// Pick the adapter for a runtime.
///
/// A allowlist, not a probe: the node decides how it runs an agent, and an
/// unknown runtime gets the conservative path rather than being handed
/// flags it may not understand. Mirrors how `runtime_auth` allowlists its
/// probes (MAIN-126) — the control plane never names an executable or a flag.
pub fn adapter_for(runtime: &str) -> Adapter {
    match runtime {
        "claude" => Adapter::Streaming,
        _ => Adapter::Tmux,
    }
}

/// The argv for a streaming run of `claude`.
///
/// Every flag verified against the pinned CLI (2.1.220, `claude --help`):
///
/// - `-p/--print` — required; the format flags "only work with --print".
/// - `--input-format stream-json` — "realtime streaming input".
/// - `--output-format stream-json` — "realtime streaming".
/// - `--verbose` — stream-json emits per-event records only in verbose mode;
///   without it a `--print` run collapses to one final result, which would put
///   us right back to reconstructing a conversation from a blob.
/// - `--replay-user-messages` — "re-emit user messages from stdin back on
///   stdout for acknowledgment". This is what lets a steering message become a
///   transcript entry *when the agent actually received it*, rather than when
///   we hopefully wrote it.
/// - `--dangerously-skip-permissions` — the run is headless: there is no human
///   to answer a permission prompt, so without it every `nook` / `git` / edit
///   the agent attempts is denied, and the run can neither drive the board
///   (`nook whoami` was coming back "denied") nor change code. The agent runs
///   in a throwaway per-job worktree on a confined node — exactly the case
///   these autonomous permissions exist for.
/// - `--session-id` — pins the id so AC-5's resume has something to name.
pub fn claude_stream_args(session_id: &str) -> Vec<String> {
    [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--replay-user-messages",
        "--dangerously-skip-permissions",
        "--session-id",
        session_id,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

// ── The wire protocol ────────────────────────────────────────────────────────

/// One line we write to the agent's stdin: a user turn.
///
/// The shape Claude Code's stream-json input expects — a `user` message with
/// Anthropic-style content blocks. Kept as a typed struct rather than hand-built
/// JSON so a field rename is a compile error, not a silent no-op at runtime.
#[derive(Debug, Serialize)]
pub struct UserTurn<'a> {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub message: UserMessage<'a>,
}

#[derive(Debug, Serialize)]
pub struct UserMessage<'a> {
    pub role: &'static str,
    pub content: Vec<ContentBlock<'a>>,
}

#[derive(Debug, Serialize)]
pub struct ContentBlock<'a> {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub text: &'a str,
}

/// Frame a human turn for the agent's stdin.
pub fn user_turn_line(text: &str) -> String {
    let turn = UserTurn {
        kind: "user",
        message: UserMessage {
            role: "user",
            content: vec![ContentBlock { kind: "text", text }],
        },
    };
    // Serialization of a fixed shape cannot fail; a newline terminates the frame.
    format!("{}\n", serde_json::to_string(&turn).unwrap_or_default())
}

/// What one output line means to us.
///
/// Deliberately a small vocabulary over a large protocol: the stream carries
/// far more than this, and mapping every field would couple us to a schema we
/// do not own. We take what the transcript and the turn signal actually need
/// and ignore the rest — an unrecognised line is [`Event::Ignored`], never an
/// error, so a CLI upgrade that adds a record type cannot break a running job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The session is up; carries the id to resume with (AC-5).
    SessionStarted { session_id: String },
    /// Assistant prose to record.
    AssistantText(String),
    /// The agent used a tool — worth a transcript line, since it is most of
    /// what a spec run actually does.
    ToolUse { name: String },
    /// A turn began: the agent is working (AC-2's real signal).
    TurnStarted,
    /// A turn ended.
    TurnEnded { ok: bool, message: Option<String> },
    /// Our own user message, echoed back by `--replay-user-messages`, which is
    /// the acknowledgement that the agent received it.
    UserEcho(String),
    /// Anything else.
    Ignored,
}

/// Parse one output line.
///
/// Tolerant by construction: a blank line, a non-JSON line (a stray warning on
/// stdout), or an unknown `type` all come back `Ignored`. The alternative —
/// failing the job on an unexpected record — would make every CLI release a
/// potential outage.
pub fn parse_event(line: &str) -> Event {
    let line = line.trim();
    if line.is_empty() {
        return Event::Ignored;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return Event::Ignored;
    };
    match v.get("type").and_then(|t| t.as_str()) {
        Some("system") => {
            // The init record carries the session id.
            if v.get("subtype").and_then(|s| s.as_str()) == Some("init") {
                if let Some(id) = v.get("session_id").and_then(|s| s.as_str()) {
                    return Event::SessionStarted {
                        session_id: id.to_string(),
                    };
                }
            }
            Event::Ignored
        }
        Some("assistant") => {
            // An assistant record starts a turn and carries content blocks.
            let blocks = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array());
            let Some(blocks) = blocks else {
                return Event::TurnStarted;
            };
            for b in blocks {
                match b.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                            if !t.trim().is_empty() {
                                return Event::AssistantText(t.to_string());
                            }
                        }
                    }
                    Some("tool_use") => {
                        let name = b
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("tool")
                            .to_string();
                        return Event::ToolUse { name };
                    }
                    _ => {}
                }
            }
            Event::TurnStarted
        }
        Some("user") => {
            // With --replay-user-messages this is our own turn coming back.
            let text = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
                .and_then(|blocks| {
                    blocks
                        .iter()
                        .find_map(|b| b.get("text").and_then(|t| t.as_str()))
                })
                .unwrap_or_default()
                .to_string();
            Event::UserEcho(text)
        }
        Some("result") => {
            let is_error = v.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false);
            let message = v
                .get("result")
                .and_then(|r| r.as_str())
                .map(|s| s.to_string());
            Event::TurnEnded {
                ok: !is_error,
                message,
            }
        }
        _ => Event::Ignored,
    }
}

/// Whether the agent is mid-turn, derived from real events rather than guessed
/// from output timing (AC-2).
///
/// A turn opens on the first assistant activity and closes on `result`. Kept as
/// a tiny state machine so the rule lives in one place and is testable without
/// a process.
#[derive(Debug, Default)]
pub struct TurnState {
    active: bool,
}

impl TurnState {
    /// Fold an event in; returns `Some(now_active)` when the state CHANGED, so
    /// a caller can report a transition without diffing.
    pub fn observe(&mut self, ev: &Event) -> Option<bool> {
        let next = match ev {
            Event::AssistantText(_) | Event::ToolUse { .. } | Event::TurnStarted => true,
            Event::TurnEnded { .. } => false,
            _ => return None,
        };
        if next == self.active {
            return None;
        }
        self.active = next;
        Some(next)
    }

    pub fn active(&self) -> bool {
        self.active
    }
}

/// A running streaming agent: the child, its stdin, and a bounded tail kept for
/// the finish message.
pub struct StreamingSession {
    child: Child,
    /// Shared rather than owned: the run loop writes the opening turn while
    /// `deliver_message` writes steering turns from another thread. `ChildStdin`
    /// cannot be cloned, so the handle itself is shared behind a mutex — and
    /// since a frame is exactly one line, holding the lock per write is all the
    /// serialisation the protocol needs.
    stdin: SharedStdin,
    pub tail: Arc<Mutex<VecDeque<String>>>,
}

/// The agent's stdin, shareable across threads. `None` once closed.
pub type SharedStdin = Arc<Mutex<Option<std::process::ChildStdin>>>;

/// Close the agent's stdin, telling it no more turns are coming.
///
/// Load-bearing, not tidiness: with `--input-format stream-json` the runtime
/// keeps reading turns until EOF, and our reader blocks on its stdout until the
/// process exits. Leaving stdin open after the run's result would deadlock the
/// two — the agent waiting for input that never comes, us waiting for output
/// that never comes. Closing on the result is what lets it exit.
pub fn close_stdin(stdin: &SharedStdin) {
    if let Ok(mut g) = stdin.lock() {
        g.take();
    }
}

/// Write one framed user turn to a shared stdin.
pub fn write_turn(stdin: &SharedStdin, text: &str) -> Result<(), String> {
    let mut guard = stdin
        .lock()
        .map_err(|_| "the agent's stdin lock is poisoned".to_string())?;
    let Some(w) = guard.as_mut() else {
        return Err("the agent's stdin is closed".into());
    };
    w.write_all(user_turn_line(text).as_bytes())
        .and_then(|_| w.flush())
        .map_err(|e| format!("could not write to the agent: {e}"))
}

/// How many recent lines to keep for a failure message.
const TAIL_LINES: usize = 40;

impl StreamingSession {
    /// Spawn the runtime in `cwd` with `args`, wired for structured streaming.
    ///
    /// `extra_env` carries the same `NOOK_JOB_ID` / `NOOK_JOB_SEED` the tmux
    /// path sets, so a skill cannot tell which adapter is running it — that
    /// equivalence is what lets AC-4's fallback stay a genuine fallback rather
    /// than a second, subtly different world.
    pub fn spawn(
        runtime: &str,
        args: &[String],
        cwd: &Path,
        extra_env: &[(&str, &str)],
    ) -> Result<Self, String> {
        let mut cmd = Command::new(runtime);
        cmd.args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("could not start {runtime}: {e}"))?;
        let stdin = Arc::new(Mutex::new(child.stdin.take()));
        Ok(StreamingSession {
            child,
            stdin,
            tail: Arc::new(Mutex::new(VecDeque::new())),
        })
    }

    /// Send a human turn. This is where MAIN-231's steering messages land —
    /// written as structured input instead of typed at a terminal.
    pub fn send(&mut self, text: &str) -> Result<(), String> {
        write_turn(&self.stdin, text)
    }

    /// A shared handle on the agent's stdin, for the delivery registry.
    pub fn stdin_handle(&self) -> SharedStdin {
        self.stdin.clone()
    }

    /// Take the child's stdout for the reader thread.
    pub fn take_stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.child.stdout.take()
    }

    /// Wait for exit; `None` if it could not be reaped.
    pub fn wait(&mut self) -> Option<i32> {
        self.child.wait().ok().and_then(|s| s.code())
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }

    /// The recent output, for an honest failure message.
    pub fn tail_text(&self) -> String {
        self.tail
            .lock()
            .map(|t| t.iter().cloned().collect::<Vec<_>>().join("\n"))
            .unwrap_or_default()
    }
}

/// Read `stdout` line by line, handing each parsed event to `on_event`, and
/// keeping a bounded tail. Blocking; run it on its own thread.
pub fn pump_events<F: FnMut(Event)>(
    stdout: std::process::ChildStdout,
    tail: Arc<Mutex<VecDeque<String>>>,
    mut on_event: F,
) {
    let reader = BufReader::new(stdout);
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if let Ok(mut t) = tail.lock() {
            t.push_back(line.clone());
            while t.len() > TAIL_LINES {
                t.pop_front();
            }
        }
        on_event(parse_event(&line));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_allowlist_sends_claude_streaming_and_everything_else_to_tmux() {
        assert_eq!(adapter_for("claude"), Adapter::Streaming);
        // NG-1: the fallback is the point, not a leftover.
        assert_eq!(adapter_for("bash"), Adapter::Tmux);
        assert_eq!(adapter_for("hermes"), Adapter::Tmux);
        assert_eq!(adapter_for("codex"), Adapter::Tmux);
        assert_eq!(adapter_for(""), Adapter::Tmux);
    }

    #[test]
    fn the_claude_argv_carries_every_flag_the_protocol_needs() {
        let a = claude_stream_args("sess-1");
        let joined = a.join(" ");
        // --print is what enables the format flags at all.
        assert!(a.contains(&"-p".to_string()));
        assert!(joined.contains("--input-format stream-json"));
        assert!(joined.contains("--output-format stream-json"));
        // Without --verbose the run collapses to one final result and there is
        // no per-event stream to build a transcript from.
        assert!(a.contains(&"--verbose".to_string()));
        // The acknowledgement that a steering message actually arrived.
        assert!(a.contains(&"--replay-user-messages".to_string()));
        assert!(joined.contains("--session-id sess-1"));
    }

    #[test]
    fn a_user_turn_is_framed_as_one_json_line() {
        let line = user_turn_line("skip the CLI");
        assert!(line.ends_with('\n'), "one frame per line");
        assert_eq!(line.matches('\n').count(), 1, "no embedded newlines");
        let v: serde_json::Value = serde_json::from_str(line.trim()).expect("valid JSON");
        assert_eq!(v["type"], "user");
        assert_eq!(v["message"]["role"], "user");
        assert_eq!(v["message"]["content"][0]["type"], "text");
        assert_eq!(v["message"]["content"][0]["text"], "skip the CLI");
    }

    #[test]
    fn a_multiline_message_stays_one_frame() {
        // The tmux path had to flatten newlines because `send_keys` would
        // submit early. Structured input does not, and that is a real gain:
        // a pasted multi-line brief arrives intact.
        let line = user_turn_line("first line\nsecond line");
        assert_eq!(line.matches('\n').count(), 1);
        let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(
            v["message"]["content"][0]["text"],
            "first line\nsecond line"
        );
    }

    #[test]
    fn parses_the_records_the_transcript_needs() {
        assert_eq!(
            parse_event(r#"{"type":"system","subtype":"init","session_id":"abc"}"#),
            Event::SessionStarted {
                session_id: "abc".into()
            }
        );
        assert_eq!(
            parse_event(
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hello"}]}}"#
            ),
            Event::AssistantText("hello".into())
        );
        assert_eq!(
            parse_event(
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read"}]}}"#
            ),
            Event::ToolUse {
                name: "Read".into()
            }
        );
        assert_eq!(
            parse_event(r#"{"type":"user","message":{"content":[{"type":"text","text":"go"}]}}"#),
            Event::UserEcho("go".into())
        );
        assert_eq!(
            parse_event(r#"{"type":"result","is_error":false,"result":"done"}"#),
            Event::TurnEnded {
                ok: true,
                message: Some("done".into())
            }
        );
        assert_eq!(
            parse_event(r#"{"type":"result","is_error":true,"result":"boom"}"#),
            Event::TurnEnded {
                ok: false,
                message: Some("boom".into())
            }
        );
    }

    /// A CLI upgrade must not be able to fail a running job. Anything we do not
    /// recognise is ignored, never an error.
    #[test]
    fn unknown_and_malformed_lines_are_ignored_not_fatal() {
        assert_eq!(parse_event(""), Event::Ignored);
        assert_eq!(parse_event("   "), Event::Ignored);
        assert_eq!(parse_event("not json at all"), Event::Ignored);
        assert_eq!(
            parse_event(r#"{"type":"brand_new_record_type"}"#),
            Event::Ignored
        );
        assert_eq!(parse_event(r#"{"no_type":true}"#), Event::Ignored);
        // A well-formed assistant record with no usable block still reads as
        // turn activity rather than nothing.
        assert_eq!(
            parse_event(r#"{"type":"assistant","message":{"content":[]}}"#),
            Event::TurnStarted
        );
    }

    #[test]
    fn the_turn_signal_tracks_real_events_and_reports_only_changes() {
        let mut t = TurnState::default();
        assert!(!t.active());

        // First assistant activity opens the turn.
        assert_eq!(t.observe(&Event::AssistantText("hi".into())), Some(true));
        assert!(t.active());
        // More activity in the same turn is not a transition.
        assert_eq!(
            t.observe(&Event::ToolUse {
                name: "Read".into()
            }),
            None
        );
        assert_eq!(t.observe(&Event::AssistantText("more".into())), None);
        // The result closes it.
        assert_eq!(
            t.observe(&Event::TurnEnded {
                ok: true,
                message: None
            }),
            Some(false)
        );
        assert!(!t.active());
        // Closing twice is not a transition either.
        assert_eq!(
            t.observe(&Event::TurnEnded {
                ok: true,
                message: None
            }),
            None
        );
        // Events that say nothing about activity leave it alone.
        assert_eq!(t.observe(&Event::UserEcho("x".into())), None);
        assert_eq!(t.observe(&Event::Ignored), None);
    }

    /// The whole point, end to end: a scripted exchange becomes the transcript
    /// entries and turn signal a real run would produce (AC-7's load-bearing
    /// case, without needing a logged-in agent).
    #[test]
    fn a_scripted_exchange_round_trips_to_transcript_entries() {
        let script = [
            r#"{"type":"system","subtype":"init","session_id":"s-1"}"#,
            r#"{"type":"user","message":{"content":[{"type":"text","text":"draft the spec"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Reading the code."}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Grep"}]}}"#,
            r#"garbage that is not json"#,
            r###"{"type":"assistant","message":{"content":[{"type":"text","text":"## Problem"}]}}"###,
            r#"{"type":"result","is_error":false,"result":"ok"}"#,
        ];

        let mut turn = TurnState::default();
        let mut entries: Vec<(&str, String)> = Vec::new();
        let mut transitions: Vec<bool> = Vec::new();
        let mut session = None;

        for line in script {
            let ev = parse_event(line);
            if let Some(now) = turn.observe(&ev) {
                transitions.push(now);
            }
            match ev {
                Event::SessionStarted { session_id } => session = Some(session_id),
                // Mirrors the driver: the echo is NOT transcribed. The control
                // plane already recorded this line when it sent it, and
                // transcribing it here too is what made a steering message
                // appear twice and read as the agent parroting the human.
                Event::UserEcho(_) => {}
                Event::AssistantText(t) => entries.push(("agent", t)),
                Event::ToolUse { name } => entries.push(("agent", format!("· {name}"))),
                Event::TurnEnded { .. } | Event::Ignored | Event::TurnStarted => {}
            }
        }

        assert_eq!(session.as_deref(), Some("s-1"), "resume id captured");
        assert_eq!(
            entries,
            vec![
                ("agent", "Reading the code.".to_string()),
                ("agent", "· Grep".to_string()),
                ("agent", "## Problem".to_string()),
            ],
            "the echoed human turn is not re-transcribed (the control plane owns \
             that line), and the garbage line vanished"
        );
        // Exactly one open and one close — not a flicker per event.
        assert_eq!(transitions, vec![true, false]);
        assert!(!turn.active(), "the run ended idle");
    }
}
