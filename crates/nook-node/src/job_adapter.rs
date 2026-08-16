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
    // The predicate lives in `nook-types` because the control plane has to
    // answer the same question — it refuses a chat session for a runtime that
    // cannot be one (MAIN-502 AC-2) — and two allowlists that "must agree" is
    // a bug with a delay on it.
    match nook_types::runtime_supports_chat(runtime) {
        true => Adapter::Streaming,
        false => Adapter::Tmux,
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
    stream_args(session_id, false, Permissions::Skip)
}

/// Who answers a permission prompt — the ONE thing that differs between a
/// headless loop run and a chat session a person is sitting in front of
/// (MAIN-502).
///
/// Kept as a parameter of the single argv builder rather than as a second
/// builder, because everything else about the launch is identical and a
/// duplicated argv is how the two drift: a flag added for the loop and missed
/// for chat is a bug nobody sees until a conversation behaves subtly unlike a
/// run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permissions {
    /// Nobody is watching: `--dangerously-skip-permissions`, sanctioned by the
    /// throwaway per-job worktree on a confined node.
    Skip,
    /// A person is: `--permission-prompt-tool stdio`, which makes the runtime
    /// ask over the SAME stdio channel it streams on — a `control_request` we
    /// answer with a `control_response`. The agent BLOCKS until it gets one,
    /// which is exactly the contract MAIN-502 AC-6 needs and the reason no
    /// timeout is imposed here.
    Ask,
}

/// The argv for a CHAT session (MAIN-502 AC-3/AC-6): the same streaming launch
/// a loop run makes, with the permission posture flipped.
///
/// Notably NOT `--dangerously-skip-permissions`. A chat session runs in a
/// person's own checkout, on their behalf, with them right there — the two
/// conditions that made skipping defensible for a job are both absent, and the
/// approval is the feature rather than an obstacle to it.
///
/// `settings` is `crate::human_permissions`' managed document (MAIN-620): the
/// narrowed allow-list that keeps a person from tapping "allow" through the
/// agent's routine tooling. `None` — no runtime it applies to, or a write that
/// failed — leaves the posture exactly as it was, asking about everything.
pub fn claude_chat_args(session_id: &str, settings: Option<&Path>) -> Vec<String> {
    let mut args = stream_args(session_id, false, Permissions::Ask);
    if let Some(path) = settings {
        args.push("--settings".into());
        args.push(path.display().to_string());
    }
    args
}

/// The same run, but continuing the agent session a previous run left behind
/// (MAIN-455 AC-3): `--resume <id>` instead of pinning a fresh `--session-id`.
///
/// This replaced `--from-pr`, which was tried first and PROVEN not to link on
/// the live stack: two consecutive runs of the same PR produced two distinct
/// agent sessions. Claude Code links a session to a PR when the session opens
/// one — a session that merely reads a PR through `gh` never acquires the link,
/// so for a reviewer the flag always came up empty. Resuming by an id WE derive
/// (stable per workspace + PR) makes the continuation a fact the caller checks
/// on disk, not a linkage hoped for.
pub fn claude_resume_args(session_id: &str) -> Vec<String> {
    stream_args(session_id, true, Permissions::Skip)
}

fn stream_args(session_id: &str, resume: bool, permissions: Permissions) -> Vec<String> {
    let mut args: Vec<String> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--replay-user-messages",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    match permissions {
        Permissions::Skip => args.push("--dangerously-skip-permissions".into()),
        Permissions::Ask => {
            args.push("--permission-prompt-tool".into());
            // `stdio` is the runtime's own name for "ask the client you are
            // already talking to", not a tool we provide.
            args.push("stdio".into());
        }
    }
    // `--resume` names an existing session to continue; `--session-id` pins a
    // NEW one. Passing both would ask for two different things at once.
    args.push(if resume { "--resume" } else { "--session-id" }.to_string());
    args.push(session_id.to_string());
    args
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
    /// The agent wants to use a tool and is BLOCKED until we answer
    /// (MAIN-502 AC-6). Only ever seen under [`Permissions::Ask`].
    PermissionRequest(PermissionRequest),
    /// Anything else.
    Ignored,
}

