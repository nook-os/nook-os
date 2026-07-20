//! tmux invocation layer. Plain tmux commands (not control mode): sessions
//! are named `nook_<short session id>` and survive node restarts — tmux is
//! the buffer of record.

use anyhow::{Context, Result};
use std::process::Command;

pub const SESSION_PREFIX: &str = "nook_";

fn tmux(args: &[&str]) -> Result<String> {
    let out = Command::new("tmux")
        .args(args)
        .output()
        .context("tmux not available")?;
    if !out.status.success() {
        anyhow::bail!(
            "tmux {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Live NookOS-managed tmux sessions on this machine.
pub fn list_nook_sessions() -> Vec<String> {
    // tmux exits non-zero when no server is running — that's just "empty".
    Command::new("tmux")
        .args(["ls", "-F", "#{session_name}"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| l.starts_with(SESSION_PREFIX))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

pub fn session_exists(name: &str) -> bool {
    Command::new("tmux")
        .args(["has-session", "-t", name])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Server-wide defaults, applied before every session create (idempotent;
/// `set -g` also reaches existing sessions):
/// - `mouse off` (explicit): mouse mode made tmux intercept wheel/clicks and
///   its copy-mode redraws corrupted full-screen TUIs like Claude Code in the
///   browser terminal. TUIs behave exactly like a plain native terminal with
///   it off. (Scrollback access is a future feature — likely per-viewer tmux
///   clients — not worth breaking TUI rendering for.)
/// - `history-limit`: applies to panes created AFTER it's set, hence here;
///   tmux retains history for when scrollback access lands.
/// - `set-clipboard on`: apps that emit OSC 52 copy into the real clipboard.
pub fn apply_server_defaults() {
    let _ = tmux(&["start-server"]);
    let _ = tmux(&["set-option", "-g", "mouse", "off"]);
    let _ = tmux(&["set-option", "-g", "history-limit", "10000"]);
    let _ = tmux(&["set-option", "-s", "set-clipboard", "on"]);
}

/// Create a detached session running `command` in `cwd`.
pub fn new_session(name: &str, cwd: &str, cols: u16, rows: u16, command: &str) -> Result<()> {
    apply_server_defaults();
    tmux(&[
        "new-session",
        "-d",
        "-s",
        name,
        "-c",
        cwd,
        // Give the session's shell (and whatever runtime it launches) a UTF-8
        // locale. Without it, TUIs like Claude Code detect a non-Unicode
        // terminal and fall back to ASCII art (box corners / bullets become
        // "_"). C.UTF-8 is available everywhere without locale-gen.
        "-e",
        "LANG=C.UTF-8",
        "-e",
        "LC_ALL=C.UTF-8",
        "-x",
        &cols.to_string(),
        "-y",
        &rows.to_string(),
        command,
    ])?;
    // Keep the pane around briefly on exit? No — session death IS the exit
    // signal. But do stop tmux from renaming sessions under us.
    let _ = tmux(&["set-option", "-t", name, "allow-rename", "off"]);
    // Follow the most-recently-attached client's size, and reflow the window
    // to it rather than to the smallest client — so a browser resize wins.
    let _ = tmux(&["set-option", "-t", name, "window-size", "latest"]);
    let _ = tmux(&["set-window-option", "-t", name, "aggressive-resize", "on"]);
    Ok(())
}

pub fn kill_session(name: &str) -> Result<()> {
    tmux(&["kill-session", "-t", name])?;
    Ok(())
}

/// Capture a session's pane as plain text: the visible screen plus up to
/// `history_lines` of scrollback above it. Joined wrapped lines (-J) so long
/// commands read naturally.
pub fn capture_pane(name: &str, history_lines: u32) -> Result<String> {
    tmux(&[
        "capture-pane",
        "-p",
        "-J",
        "-t",
        name,
        "-S",
        &format!("-{history_lines}"),
    ])
}

/// Force tmux to fully repaint the client attached to `session` — a proper
/// cursor-addressed redraw through the existing PTY, so a (re)connecting
/// browser gets a coherent screen instead of mid-stream deltas.
pub fn repaint(session: &str) {
    let ttys = Command::new("tmux")
        .args(["list-clients", "-t", session, "-F", "#{client_tty}"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    for tty in ttys.lines().filter(|t| !t.is_empty()) {
        let _ = Command::new("tmux")
            .args(["refresh-client", "-t", tty])
            .output();
    }
}
