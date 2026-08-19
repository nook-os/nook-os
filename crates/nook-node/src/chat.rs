//! A session driven as a CONVERSATION rather than as a terminal (MAIN-502).
//!
//! The pieces were already here and already used together by loop jobs:
//! [`crate::job_adapter`] runs `claude` headless over a structured stdin/stdout
//! protocol, and the browser already renders that shape of exchange. What was
//! missing is a session that IS one — an agent process with no tmux anywhere,
//! taking whole messages instead of keystrokes.
//!
//! Two things make this genuinely different from `loop_job::drive_streaming`,
//! and both are why it is a module rather than a fourth argument to that:
//!
//! - **The process outlives a turn.** A job sends one opening turn, waits for
//!   its result, and closes stdin so the agent can exit. A conversation does
//!   the opposite: stdin stays open for as long as the session does, so every
//!   `result` is the end of a turn and not the end of anything else. (Verified
//!   against the pinned CLI: one process, many turns.)
//! - **A human answers permissions.** A job runs with
//!   `--dangerously-skip-permissions` because nobody is there. Somebody is
//!   here, so the runtime asks — and BLOCKS — over the same stdio channel, and
//!   this module is what holds the blocked request until an answer comes back
//!   from the browser. Which requests are worth putting in front of that
//!   person is `crate::human_permissions`' business (MAIN-620), and "always"
//!   is this module's: a remembered tool is answered by the reader thread and
//!   never announced again.
//!
//! What it deliberately does not own: the argv (that is `job_adapter`'s, so the
//! two adapters cannot drift), and the conversation itself (that is the control
//! plane's — messages are persisted there, which is what makes them survive a
//! reload, a reconnect and this process dying).

use std::collections::{HashMap, HashSet};
use std::path::Path;

use nook_proto::NodeToControl;
use nook_types::SessionId;
use tokio::sync::mpsc::Sender;

use crate::job_adapter::{self, Event, PermissionRequest, SharedStdin, StreamingSession};

/// One live chat agent.
pub struct ChatHandle {
    /// Where a human turn goes. Shared because the reader thread and the
    /// manager thread both write to it — a turn from one, a permission answer
    /// from the other.
    stdin: SharedStdin,
    /// Permission requests the agent is blocked on, by the runtime's own id.
    ///
    /// Held HERE and not merely on the control plane's row because answering
    /// needs the tool's original arguments back (see
    /// [`PermissionRequest::input`]), and those never leave this machine — the
    /// browser sends a verdict, not a tool call.
    pending: std::sync::Arc<std::sync::Mutex<HashMap<String, PermissionRequest>>>,
    /// Tools this session's person said "always" to (MAIN-620 AC-3).
    ///
    /// The reader thread consults it BEFORE announcing a request, so a
    /// remembered tool is answered here and never becomes a prompt again. In
    /// memory and scoped to this agent process on purpose: "this tool, this
    /// session" is what the button says, and a durable grant is a different,
    /// larger decision than the one a person makes to get unblocked.
    always: std::sync::Arc<std::sync::Mutex<HashSet<String>>>,
    /// Kept only to kill it. Everything else about the process is the reader
    /// thread's.
    child: std::sync::Arc<std::sync::Mutex<StreamingSession>>,
}

impl ChatHandle {
    /// Send a human turn. A closed stdin means the agent has gone.
    pub fn send(&self, text: &str) -> Result<(), String> {
        job_adapter::write_turn(&self.stdin, text)
    }

    /// Answer a permission the agent is blocked on.
    ///
    /// A request id we are not holding is DROPPED, not guessed at: it is a
    /// second answer to something already settled, or an answer addressed to a
    /// previous process. Either way there is nothing waiting for it, and
    /// making something up would be inventing an approval.
    ///
    /// `remember` is "allow always (this tool, this session)" (MAIN-620 AC-3):
    /// the tool joins [`ChatHandle::always`] and the reader answers it directly
    /// from then on. Only ever recorded on an ALLOW — remembering a denial
    /// would refuse a tool for the rest of the session on one tap, with no way
    /// back short of a restart, and nothing in the UI says that is what it does.
    pub fn decide(&self, request_id: &str, allow: bool, remember: bool) -> Result<(), String> {
        let req = self
            .pending
            .lock()
            .map_err(|_| "the pending-permission lock is poisoned".to_string())?
            .remove(request_id);
        let Some(req) = req else {
            return Err(format!("no permission request {request_id} is outstanding"));
        };
        if allow && remember {
            if let Ok(mut a) = self.always.lock() {
                a.insert(req.tool_name.clone());
            }
        }
        job_adapter::write_line(
            &self.stdin,
            &job_adapter::permission_response_line(&req, allow),
        )
    }