/// One tool the agent is waiting on a human for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRequest {
    /// The runtime's id for this exchange. The answer is addressed to it, and
    /// it is the only thing that ties a click in a browser back to the process
    /// that is blocked.
    pub id: String,
    /// `Bash`, `Write`, `Edit`, an MCP tool's name…
    pub tool_name: String,
    /// The runtime's own one-line summary — a command, a path. Empty when it
    /// offers none.
    pub description: String,
    /// The tool's arguments EXACTLY as the runtime sent them, kept so an
    /// `allow` can hand them straight back as `updatedInput`.
    ///
    /// Echoed rather than reconstructed, and never edited: this end is
    /// deciding whether the call may happen, not what the call is. A rewritten
    /// input would be a tool invocation the agent did not ask for and the
    /// human did not see.
    pub input: serde_json::Value,
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
        // The permission handshake (MAIN-502 AC-6), verified against the
        // pinned CLI: the runtime writes a `control_request` carrying
        // `can_use_tool` and then STOPS, reading its stdin, until a
        // `control_response` with a matching `request_id` comes back.
        //
        // Any OTHER control_request is `Ignored`, not an error, for the reason
        // the module gives: this is a protocol we do not own, and a CLI
        // release that adds a request subtype must not be able to fail a
        // running session. It does mean an unanswered request blocks that
        // exchange — which is the runtime's own behaviour, and visible, rather
        // than a silently wrong answer.
        Some("control_request") => {
            let Some(req) = v.get("request") else {
                return Event::Ignored;
            };
            if req.get("subtype").and_then(|s| s.as_str()) != Some("can_use_tool") {
                return Event::Ignored;
            }
            let Some(id) = v.get("request_id").and_then(|s| s.as_str()) else {
                // No id is no way to answer: an approval we could not address
                // is worse than none, because the human would think they had
                // given one.
                return Event::Ignored;
            };
            Event::PermissionRequest(PermissionRequest {
                id: id.to_string(),
                tool_name: req
                    .get("tool_name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("tool")
                    .to_string(),
                description: req
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or_default()
                    .to_string(),
                input: req.get("input").cloned().unwrap_or(serde_json::Value::Null),
            })
        }
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

/// Frame the answer to a permission request (MAIN-502 AC-6).
///
/// `allow` hands the tool's own arguments straight back as `updatedInput` —
/// see [`PermissionRequest::input`] for why they are echoed unedited. `deny`
/// carries a message, which the runtime surfaces to the agent as the tool's
/// error: the agent learns it was refused and can say something about it,
/// rather than watching a call fail for no stated reason.
pub fn permission_response_line(req: &PermissionRequest, allow: bool) -> String {
    let response = if allow {
        serde_json::json!({ "behavior": "allow", "updatedInput": req.input })
    } else {
        serde_json::json!({
            "behavior": "deny",
            "message": "denied by the person in this session",
        })
    };
    let frame = serde_json::json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": req.id,
            "response": response,
        },
    });
    format!("{}\n", serde_json::to_string(&frame).unwrap_or_default())
}

/// Write one framed line to a shared stdin.
///
/// The lower half of [`write_turn`], split out because a permission response
/// is the same "one JSON line, then flush" operation on the same handle and
/// under the same lock — the protocol's only serialisation requirement.
pub fn write_line(stdin: &SharedStdin, line: &str) -> Result<(), String> {
    let mut guard = stdin
        .lock()
        .map_err(|_| "the agent's stdin lock is poisoned".to_string())?;
    let Some(w) = guard.as_mut() else {
        return Err("the agent's stdin is closed".into());
    };
    w.write_all(line.as_bytes())
        .and_then(|_| w.flush())
        .map_err(|e| format!("could not write to the agent: {e}"))
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
    write_line(stdin, &user_turn_line(text))
}

/// How many recent lines to keep for a failure message.
const TAIL_LINES: usize = 40;

