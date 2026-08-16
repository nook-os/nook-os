//! Session engine: one tmux session per NookOS session, streamed through a
//! persistent `tmux attach` running inside a portable-pty PTY.
//!
//! The PTY attach client stays alive whether or not anyone is watching —
//! tmux keeps the session; browsers attach and detach freely upstream.
//!
//! The manager runs on its OWN thread, fed by [`Cmd`]s from the connection's
//! read loop. That thread placement is load-bearing: start/attach/kill spawn
//! tmux subprocesses, and when they ran inline on the WebSocket read loop a
//! reconcile burst (repeated `StartSession` for a spec whose checkout is
//! missing) stalled the loop for the duration of every spawn — during which
//! `SessionInput` sat unread and the terminal froze mid-keystroke. One
//! channel, one thread: command order is preserved, and the read loop never
//! waits on a subprocess.

use anyhow::{Context, Result};
use base64::Engine;
use nook_proto::NodeToControl;
use nook_types::SessionId;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::collections::HashMap;
use std::io::{Read, Write};
use tokio::sync::mpsc::Sender;

use crate::tmux;

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

struct SessionHandle {
    tmux_name: String,
    master: Box<dyn MasterPty + Send>,
    input_tx: std::sync::mpsc::Sender<Vec<u8>>,
    /// With no viewers attached the PTY is still read (exit detection) but
    /// output frames are not forwarded — N idle sessions cost ~zero bandwidth.
    forward: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Cleared when the reader thread ends. A handle whose PTY has died must
    /// not be reused: attaching to it would silently swallow input.
    alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// Everything the connection asks of the session engine, in arrival order.
pub enum Cmd {
    Start {
        session_id: SessionId,
        runtime: String,
        cwd: String,
        cols: u16,
        rows: u16,
        ports: Vec<nook_types::LeasedPort>,
        /// Declared listeners this session did not get (MAIN-377).
        unsatisfied: Vec<String>,
        /// Exported into the session so the git-ssh shim can name its repo
        /// without a session lookup (MAIN-367). `None` for an ad-hoc terminal.
        workspace_id: Option<String>,
        /// The tenant this session's workspace belongs to, exported as
        /// `NOOK_TENANT_ID` so `nook` inside it is already scoped. The
        /// WORKSPACE's tenant, not this node's — they differ under
        /// cross-tenant placement, which is the case it exists for.
        tenant_id: Option<String>,
    },
    /// Start a session as a CHAT (MAIN-502): the runtime driven through the
    /// streaming adapter, with no tmux anywhere. Its own command rather than a
    /// flag on `Start`, because the two share no step after "which directory"
    /// — and because a chat that fell through to the tmux path by accident
    /// would look like it worked.
    StartChat {
        session_id: SessionId,
        runtime: String,
        cwd: String,
        ports: Vec<nook_types::LeasedPort>,
        unsatisfied: Vec<String>,
        workspace_id: Option<String>,
        tenant_id: Option<String>,
    },
    /// A human turn for a chat session's agent.
    ChatMessage {
        session_id: SessionId,
        text: String,
    },
    /// A human's verdict on a permission that agent is blocked on, and
    /// whether it stands for the rest of the session (MAIN-620 AC-3).
    ChatPermission {
        session_id: SessionId,
        request_id: String,
        allow: bool,
        remember: bool,
    },
    StartAuth {
        session_id: SessionId,
        runtime: String,
        cols: u16,
        rows: u16,
    },
    Attach {
        session_id: SessionId,
        tmux_session: Option<String>,
    },
    Input {
        session_id: SessionId,
        data_b64: String,
    },
    Resize {
        session_id: SessionId,
        cols: u16,
        rows: u16,
    },
    Kill {
        session_id: SessionId,
    },
    Detach {
        session_id: SessionId,
    },
}

pub struct Manager {
    /// Lifecycle messages go out with `blocking_send`, NEVER `try_send`.
    ///
    /// `SessionStarted` carries the tmux name — the only way the control plane
    /// ever learns which tmux session backs a row. `try_send` drops it when the
    /// channel is full and `let _ =` hid that: under load the node would create
    /// the tmux session, attach its PTY, and report nothing, leaving a row with
    /// a NULL `tmux_session` that no viewer could ever attach to. Nothing
    /// re-sends it, so the session was broken for good. Output frames are the
    /// droppable ones and they already block; losing the identity of a session
    /// to save a few milliseconds on a saturated link is the wrong trade.
    ///
    /// Blocking here is safe in a way it would not have been before MAIN-362:
    /// this is the manager's own thread, not the connection's read loop, so a
    /// backed-up socket delays session bookkeeping and nothing else.
    ///
    /// The CONTROL lane, not the output one: lifecycle must not queue behind
    /// megabytes of somebody's `cat` (MAIN-371).
    ctl: Sender<NodeToControl>,
    /// Terminal output frames, and only those. Bulky, constant, and the one
    /// thing on this connection that can be a few milliseconds late without
    /// anyone noticing — which is exactly why it gets its own queue.
    frames: Sender<NodeToControl>,
    sessions: HashMap<SessionId, SessionHandle>,
    /// Live CHAT agents (MAIN-502), beside the tmux handles rather than in
    /// their own manager: a session is one of the two, never both, and one
    /// command queue is what keeps "start it, then send it a message" in that
    /// order. `Kill` looks in both maps for the same reason.
    chats: HashMap<SessionId, crate::chat::ChatHandle>,
}

impl Manager {
    pub fn new(frames: Sender<NodeToControl>, ctl: Sender<NodeToControl>) -> Self {
        Self {
            ctl,
            frames,
            sessions: HashMap::new(),
            chats: HashMap::new(),
        }
    }