    pub fn kill(&self) {
        if let Ok(mut c) = self.child.lock() {
            c.kill();
        }
    }
}

/// Start an agent for a chat session.
///
/// `Err` is a session that never got going — the caller fails the row, exactly
/// as the tmux path does for a missing checkout or an unknown runtime, so the
/// browser is told why instead of watching "starting" forever.
pub fn start(
    out: &Sender<NodeToControl>,
    session_id: SessionId,
    runtime: &str,
    cwd: &Path,
    env: &[(&str, &str)],
) -> Result<ChatHandle, String> {
    // A FRESH agent id per launch, not this session's id.
    //
    // Verified against the pinned CLI: `--session-id` on an id that already has
    // a session file fails outright — *"Session ID … is already in use"*, exit
    // 1, before a single stdout line. Pinning our session id would therefore
    // work exactly once and make RESTART impossible, which is the one operation
    // a session must always have.
    //
    // Pinned rather than resumed, which is NG-2: a chat session's conversation
    // is its own. The durable history is the control plane's rows, so a restart
    // keeps everything a person can read — the agent starts fresh, and the id
    // goes on the transcript below so an operator can resume it by hand.
    let agent_session = uuid::Uuid::now_v7().to_string();
    // The managed allow-list (MAIN-620): the agent's routine tooling — `nook`,
    // reads, edits in this very checkout — stops raising a prompt, and
    // everything else still does. Best-effort by design; see
    // `human_permissions::settings_for` for why a failure here is not one.
    let settings = crate::human_permissions::settings_for(runtime, cwd);
    let args = job_adapter::claude_chat_args(&agent_session, settings.as_deref());
    // No sandbox, deliberately (MAIN-611 NG-2): this is a person's own
    // conversation on their own machine, not a loop job driven by untrusted
    // instructions, and confining a human's shell is not what that card is for.
    let mut session = StreamingSession::spawn(runtime, &args, cwd, env, None)?;
    let stdout = match session.take_stdout() {
        Some(s) => s,
        None => {
            session.kill();
            return Err("the agent produced no stdout".into());
        }
    };
    // Drained into the SAME bounded tail the stdout pump keeps, so a launch
    // that dies before saying anything on stdout still explains itself in the
    // conversation instead of leaving a bare exit status.
    if let Some(stderr) = session.take_stderr() {
        let tail = session.tail.clone();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stderr)
                .lines()
                .map_while(Result::ok)
            {
                if let Ok(mut t) = tail.lock() {
                    t.push_back(line);
                    while t.len() > TAIL_LINES {
                        t.pop_front();
                    }
                }
            }
        });
    }

    let stdin = session.stdin_handle();
    let tail = session.tail.clone();
    let pending = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));
    let always = std::sync::Arc::new(std::sync::Mutex::new(HashSet::new()));
    let child = std::sync::Arc::new(std::sync::Mutex::new(session));

    let tx = out.clone();
    let pending_reader = pending.clone();
    let always_reader = always.clone();
    let stdin_reader = stdin.clone();
    let child_reader = child.clone();
    std::thread::spawn(move || {
        job_adapter::pump_events(stdout, tail, |ev| match ev {
            // Recorded as an ordinary line of the conversation, because that is
            // what it is: the agent talking. The control plane persists it
            // before anyone sees it, which is what makes it survive a reload.
            Event::AssistantText(text) => say(&tx, session_id, "agent", text),
            // The same `· Name` shape a loop transcript uses, so the browser's
            // existing fold-tool-activity mapping reads a session and a run
            // identically instead of learning a second convention.
            Event::ToolUse { name } => say(&tx, session_id, "agent", format!("· {name}")),
            Event::PermissionRequest(req) => {
                // Already granted for the rest of this session (MAIN-620 AC-3):
                // answered here and never announced, so "always" means what the
                // button says instead of merely pre-filling the next prompt.
                // The agent's own `· <tool>` line is still in the conversation,
                // so the tool call remains visible — it just is not a question.
                let remembered = always_reader
                    .lock()
                    .map(|a| a.contains(&req.tool_name))
                    .unwrap_or(false);
                if remembered {
                    let _ = job_adapter::write_line(
                        &stdin_reader,
                        &job_adapter::permission_response_line(&req, true),
                    );
                    return;
                }
                let frame = NodeToControl::ChatPermission {
                    session_id,
                    request_id: req.id.clone(),
                    tool_name: req.tool_name.clone(),
                    description: req.description.clone(),
                };
                // Held BEFORE it is announced. Announcing first opens a window
                // in which a very fast answer arrives for a request this end
                // is not yet holding, and that answer would be dropped as
                // stale — leaving the agent blocked on a permission somebody
                // has already granted.
                if let Ok(mut p) = pending_reader.lock() {
                    p.insert(req.id.clone(), req);
                }
                let _ = tx.blocking_send(frame);
            }
            // The runtime's own echo of a human turn. NOT recorded, for the
            // reason `drive_streaming` gives at length: the control plane wrote
            // that line when it accepted it, and recording it again is how a
            // message appears twice and reads as the agent parroting you.
            Event::UserEcho(_) => {}
            // On the durable transcript, exactly as `drive_streaming` records
            // it: the runtime's own id for this conversation, which is what an
            // operator resumes by hand after a restart. An in-memory copy would
            // die with the very restart it is meant to survive.
            Event::SessionStarted { session_id: agent } => {
                say(&tx, session_id, "system", format!("agent session {agent}"))
            }
            // A turn ended. Deliberately nothing: stdin stays open, so the next
            // thing the person types is simply the next turn. This is the one
            // place a conversation differs from a run, where the same event
            // closes stdin and lets the agent exit.
            Event::TurnEnded { .. } => {}
            Event::TurnStarted | Event::Ignored => {}
        });

        // The stream ended: the agent exited, crashed, or was killed. Say so in
        // the conversation — a chat that simply stopped answering is
        // indistinguishable from one that is thinking — and then report the
        // exit, which is what moves the row out of `running`.
        let code = reap(&child_reader);
        let tail = child_reader
            .lock()
            .map(|c| c.tail_text())
            .unwrap_or_default();
        let reason = match code {
            Some(0) => "the agent exited".to_string(),
            Some(c) => format!("the agent exited with status {c}"),
            None => "the agent died without an exit status".to_string(),
        };
        say(
            &tx,
            session_id,
            "system",
            if tail.is_empty() {
                reason
            } else {
                format!("{reason}\n{tail}")
            },
        );
        let _ = tx.blocking_send(NodeToControl::SessionExited {
            session_id,
            exit_code: code,
        });
    });

    Ok(ChatHandle {
        stdin,
        pending,
        always,
        child,
    })
}