/// Ask the kernel to kill this child when we die (MAIN-506).
///
/// The unit sets `KillMode=process` so that an agent restart spares a
/// terminal's tmux server — correct, and untouched. But a STREAMING job has no
/// tmux: this agent is the only reader of the child's stdout, so a child that
/// outlives us produces output nobody records, no outcome, and no verdict —
/// it just burns CPU and disk until a human notices. There is nothing to
/// preserve, so the child goes with us.
///
/// Applied HERE and only here, which is what keeps AC-4 true: the tmux path in
/// `sessions` never comes through this function, and `PR_SET_PDEATHSIG` is
/// cleared across `fork` anyway, so a daemonising server could not inherit it.
///
/// The signal fires when the spawning THREAD exits, not the process — so the
/// thread that calls [`StreamingSession::spawn`] must be the one that pumps and
/// waits. `loop_job::run_agent_once` does exactly that, on its own blocking
/// thread, from spawn to `wait`.
///
/// Linux only. macOS has no `PR_SET_PDEATHSIG` equivalent, so there the child
/// is left to notice its stdin EOF, and the control plane's stall reaper is what
/// stops the JOB from hanging in `running` regardless of the process.
///
/// It reaches only as far as the CHILD, which for a sandboxed job is the
/// `docker exec` client rather than the agent (MAIN-611). Nothing is lost:
/// removing the container ends every process inside it, and `sandbox::Sandbox`
/// does that on drop — a stronger guarantee than this one, since it takes the
/// job's compose stack with it too.
fn die_with_parent(cmd: &mut Command) {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: `prctl` is async-signal-safe and this closure runs between
        // fork and exec, where only such calls are legal.
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            });
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = cmd;
}