    /// The manager's thread. Ends when the connection drops its sender, which
    /// is the connection ending — a reconnect builds a fresh one, exactly as
    /// it built a fresh `Manager` before.
    pub fn spawn(
        frames: Sender<NodeToControl>,
        ctl: Sender<NodeToControl>,
    ) -> std::sync::mpsc::Sender<Cmd> {
        let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
        std::thread::spawn(move || {
            let mut manager = Manager::new(frames, ctl);
            while let Ok(cmd) = rx.recv() {
                manager.handle(cmd);
            }
        });
        tx
    }

    /// The connection ended, taking this manager with it. A tmux session
    /// outlives that by design — it is detached, and the next connection finds
    /// it again by name — but a chat agent is a plain child of this process
    /// with nothing left holding it (MAIN-502).
    ///
    /// Leaving it alive orphans it for good. `Register` on the very next
    /// connect force-exits the row, because a chat's `tmux_session` is
    /// permanently NULL and `expire_sessions_missing_from_tmux` ends every live
    /// session that is not in the node's tmux list; the orphan sweep beside it
    /// only recognises `nook_` tmux names, so nothing ever reaps the process.
    /// It would keep the checkout busy and stay bound to leased ports that the
    /// allocator, seeing a non-live session, is now free to hand to somebody
    /// else — one stray agent per reconnect, per chat session.
    ///
    /// So the agents die with the connection and the exited row is TRUE. A
    /// reconnect starts a fresh agent, which is NG-2's model anyway: a chat's
    /// durable history is the control plane's rows, not the process.
    ///
    /// Reached through `Drop` rather than a call at the end of the command
    /// loop, so that a panic inside `handle` — which unwinds straight past any
    /// such call — cannot orphan them either.
    fn kill_live_chats(&mut self) {
        for (session_id, handle) in self.chats.drain() {
            tracing::info!(
                session = %session_id.0,
                "connection ended — killing the chat agent so the session's exit is real"
            );
            handle.kill();
        }
    }

    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Start {
                session_id,
                runtime,
                cwd,
                cols,
                rows,
                ports,
                unsatisfied,
                workspace_id,
                tenant_id,
            } => self.start(
                session_id,
                &runtime,
                &cwd,
                cols,
                rows,
                &ports,
                &unsatisfied,
                workspace_id.as_deref(),
                tenant_id.as_deref(),
            ),
            Cmd::StartChat {
                session_id,
                runtime,
                cwd,
                ports,
                unsatisfied,
                workspace_id,
                tenant_id,
            } => self.start_chat(
                session_id,
                &runtime,
                &cwd,
                &ports,
                &unsatisfied,
                workspace_id.as_deref(),
                tenant_id.as_deref(),
            ),
            Cmd::ChatMessage { session_id, text } => self.chat_message(session_id, &text),
            Cmd::ChatPermission {
                session_id,
                request_id,
                allow,
                remember,
            } => self.chat_permission(session_id, &request_id, allow, remember),
            Cmd::StartAuth {
                session_id,
                runtime,
                cols,
                rows,
            } => self.start_auth(session_id, &runtime, cols, rows),
            Cmd::Attach {
                session_id,
                tmux_session,
            } => self.attach(session_id, tmux_session.as_deref()),
            Cmd::Input {
                session_id,
                data_b64,
            } => self.input(session_id, &data_b64),
            Cmd::Resize {
                session_id,
                cols,
                rows,
            } => self.resize(session_id, cols, rows),
            Cmd::Kill { session_id } => self.kill(session_id),
            Cmd::Detach { session_id } => self.detach(session_id),
        }
    }

    /// A named session failed to start. Reported against the session id so the
    /// control plane can mark that row errored — an unnamed `Error` only ever
    /// reached the activity log, which left the terminal spinning on
    /// "starting" with no way to learn why.
    fn session_failed(&self, session_id: SessionId, message: String) {
        tracing::warn!(%session_id, %message, "session failed to start");
        let _ = self.ctl.blocking_send(NodeToControl::SessionFailed {
            session_id,
            message,
        });
    }

    // Matches `new_job_session`'s precedent in tmux.rs: these are the fields of
    // one session-start request, already grouped as `Cmd::Start` on the wire.
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        &mut self,
        session_id: SessionId,
        runtime: &str,
        workspace_path: &str,
        cols: u16,
        rows: u16,
        ports: &[nook_types::LeasedPort],
        unsatisfied: &[String],
        workspace_id: Option<&str>,
        tenant_id: Option<&str>,
    ) {
        // The runtime string is the executable to launch. Restrict to the
        // known set so the control plane can't run arbitrary commands.
        if !crate::capabilities::KNOWN_RUNTIMES.contains(&runtime) {
            return self.session_failed(session_id, format!("unknown runtime '{runtime}'"));
        }
        // A skill this agent has never heard of makes `/nook-review` ordinary
        // prose: the loop would "start" and then sit in the checkout doing
        // nothing, forever, looking healthy from every angle. Refuse instead —
        // the same call `loop_job` makes, and for the same reason. The
        // reconciler retries next pass, exactly as it does for a missing
        // runtime, so a node that gets the skill converges without help.
        // …and a review loop with no GitHub credential is worse than one with
        // no skill, because it does not look broken (MAIN-448 AC-7). `gh` with
        // no token lists nothing, so every PR reads as "nothing to review" and
        // the agent reports a clean pass forever. That was already
        // indistinguishable from a quiet repo; MAIN-448 makes a quiet repo a
        // real, expected, scaled-to-zero state, so without this guard the two
        // become impossible to tell apart at all.
        //
        // The same predicate `loop_job` refuses a review JOB with, so a node
        // cannot pass one check and fail the other.
        let tmux_name = format!("{}{}", tmux::SESSION_PREFIX, session_id.0.simple());
        // Restart of an ended session: discard the old PTY before re-attaching.
        self.sessions.remove(&session_id);

        // Whether WE created the tmux session, which is the only case that may
        // be driven: re-attaching to a live review loop must not type a second
        // `/loop` into an agent already running one.
        let fresh = !tmux::session_exists(&tmux_name);
        if fresh {
            // The canonical (hyphenated) uuid, not the tmux name's simple form,
            // so `GET /api/v1/sessions/{id}` inside the session resolves.
            let sid = session_id.0.to_string();
            let extra: Vec<(&str, &str)> = Vec::new();
            if let Err(e) = tmux::new_session(
                &tmux_name,
                workspace_path,
                cols,
                rows,
                runtime,
                &sid,
                ports,
                unsatisfied,
                workspace_id,
                tenant_id,
                &extra,
            ) {
                return self.session_failed(session_id, e.to_string());
            }
        }
        match self.attach_pty(session_id, &tmux_name, cols, rows, false) {
            Ok(()) => {
                let _ = self.ctl.blocking_send(NodeToControl::SessionStarted {
                    session_id,
                    tmux_session: tmux_name,
                });
            }
            Err(e) => self.session_failed(session_id, e.to_string()),
        }
    }

    /// Start a session as a CHAT (MAIN-502): the runtime driven through the
    /// streaming adapter, no tmux, no PTY.
    ///
    /// The environment is assembled to the same recipe `tmux::new_session`
    /// exports — the leased ports under the names the WORKSPACE declared, the
    /// unsatisfied list, the session/workspace/tenant ids, the git-ssh shim and
    /// its id, the fleet's `GH_TOKEN`. An agent must not be able to tell which
    /// surface it is being driven through, or a skill that works in a terminal
    /// quietly fails in a chat.
    #[allow(clippy::too_many_arguments)]
    pub fn start_chat(
        &mut self,
        session_id: SessionId,
        runtime: &str,
        cwd: &str,
        ports: &[nook_types::LeasedPort],
        unsatisfied: &[String],
        workspace_id: Option<&str>,
        tenant_id: Option<&str>,
    ) {
        // The same allowlist a terminal session goes through: the runtime
        // string is an executable to launch, and the wire never chooses one.
        if !crate::capabilities::KNOWN_RUNTIMES.contains(&runtime) {
            return self.session_failed(session_id, format!("unknown runtime '{runtime}'"));
        }
        // …plus the one this surface adds. A runtime with no streaming adapter
        // handed `--input-format stream-json` does not fail cleanly; it fails
        // as a shell that swallowed a flag and then sat there. The control
        // plane refuses this too (AC-2) — this is the machine's own answer,
        // for a request that reached it anyway.
        if !nook_types::runtime_supports_chat(runtime) {
            return self.session_failed(
                session_id,
                format!(
                    "'{runtime}' cannot run as a chat — it does not speak a streaming protocol"
                ),
            );
        }
        if !std::path::Path::new(cwd).is_dir() {
            return self.session_failed(
                session_id,
                format!("checkout {cwd} does not exist on this node"),
            );
        }
        // Restart of an ended chat: drop the old agent before starting another,
        // or the previous process keeps a stdin nobody writes to and a stdout
        // still appending to this session's conversation.
        if let Some(old) = self.chats.remove(&session_id) {
            old.kill();
        }

        let sid = session_id.0.to_string();
        let port_s: Vec<(String, String)> = ports
            .iter()
            .map(|p| (p.env.clone(), p.port.to_string()))
            .collect();
        let skipped = unsatisfied.join(",");
        let mut env: Vec<(&str, &str)> = vec![
            ("NOOK_SESSION_ID", sid.as_str()),
            ("LANG", "C.UTF-8"),
            ("LC_ALL", "C.UTF-8"),
        ];
        for (k, v) in &port_s {
            env.push((k.as_str(), v.as_str()));
        }
        if !skipped.is_empty() {
            env.push(("NOOK_PORTS_UNSATISFIED", skipped.as_str()));
        }
        let gh;
        if let Some(t) = crate::config::fleet_gh_token() {
            gh = t;
            env.push(("GH_TOKEN", gh.as_str()));
        }
        if let Some(w) = workspace_id {
            env.push(("NOOK_WORKSPACE_ID", w));
            // The other half of MAIN-367's mechanism — the shim is useless
            // without the id, and the id feeds nothing without the shim. The
            // two travel together in every session constructor, which is what
            // `tmux.rs`'s guard test is about.
            env.push(("GIT_SSH_COMMAND", "nook get workspace git-ssh"));
        }
        if let Some(t) = tenant_id {
            env.push(("NOOK_TENANT_ID", t));
        }

        match crate::chat::start(
            &self.ctl,
            session_id,
            runtime,
            std::path::Path::new(cwd),
            &env,
        ) {
            Ok(handle) => {
                self.chats.insert(session_id, handle);
                // A chat session has no tmux name — that is the point — so the
                // field it would go in is empty. The control plane stores it as
                // NULL, which is already what it does for a session whose node
                // has not reported one, and nothing attaches a PTY to a chat.
                let _ = self.ctl.blocking_send(NodeToControl::SessionStarted {
                    session_id,
                    tmux_session: String::new(),
                });
            }
            Err(e) => self.session_failed(session_id, e),
        }
    }

    /// Deliver a human turn to a chat session's agent.
    fn chat_message(&mut self, session_id: SessionId, text: &str) {
        let Some(handle) = self.chats.get(&session_id) else {
            // The agent is not on this node — most often because the node
            // restarted since the session started. Said out loud rather than
            // dropped: a message that vanished silently is the one failure a
            // conversation may not have.
            return self.chat_note(
                session_id,
                "this session's agent is not running on this machine any more — restart it",
            );
        };
        if let Err(e) = handle.send(text) {
            self.chat_note(session_id, format!("could not reach the agent: {e}"));
        }
    }

    /// Answer a permission the agent is blocked on.
    fn chat_permission(
        &mut self,
        session_id: SessionId,
        request_id: &str,
        allow: bool,
        remember: bool,
    ) {
        let Some(handle) = self.chats.get(&session_id) else {
            return self.chat_note(
                session_id,
                "this session's agent is not running on this machine any more — restart it",
            );
        };
        if let Err(e) = handle.decide(request_id, allow, remember) {
            self.chat_note(session_id, format!("could not answer the agent: {e}"));
        }
    }

    /// Put a line in the conversation on the node's own behalf.
    fn chat_note(&self, session_id: SessionId, body: impl Into<String>) {
        let _ = self.ctl.blocking_send(NodeToControl::ChatMessage {
            session_id,
            role: "system".into(),
            body: body.into(),
        });
    }

    /// Start a runtime's LOGIN flow in a session (MAIN-126). Like `start`, but
    /// the pane runs the runtime's ALLOWLISTED login command (the node picks it,
    /// never the caller) in the home directory, so a headless node can be
    /// device-authorized from the UI: the code/URL renders and any pasted-back
    /// code reaches the CLI through the ordinary session input path.
    pub fn start_auth(&mut self, session_id: SessionId, runtime: &str, cols: u16, rows: u16) {
        let Some(login_args) = crate::runtime_auth::login_args(runtime) else {
            return self.session_failed(session_id, format!("no login flow known for '{runtime}'"));
        };
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
        let tmux_name = format!("{}{}", tmux::SESSION_PREFIX, session_id.0.simple());
        self.sessions.remove(&session_id);
        if !tmux::session_exists(&tmux_name) {
            let sid = session_id.0.to_string();
            if let Err(e) =
                tmux::new_auth_session(&tmux_name, &home, cols, rows, runtime, login_args, &sid)
            {
                return self.session_failed(session_id, e.to_string());
            }
        }
        match self.attach_pty(session_id, &tmux_name, cols, rows, true) {
            Ok(()) => {
                let _ = self.ctl.blocking_send(NodeToControl::SessionStarted {
                    session_id,
                    tmux_session: tmux_name,
                });
            }
            Err(e) => self.session_failed(session_id, e.to_string()),
        }
    }

    /// Spawn the persistent `tmux attach` PTY and its pump threads. `is_auth`
    /// marks a login/authorize session (MAIN-126): when it exits, the node
    /// re-probes runtime authorization and pushes the fresh set, so the panel
    /// reflects the new state without a reconnect.
    fn attach_pty(
        &mut self,
        session_id: SessionId,
        tmux_name: &str,
        cols: u16,
        rows: u16,
        is_auth: bool,
    ) -> Result<()> {
        if self.sessions.contains_key(&session_id) {
            return Ok(()); // already attached
        }
        // We are about to become this session's client, so anything else
        // attached is a leftover from a previous connection — see
        // `tmux::detach_clients`. Safe here precisely because we hold no handle
        // for this session: every caller either removed it or checked above.
        crate::tmux::detach_clients(tmux_name);
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty failed")?;

        let mut cmd = CommandBuilder::new("tmux");
        // This attach spawns tmux directly through portable_pty, bypassing
        // tmux.rs's `tmux_command()`, so it must carry the node's `-L <socket>`
        // itself — and BEFORE `attach`, since the server flag precedes the
        // subcommand (MAIN-108 AC-2). Absent socket → no flag, the default server.
        if let Some(sock) = crate::tmux::socket_name() {
            cmd.args(["-L", sock]);
        }
        cmd.args(["attach", "-t", tmux_name]);
        // A UTF-8 locale on the attaching client is how modern tmux detects
        // UTF-8 (the old `-u` flag was removed in 2.2) — needed so wide/Unicode
        // glyphs render instead of being replaced with "_".
        cmd.env("TERM", "xterm-256color");
        cmd.env("LANG", "C.UTF-8");
        cmd.env("LC_ALL", "C.UTF-8");
        let mut child = pair
            .slave
            .spawn_command(cmd)
            .context("spawning tmux attach failed")?;

        let mut reader = pair.master.try_clone_reader()?;
        let mut writer = pair.master.take_writer()?;
        let (input_tx, input_rx) = std::sync::mpsc::channel::<Vec<u8>>();

        // Input pump: control plane → PTY.
        std::thread::spawn(move || {
            while let Ok(bytes) = input_rx.recv() {
                if writer.write_all(&bytes).is_err() {
                    break;
                }
                let _ = writer.flush();
            }
        });

        // Output pump: PTY → control plane. EOF means the attach client died,
        // which (almost always) means the tmux session ended. Frames are only
        // forwarded while viewers are attached; the read itself never stops,
        // so exit detection stays live for paused sessions.
        let frames = self.frames.clone();
        let ctl = self.ctl.clone();
        let tmux_name_owned = tmux_name.to_string();
        let forward = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let forward_reader = forward.clone();
        let alive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let alive_reader = alive.clone();
        std::thread::spawn(move || {
            // 4KB chunks: base64 (~5.5KB) + envelope stays under the bus's
            // inline NOTIFY budget, so cross-instance frames never detour
            // through the outbox table.
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if !forward_reader.load(std::sync::atomic::Ordering::Relaxed) {
                            continue;
                        }
                        let frame = NodeToControl::SessionOutput {
                            session_id,
                            data_b64: B64.encode(&buf[..n]),
                        };
                        if frames.blocking_send(frame).is_err() {
                            break;
                        }
                    }
                }
            }
            // This PTY is finished either way — never let it be reused.
            alive_reader.store(false, std::sync::atomic::Ordering::Relaxed);
            let _ = child.wait();
            let exited = !tmux::session_exists(&tmux_name_owned);
            if exited {
                let _ = ctl.blocking_send(NodeToControl::SessionExited {
                    session_id,
                    exit_code: None,
                });
                // An authorize session just ended — re-probe and push the fresh
                // auth state so the panel flips without waiting for a reconnect
                // (MAIN-126 AC-4).
                if is_auth {
                    let profiles = crate::runtime_auth::probe_all();
                    let _ = ctl.blocking_send(NodeToControl::RuntimeAuthStatus { profiles });
                }
            }
        });

        self.sessions.insert(
            session_id,
            SessionHandle {
                tmux_name: tmux_name.to_string(),
                master: pair.master,
                input_tx,
                forward,
                alive,
            },
        );
        Ok(())
    }

    /// Forget a session whose PTY has exited, so the next attach rebuilds it.
    fn drop_if_dead(&mut self, session_id: SessionId) {
        let dead = self
            .sessions
            .get(&session_id)
            .is_some_and(|h| !h.alive.load(std::sync::atomic::Ordering::Relaxed));
        if dead {
            self.sessions.remove(&session_id);
        }
    }

    /// Browser (re)attached: replay the visible screen + recent scrollback.
    /// If the node itself restarted, re-establish the PTY first.
    pub fn attach(&mut self, session_id: SessionId, tmux_name_hint: Option<&str>) {
        // The tmux server may have been started by something other than this
        // agent, or by a version of it that predates the wheel bindings — in
        // which case scrolling types arrow keys. Cheap to re-check, and attach
        // is the moment someone is about to notice.
        tmux::ensure_server_defaults();
        // A handle whose PTY died is worse than none: it accepts input into a
        // closed pipe. Drop it so we re-establish below.
        self.drop_if_dead(session_id);
        if !self.sessions.contains_key(&session_id) {
            let Some(name) = tmux_name_hint.map(str::to_string) else {
                return self
                    .session_failed(session_id, "session is not running on this node".into());
            };
            if !tmux::session_exists(&name) {
                let _ = self.ctl.blocking_send(NodeToControl::SessionExited {
                    session_id,
                    exit_code: None,
                });
                return;
            }
            // Re-establish at the session's creation size; the browser's
            // FitAddon sends a ResizeSession moments later to correct it.
            if let Err(e) = self.attach_pty(session_id, &name, 120, 32, false) {
                return self.session_failed(session_id, e.to_string());
            }
        }
        let Some(handle) = self.sessions.get(&session_id) else {
            return;
        };
        // A viewer is (re)attaching: resume output forwarding.
        handle
            .forward
            .store(true, std::sync::atomic::Ordering::Relaxed);
        // Force a real tmux repaint through the live PTY: the browser gets a
        // clean cursor-addressed full-screen redraw (no stair-stepped plain
        // text, no collision with the delta stream).
        tmux::repaint(&handle.tmux_name);
    }

    /// Last viewer left: stop forwarding output (reads continue for exit
    /// detection). AttachSession resumes.
    pub fn detach(&mut self, session_id: SessionId) {
        if let Some(handle) = self.sessions.get(&session_id) {
            handle
                .forward
                .store(false, std::sync::atomic::Ordering::Relaxed);
        }
    }

    pub fn input(&mut self, session_id: SessionId, data_b64: &str) {
        self.drop_if_dead(session_id);
        let Some(handle) = self.sessions.get(&session_id) else {
            return;
        };
        if let Ok(bytes) = B64.decode(data_b64) {
            let _ = handle.input_tx.send(bytes);
        }
    }

    pub fn resize(&mut self, session_id: SessionId, cols: u16, rows: u16) {
        if let Some(handle) = self.sessions.get(&session_id) {
            let _ = handle.master.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
    }

    pub fn kill(&mut self, session_id: SessionId) {
        // A chat session has no tmux to kill and no PTY to see EOF on — the
        // agent process IS the session — so it is ended here and its reader
        // thread reports the exit as it would for any other death.
        if let Some(handle) = self.chats.remove(&session_id) {
            handle.kill();
            return;
        }
        if let Some(handle) = self.sessions.remove(&session_id) {
            let _ = tmux::kill_session(&handle.tmux_name);
            // Reader thread sees EOF and reports SessionExited.
            return;
        }
        // The node process restarted since this session was attached: no
        // handle, but the tmux session may still be alive. Kill it by its
        // derived name and report the exit ourselves (no reader thread).
        let name = format!("{}{}", tmux::SESSION_PREFIX, session_id.0.simple());
        if tmux::session_exists(&name) {
            let _ = tmux::kill_session(&name);
        }
        let _ = self.ctl.blocking_send(NodeToControl::SessionExited {
            session_id,
            exit_code: None,
        });
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        self.kill_live_chats();
    }
}

