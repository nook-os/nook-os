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
        /// What the control plane declared this session FOR (MAIN-326). `None`
        /// — a person's terminal — just opens the runtime, which is everything
        /// this ever did.
        managed_purpose: Option<nook_types::ManagedPurpose>,
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
}

impl Manager {
    pub fn new(frames: Sender<NodeToControl>, ctl: Sender<NodeToControl>) -> Self {
        Self {
            ctl,
            frames,
            sessions: HashMap::new(),
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
                managed_purpose,
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
                managed_purpose,
            ),
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
        managed_purpose: Option<nook_types::ManagedPurpose>,
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
        if let Some(skill) = managed_purpose.and_then(drive_skill) {
            if !crate::wizard::skills::is_installed(skill) {
                return self.session_failed(
                    session_id,
                    format!(
                        "skill {skill} is not installed on this node — the loop skills ship \
                         with the agent binary and are written on `nook run`; this node is \
                         running a build that predates them, or could not write its agent \
                         skill directory"
                    ),
                );
            }
        }
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
            ) {
                return self.session_failed(session_id, e.to_string());
            }
        }
        match self.attach_pty(session_id, &tmux_name, cols, rows, false) {
            Ok(()) => {
                if fresh {
                    if let Some(skill) = managed_purpose.and_then(drive_skill) {
                        drive_loop_skill(&tmux_name, skill);
                    }
                }
                let _ = self.ctl.blocking_send(NodeToControl::SessionStarted {
                    session_id,
                    tmux_session: tmux_name,
                });
            }
            Err(e) => self.session_failed(session_id, e.to_string()),
        }
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

/// The skill a declared purpose drives, if it drives one (MAIN-326).
///
/// A fixed table keyed by purpose — the node chooses the command, never the
/// wire, exactly as it does for a runtime's login flow. `Access` drives nothing:
/// it is a terminal for a person, and typing into it would be typing over them.
fn drive_skill(purpose: nook_types::ManagedPurpose) -> Option<&'static str> {
    match purpose {
        nook_types::ManagedPurpose::Access => None,
        nook_types::ManagedPurpose::ReviewLoop => Some("nook-review"),
    }
}

/// The runtime needs a beat to come up before it can take input. We do not read
/// its output to decide when it is ready — the same constraint `loop_job` works
/// under — so wait a fixed moment, then type.
const SKILL_STARTUP_DELAY: std::time::Duration = std::time::Duration::from_secs(5);

/// Put a declared session to work: `/loop /<skill>`, typed once the runtime is
/// up.
///
/// `/loop` with NO interval, so the agent paces its own iterations — a review
/// pass takes as long as the PRs in front of it take, and a fixed interval would
/// either stack passes on a busy repo or idle on a quiet one.
///
/// On its own thread because the delay must not block the session manager: that
/// thread is what every other terminal on this machine gets its keystrokes
/// through, and five seconds of it is five seconds of a frozen fleet.
fn drive_loop_skill(tmux_name: &str, skill: &'static str) {
    let tmux_name = tmux_name.to_string();
    std::thread::spawn(move || {
        std::thread::sleep(SKILL_STARTUP_DELAY);
        let line = format!("/loop /{skill}");
        match tmux::send_keys(&tmux_name, &line) {
            Ok(()) => tracing::info!(session = %tmux_name, %line, "started the declared loop"),
            // Loud, because the session is now a live agent sitting idle: it
            // looks exactly like a working review loop from the control plane.
            Err(e) => {
                tracing::error!(session = %tmux_name, %line, error = %e, "could not start the declared loop")
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use nook_types::ManagedPurpose;

    /// The node — never the wire — decides what runs, so this table IS the
    /// contract. A purpose that mapped to the wrong skill would put a build
    /// agent on the review loop's node with nobody having asked for one.
    #[test]
    fn only_the_review_loop_drives_a_skill() {
        assert_eq!(drive_skill(ManagedPurpose::ReviewLoop), Some("nook-review"));
        assert_eq!(
            drive_skill(ManagedPurpose::Access),
            None,
            "a person's terminal is theirs — typing into it types over them"
        );
    }
}
