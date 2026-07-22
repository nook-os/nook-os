//! Session engine: one tmux session per NookOS session, streamed through a
//! persistent `tmux attach` running inside a portable-pty PTY.
//!
//! The PTY attach client stays alive whether or not anyone is watching —
//! tmux keeps the session; browsers attach and detach freely upstream.

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

pub struct Manager {
    out: Sender<NodeToControl>,
    sessions: HashMap<SessionId, SessionHandle>,
}

impl Manager {
    pub fn new(out: Sender<NodeToControl>) -> Self {
        Self {
            out,
            sessions: HashMap::new(),
        }
    }

    /// A named session failed to start. Reported against the session id so the
    /// control plane can mark that row errored — an unnamed `Error` only ever
    /// reached the activity log, which left the terminal spinning on
    /// "starting" with no way to learn why.
    fn session_failed(&self, session_id: SessionId, message: String) {
        tracing::warn!(%session_id, %message, "session failed to start");
        let _ = self.out.try_send(NodeToControl::SessionFailed {
            session_id,
            message,
        });
    }

    pub fn start(
        &mut self,
        session_id: SessionId,
        runtime: &str,
        workspace_path: &str,
        cols: u16,
        rows: u16,
    ) {
        // The runtime string is the executable to launch. Restrict to the
        // known set so the control plane can't run arbitrary commands.
        if !crate::capabilities::KNOWN_RUNTIMES.contains(&runtime) {
            return self.session_failed(session_id, format!("unknown runtime '{runtime}'"));
        }
        let tmux_name = format!("{}{}", tmux::SESSION_PREFIX, session_id.0.simple());
        // Restart of an ended session: discard the old PTY before re-attaching.
        self.sessions.remove(&session_id);

        if !tmux::session_exists(&tmux_name) {
            if let Err(e) = tmux::new_session(&tmux_name, workspace_path, cols, rows, runtime) {
                return self.session_failed(session_id, e.to_string());
            }
        }
        match self.attach_pty(session_id, &tmux_name, cols, rows) {
            Ok(()) => {
                let _ = self.out.try_send(NodeToControl::SessionStarted {
                    session_id,
                    tmux_session: tmux_name,
                });
            }
            Err(e) => self.session_failed(session_id, e.to_string()),
        }
    }

    /// Spawn the persistent `tmux attach` PTY and its pump threads.
    fn attach_pty(
        &mut self,
        session_id: SessionId,
        tmux_name: &str,
        cols: u16,
        rows: u16,
    ) -> Result<()> {
        if self.sessions.contains_key(&session_id) {
            return Ok(()); // already attached
        }
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
        let out = self.out.clone();
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
                        if out.blocking_send(frame).is_err() {
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
                let _ = out.blocking_send(NodeToControl::SessionExited {
                    session_id,
                    exit_code: None,
                });
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
        // A handle whose PTY died is worse than none: it accepts input into a
        // closed pipe. Drop it so we re-establish below.
        self.drop_if_dead(session_id);
        if !self.sessions.contains_key(&session_id) {
            let Some(name) = tmux_name_hint.map(str::to_string) else {
                return self
                    .session_failed(session_id, "session is not running on this node".into());
            };
            if !tmux::session_exists(&name) {
                let _ = self.out.try_send(NodeToControl::SessionExited {
                    session_id,
                    exit_code: None,
                });
                return;
            }
            // Re-establish at the session's creation size; the browser's
            // FitAddon sends a ResizeSession moments later to correct it.
            if let Err(e) = self.attach_pty(session_id, &name, 120, 32) {
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
        let _ = self.out.try_send(NodeToControl::SessionExited {
            session_id,
            exit_code: None,
        });
    }
}