#[cfg(test)]
mod tests {
    /// MAIN-455 AC-1. The regression that matters now is the opposite of the
    /// one this module used to guard: a managed session must NOT drive a skill.
    ///
    /// Typing `/loop /nook-review` into an interactive agent is what put an
    /// attachable TUI on a machine, blocked on Claude Code's onboarding prompt
    /// with nobody to answer it, and left nothing to read when it died. Reviews
    /// are headless runs now, and a session is a person's terminal again.
    #[test]
    fn a_session_start_drives_no_skill() {
        let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/sessions.rs"))
            .expect("this file must be readable");
        // Only the code, not this module: the banned words are spelled out
        // below, so a whole-file scan would find its own list and fail forever.
        let code = src
            .split("#[cfg(test)]")
            .next()
            .expect("code precedes tests");
        for banned in ["drive_loop_skill", "drive_skill", "/loop /"] {
            assert!(
                !code.contains(banned),
                "`{banned}` is back — a managed session must never type into an agent"
            );
        }
    }

    /// MAIN-502. The connection ending must take the chat agents with it.
    ///
    /// A chat session's `tmux_session` is permanently NULL, so `Register`'s
    /// reconcile on the very next connect force-exits the row — and the orphan
    /// sweep beside it only knows `nook_` tmux names. An agent left running
    /// there is unreachable forever: still in the checkout, still holding ports
    /// the allocator has already re-let, one more per reconnect. So the row
    /// saying `exited` has to be TRUE, and that is what this asserts.
    ///
    /// A stand-in binary rather than `claude`: the argv is fixed and the point
    /// is the lifecycle, so the script ignores its arguments and refuses to die
    /// on its own — if the manager does not kill it, nothing will and the test
    /// fails on its timeout rather than passing quietly.
    ///
    /// The agent is started through `chat::start` rather than `Cmd::StartChat`
    /// because `start_chat`'s allowlist only launches a name from
    /// `KNOWN_RUNTIMES` — a real guard, and putting a fake `claude` on `PATH`
    /// to get around it would mutate the whole test binary's environment. The
    /// ending is the real one either way: dropping the manager is exactly what
    /// its thread does when the connection goes.
    #[tokio::test]
    async fn ending_the_connection_kills_the_chat_agents() {
        use nook_proto::NodeToControl;
        use nook_types::SessionId;
        use std::io::Write;

        let dir = std::env::temp_dir().join(format!("nook-chat-kill-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).expect("a scratch checkout");
        let runtime = dir.join("agent.sh");
        let mut f = std::fs::File::create(&runtime).expect("the stand-in agent");
        // Never exits on its own, and holds stdout open so the reader thread
        // can only ever see EOF because the process was killed.
        f.write_all(b"#!/bin/sh\nwhile :; do sleep 30; done\n")
            .expect("write the stand-in");
        drop(f);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755))
                .expect("make it executable");
        }