impl StreamingSession {
    /// Spawn the runtime in `cwd` with `args`, wired for structured streaming.
    ///
    /// `extra_env` carries the same `NOOK_JOB_ID` / `NOOK_JOB_SEED` the tmux
    /// path sets, so a skill cannot tell which adapter is running it — that
    /// equivalence is what lets AC-4's fallback stay a genuine fallback rather
    /// than a second, subtly different world.
    ///
    /// `sandbox` is the confinement (MAIN-611 AC-1). `Some` runs the runtime
    /// INSIDE the job's container, which is what every loop job passes; `None`
    /// spawns it directly and is a human's own conversation (`chat.rs`), which
    /// this card deliberately does not confine (NG-2).
    ///
    /// The two are not the same launch in one respect worth knowing: a direct
    /// spawn INHERITS this process's whole environment — the node's join token
    /// and its own credentials included — while `docker exec` inherits nothing
    /// and carries only what `extra_env` names. That asymmetry is AC-7.
    pub fn spawn(
        runtime: &str,
        args: &[String],
        cwd: &Path,
        extra_env: &[(&str, &str)],
        sandbox: Option<&crate::sandbox::Sandbox>,
    ) -> Result<Self, String> {
        let mut cmd = match sandbox {
            Some(sb) => sb.exec_command(runtime, args, cwd, extra_env),
            None => {
                let mut cmd = Command::new(runtime);
                cmd.args(args).current_dir(cwd);
                for (k, v) in extra_env {
                    cmd.env(k, v);
                }
                cmd
            }
        };
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        die_with_parent(&mut cmd);
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

    /// Take the child's stderr.
    ///
    /// Nothing reads it on the loop path, which is survivable there because a
    /// job reports its own failure. It is not survivable in a chat session: a
    /// launch that dies before writing a single stdout line — a session id
    /// already in use, a runtime that will not start — puts its ONLY
    /// explanation here, and without this the conversation would say "the
    /// agent exited with status 1" and nothing else.
    pub fn take_stderr(&mut self) -> Option<std::process::ChildStderr> {
        self.child.stderr.take()
    }

    /// Wait for exit; `None` if it could not be reaped.
    pub fn wait(&mut self) -> Option<i32> {
        self.child.wait().ok().and_then(|s| s.code())
    }

    /// Has it exited yet? `Some(code)` once it has, `None` while it runs.
    ///
    /// The non-blocking half of [`wait`](Self::wait), for a caller that holds
    /// this behind a lock somebody else needs: a blocking wait taken across
    /// that lock is a deadlock waiting for a process that may never exit.
    pub fn try_wait(&mut self) -> Option<i32> {
        match self.child.try_wait() {
            // Still running, or exited on a signal with no code — either way
            // there is no code to report yet.
            Ok(Some(status)) => status.code().or(Some(-1)),
            _ => None,
        }
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

    /// MAIN-506 AC-3: the streaming child does not outlive the agent.
    ///
    /// Spawned from a thread that then exits, which is exactly what
    /// `PR_SET_PDEATHSIG` keys on — so a `sleep 60` that comes back in well
    /// under a second came back because the kernel killed it.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_streaming_child_dies_with_the_thread_that_spawned_it() {
        let mut session = std::thread::spawn(|| {
            StreamingSession::spawn("sleep", &["60".to_string()], Path::new("/"), &[], None)
        })
        .join()
        .expect("the spawning thread")
        .expect("sleep");

        let started = std::time::Instant::now();
        // Killed by a signal, so there is no exit code — the point is that this
        // returns at all rather than blocking for the full minute.
        assert_eq!(session.wait(), None, "the child was signalled, not exited");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "the child outlived its spawning thread"
        );
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

    /// The resume launch names an EXISTING session and must not also pin a new
    /// one — the two flags ask for different things, and passing both is how a
    /// warm reviewer silently becomes a cold one.
    #[test]
    fn resume_args_resume_and_never_pin() {
        let a = claude_resume_args("sess-2");
        let joined = a.join(" ");
        assert!(joined.contains("--resume sess-2"));
        assert!(!joined.contains("--session-id"));
        // Same streaming contract as a pinned run — the caller cannot tell.
        assert!(a.contains(&"-p".to_string()));
        assert!(joined.contains("--output-format stream-json"));
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

    /// MAIN-502 AC-3: a chat launch is the SAME launch, differing only in who
    /// answers a permission — and, since MAIN-620, in the managed allow-list
    /// that decides which permissions are worth asking about. Written as a diff
    /// between the two argvs rather than as a second expected list, so a flag
    /// added to the streaming contract cannot land in one and be forgotten in
    /// the other — which is exactly what a forked argv would let happen.
    #[test]
    fn the_chat_argv_is_the_run_argv_with_only_the_permission_posture_changed() {
        let run = claude_stream_args("sess-1");
        let chat = claude_chat_args("sess-1", Some(Path::new("/tmp/perm.json")));
        let common = |a: &[String]| -> Vec<String> {
            let mut out = Vec::new();
            let mut it = a.iter();
            while let Some(s) = it.next() {
                match s.as_str() {
                    "--dangerously-skip-permissions" | "stdio" => {}
                    // Flag AND its value, or the path would survive the filter
                    // and the diff would compare a list against a longer one.
                    "--permission-prompt-tool" | "--settings" => {
                        it.next();
                    }
                    _ => out.push(s.clone()),
                }
            }
            out
        };
        assert_eq!(
            common(&run),
            common(&chat),
            "everything but the permission posture must be identical"
        );
    }

    /// MAIN-620 AC-1: a human session carries the managed allow-list, and a
    /// loop job never does — its posture is `Skip`, and a second, narrower
    /// statement of permissions on top of a blanket one is only confusing.
    #[test]
    fn only_a_human_session_carries_the_managed_allow_list() {
        let chat = claude_chat_args("sess-1", Some(Path::new("/tmp/perm.json")));
        assert!(
            chat.join(" ").contains("--settings /tmp/perm.json"),
            "{chat:?}"
        );
        // No document — an unwritable config dir — leaves the launch exactly as
        // MAIN-502 shipped it: asking about everything, never failing to start.
        let bare = claude_chat_args("sess-1", None);
        assert!(!bare.contains(&"--settings".to_string()), "{bare:?}");
        assert!(!claude_stream_args("sess-1").contains(&"--settings".to_string()));
        assert!(!claude_resume_args("sess-1").contains(&"--settings".to_string()));
    }

    /// MAIN-502 AC-6, the negative that carries the weight: a chat session must
    /// NOT skip permissions. A person is sitting in front of it, and the whole
    /// feature is that they get asked.
    #[test]
    fn a_chat_asks_permission_and_a_run_skips_it() {
        let chat = claude_chat_args("sess-1", None);
        let joined = chat.join(" ");
        assert!(
            !chat.contains(&"--dangerously-skip-permissions".to_string()),
            "a chat session must never bypass permissions"
        );
        // `stdio` is the runtime's own name for "ask the client on this pipe",
        // which is what makes the request arrive as a `control_request` we can
        // put in front of a human.
        assert!(joined.contains("--permission-prompt-tool stdio"));
        assert!(
            joined.contains("--session-id sess-1"),
            "pinned, never resumed"
        );

        // …and the loop path is untouched: nobody is there to ask.
        let run = claude_stream_args("sess-1");
        assert!(run.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(!run.join(" ").contains("--permission-prompt-tool"));
    }

    /// The permission handshake, both halves, against the records the pinned
    /// CLI actually emits (MAIN-502 AC-6).
    #[test]
    fn a_permission_request_parses_and_both_answers_frame() {
        let line = r#"{"type":"control_request","request_id":"r-1","request":{"subtype":"can_use_tool","tool_name":"Write","input":{"file_path":"/tmp/note.txt","content":"banana\n"},"description":"note.txt","tool_use_id":"toolu_1"}}"#;
        let Event::PermissionRequest(req) = parse_event(line) else {
            panic!("a can_use_tool control request is a permission request");
        };
        assert_eq!(req.id, "r-1");
        assert_eq!(req.tool_name, "Write");
        assert_eq!(req.description, "note.txt");
        assert_eq!(req.input["file_path"], "/tmp/note.txt");

        // Allow hands the tool's arguments straight back. The runtime runs
        // `updatedInput`, so echoing is what makes an approval approve the call
        // the human was shown rather than some other one.
        let allow: serde_json::Value =
            serde_json::from_str(permission_response_line(&req, true).trim()).unwrap();
        assert_eq!(allow["type"], "control_response");
        assert_eq!(allow["response"]["subtype"], "success");
        assert_eq!(allow["response"]["request_id"], "r-1");
        assert_eq!(allow["response"]["response"]["behavior"], "allow");
        assert_eq!(
            allow["response"]["response"]["updatedInput"], req.input,
            "the input is echoed unedited"
        );

        let deny: serde_json::Value =
            serde_json::from_str(permission_response_line(&req, false).trim()).unwrap();
        assert_eq!(deny["response"]["response"]["behavior"], "deny");
        assert!(
            deny["response"]["response"]["message"].is_string(),
            "a refusal tells the agent it was refused, rather than failing mutely"
        );

        // One frame per line, like every other message on this pipe.
        assert_eq!(
            permission_response_line(&req, true).matches('\n').count(),
            1
        );
    }

    /// A control request we do not understand is IGNORED, not answered.
    ///
    /// Answering one would be inventing a reply to a question we did not read;
    /// failing on one would let a CLI release break a live session. Ignoring
    /// leaves that exchange visibly stuck, which is the runtime's own behaviour
    /// and the only honest option of the three.
    #[test]
    fn an_unknown_control_request_is_ignored_never_answered() {
        assert_eq!(
            parse_event(
                r#"{"type":"control_request","request_id":"r","request":{"subtype":"brand_new"}}"#
            ),
            Event::Ignored
        );
        // No id is no way to address an answer — and an approval nobody can
        // deliver is worse than none, because the human believes they gave it.
        assert_eq!(
            parse_event(
                r#"{"type":"control_request","request":{"subtype":"can_use_tool","tool_name":"Bash"}}"#
            ),
            Event::Ignored
        );
    }

    /// A permission request says nothing about whether a turn is in flight: the
    /// agent is mid-turn and BLOCKED, which is the same working state it was in
    /// a moment ago. Flipping the indicator here would show "idle" for exactly
    /// the interval a person most needs to see that it is waiting on them.
    #[test]
    fn a_permission_request_does_not_move_the_turn_signal() {
        let mut t = TurnState::default();
        assert_eq!(
            t.observe(&Event::AssistantText("working".into())),
            Some(true)
        );
        let req = PermissionRequest {
            id: "r".into(),
            tool_name: "Bash".into(),
            description: "ls".into(),
            input: serde_json::json!({}),
        };
        assert_eq!(t.observe(&Event::PermissionRequest(req)), None);
        assert!(t.active(), "still mid-turn, just waiting on a human");
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
                Event::TurnEnded { .. }
                | Event::Ignored
                | Event::TurnStarted
                // Never seen on this path: a run skips permissions.
                | Event::PermissionRequest(_) => {}
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