/// How many recent output lines to keep for a failure message. The stdout pump
/// bounds its own; this is the stderr drain's half of the same budget.
const TAIL_LINES: usize = 40;

/// How long to let the agent finish exiting on its own before insisting.
const REAP_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Reap the child WITHOUT holding its lock across a blocking wait.
///
/// The lock is shared with [`ChatHandle::kill`], which the session manager
/// calls on its own thread — the one thread every session's commands queue
/// behind. A blocking `wait()` taken across that lock would hand a process
/// that closed stdout but did not exit the power to freeze every other session
/// on the machine, which is precisely the stall MAIN-362 moved this work off
/// the read loop to prevent.
///
/// So: poll, releasing the lock between attempts, and after the grace period
/// stop waiting and kill it. A process still holding stdout open five seconds
/// after its stream ended is not going to finish on its own.
fn reap(child: &std::sync::Arc<std::sync::Mutex<StreamingSession>>) -> Option<i32> {
    let deadline = std::time::Instant::now() + REAP_GRACE;
    loop {
        if let Ok(mut c) = child.lock() {
            if let Some(code) = c.try_wait() {
                return Some(code);
            }
            if std::time::Instant::now() >= deadline {
                c.kill();
                return c.wait();
            }
        } else {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn say(out: &Sender<NodeToControl>, session_id: SessionId, role: &str, body: impl Into<String>) {
    let _ = out.blocking_send(NodeToControl::ChatMessage {
        session_id,
        role: role.to_string(),
        body: body.into(),
    });
}

#[cfg(test)]
mod tests {
    /// A chat launch must NOT pin this session's own id (MAIN-502).
    ///
    /// Verified against the pinned CLI: `--session-id` on an id that already
    /// has a session file exits 1 with *"Session ID … is already in use"*
    /// before writing anything to stdout. Using the session id therefore works
    /// exactly once — the failure only appears on RESTART, which is both the
    /// least-tested path and the one a person reaches for when something is
    /// already wrong. A source guard rather than a behavioural test because
    /// the alternative needs a logged-in agent and a real second launch, and
    /// the regression is a single argument being passed.
    #[test]
    fn the_agent_id_is_minted_per_launch_never_the_session_id() {
        let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/chat.rs"))
            .expect("this file must be readable");
        // Only the code: the banned expression is spelled out below, so a
        // whole-file scan would find its own guard and fail forever.
        let code = src
            .split("#[cfg(test)]")
            .next()
            .expect("code precedes tests");
        assert!(
            !code.contains("claude_chat_args(&session_id"),
            "the launch is pinning the session's own id again — restart will \
             die with \"Session ID is already in use\""
        );
        assert!(
            code.contains("uuid::Uuid::now_v7()"),
            "a fresh agent id per launch is what makes restart possible"
        );
    }

    /// MAIN-620 AC-3, end to end against a stand-in agent: "allow always"
    /// settles the TOOL, not the request.
    ///
    /// Behavioural rather than a source guard, because the property is about
    /// ordering — the tool has to be remembered before the answer that unblocks
    /// the agent is written, or the very next request races the set and prompts
    /// anyway. A stand-in `sh` agent makes that ordering observable: it asks,
    /// waits for the response on stdin, and asks again about the same tool the
    /// moment it has one.
    #[tokio::test]
    async fn allow_always_answers_the_same_tool_without_asking_again() {
        use nook_proto::NodeToControl;
        use nook_types::SessionId;
        use std::io::Write;

        fn request(id: &str, tool: &str) -> String {
            format!(
                r#"{{"type":"control_request","request_id":"{id}","request":{{"subtype":"can_use_tool","tool_name":"{tool}","input":{{}},"description":"{id}"}}}}"#
            )
        }

        let dir = std::env::temp_dir().join(format!("nook-chat-always-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).expect("a scratch checkout");
        let answers = dir.join("answers.jsonl");
        let runtime = dir.join("agent.sh");
        let mut f = std::fs::File::create(&runtime).expect("the stand-in agent");
        write!(
            f,
            "#!/bin/sh\nprintf '%s\\n' '{first}'\nread -r a; printf '%s\\n' \"$a\" >> '{answers}'\nprintf '%s\\n' '{second}'\nread -r b; printf '%s\\n' \"$b\" >> '{answers}'\nwhile :; do sleep 30; done\n",
            first = request("r-1", "Bash"),
            second = request("r-2", "Bash"),
            answers = answers.display(),
        )
        .expect("write the stand-in");
        drop(f);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755))
                .expect("make it executable");
        }

        let (ctl_tx, mut ctl_rx) = tokio::sync::mpsc::channel(32);
        let session_id = SessionId::new();
        let handle = super::start(&ctl_tx, session_id, &runtime.to_string_lossy(), &dir, &[])
            .expect("the stand-in agent starts");

        // The FIRST request reaches the person, because nothing is remembered
        // yet — this is the ordinary MAIN-502 exchange.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut asked = Vec::new();
        while let Ok(Some(msg)) = tokio::time::timeout_at(deadline, ctl_rx.recv()).await {
            if let NodeToControl::ChatPermission { request_id, .. } = msg {
                asked.push(request_id);
                break;
            }
        }
        assert_eq!(asked, vec!["r-1".to_string()], "the first ask is announced");

        handle.decide("r-1", true, true).expect("answer it");

        // …and the SECOND never does. The agent asks about `Bash` again the
        // instant it is unblocked; both answers landing in the file is what
        // proves it was answered rather than merely ignored.
        let until = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut both_answered = false;
        while tokio::time::Instant::now() < until {
            if let Ok(text) = std::fs::read_to_string(&answers) {
                if text.contains("\"r-2\"") {
                    both_answered = true;
                    break;
                }
            }
            if let Ok(Some(NodeToControl::ChatPermission { request_id, .. })) =
                tokio::time::timeout(std::time::Duration::from_millis(200), ctl_rx.recv()).await
            {
                panic!("a remembered tool asked again: {request_id}");
            }
        }
        handle.kill();
        assert!(
            both_answered,
            "the second request must be answered by the node, not announced"
        );
        let text = std::fs::read_to_string(&answers).unwrap_or_default();
        assert!(
            text.matches("\"behavior\":\"allow\"").count() >= 2,
            "both were allowed: {text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