        let (frames_tx, _frames_rx) = tokio::sync::mpsc::channel(32);
        let (ctl_tx, mut ctl_rx) = tokio::sync::mpsc::channel(32);
        let mut manager = super::Manager::new(frames_tx, ctl_tx.clone());

        let session_id = SessionId::new();
        let handle = crate::chat::start(&ctl_tx, session_id, &runtime.to_string_lossy(), &dir, &[])
            .expect("the stand-in agent starts");
        manager.chats.insert(session_id, handle);

        // The connection goes away: the control plane redeployed, or the link
        // blipped. No Kill is sent, which is the whole point — on this path
        // nobody is left to send one. Dropping the manager is exactly what the
        // end of its thread does, panic or not.
        drop(manager);

        // A TOTAL deadline, not a per-message one, and a generous one: this
        // asserts that the agent does not OUTLIVE its connection, which is a
        // statement about eventuality rather than latency. Ten seconds read as
        // a latency bound and made the test fail on loaded CI runners with
        // "the agent outlived its connection" while the kill path was working
        // perfectly — it failed on `main` too, including the 0.6.7 version
        // bump, which contains no session work at all.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
        let mut exited = false;
        while let Ok(Some(msg)) = tokio::time::timeout_at(deadline, ctl_rx.recv()).await {
            if let NodeToControl::SessionExited { session_id: id, .. } = msg {
                assert_eq!(id, session_id, "the session that exited is ours");
                exited = true;
                break;
            }
        }
        assert!(
            exited,
            "the agent outlived its connection — it is orphaned now, holding \
             this session's leased ports with nothing left that could reap it"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The other half of the test above: it has to be reached by DROPPING the
    /// manager, not by a call at the end of the command loop.
    ///
    /// The distinction is the panic. A call sited after the loop is skipped
    /// entirely when `handle` unwinds, which strands exactly the agents this
    /// exists to reap — and that path leaves no trace anyone would connect to a
    /// stray `claude` days later.
    #[test]
    fn the_chat_agents_are_killed_by_dropping_the_manager() {
        let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/sessions.rs"))
            .expect("this file must be readable");
        let code = src
            .split("#[cfg(test)]")
            .next()
            .expect("code precedes tests");
        let (_, after_impl) = code
            .split_once("impl Drop for Manager")
            .expect("the manager still reaps its chat agents on drop");
        assert!(
            after_impl
                .split_once('}')
                .expect("the drop body closes")
                .0
                .contains("kill_live_chats()"),
            "Drop no longer kills the live chat agents — every reconnect now \
             strands one agent per chat session"
        );
    }
}
