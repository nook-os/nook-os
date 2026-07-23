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
/// - `mouse on` + explicit wheel bindings: without mouse mode tmux ignores the
///   wheel, and the browser terminal falls back to xterm's "alternate scroll"
///   emulation — translating the wheel into arrow keys. In a shell that means
///   scrolling silently walks your command history instead of scrolling.
///   Turning mouse mode on alone is not enough either: bare `mouse on` makes
///   the wheel yank a full-screen TUI away and drop you into copy-mode showing
///   pre-app scrollback. So the wheel is bound by context (see below).
/// - `history-limit`: applies to panes created AFTER it's set, hence here.
/// - `set-clipboard on`: apps that emit OSC 52 copy into the real clipboard.
pub fn apply_server_defaults() {
    let _ = tmux(&["start-server"]);
    let _ = tmux(&["set-option", "-g", "mouse", "on"]);
    let _ = tmux(&["set-option", "-g", "history-limit", "10000"]);
    let _ = tmux(&["set-option", "-s", "set-clipboard", "on"]);

    // Wheel policy, in priority order:
    //   1. the app asked for mouse reporting (Claude Code, `vim -c 'set
    //      mouse=a'`) -> forward the event; the app scrolls itself.
    //   2. otherwise a full-screen app on the alternate screen (less, vim)
    //      -> arrow keys, which is what a native terminal sends and what the
    //      app expects. Crucially NOT copy-mode, which would replace the TUI.
    //   3. otherwise a normal shell -> enter copy-mode and scroll the real
    //      scrollback, which is what "scroll up in my terminal" should mean.
    for (key, arrow, down_fallback) in WHEEL_KEYS {
        let args = wheel_binding(key, arrow, down_fallback);
        let _ = tmux(&args.iter().map(String::as_str).collect::<Vec<_>>());
    }
}

/// The two wheel directions and what each does when there is no full-screen app:
/// scrolling up has to ENTER copy-mode first, scrolling down is already in it.
const WHEEL_KEYS: [(&str, &str, &str); 2] = [
    ("WheelUpPane", "Up", "copy-mode -e; send-keys -M"),
    ("WheelDownPane", "Down", "send-keys -M"),
];

/// Build one wheel binding. Split out from the tmux call so the policy can be
/// asserted without a running server — the ordering of these three branches is
/// the whole fix, and it is not something you can check by reading it back.
fn wheel_binding(key: &str, arrow: &str, no_app_fallback: &str) -> Vec<String> {
    let alt_branch =
        format!("if -Ft= \"#{{alternate_on}}\" \"send-keys -N 3 {arrow}\" \"{no_app_fallback}\"");
    [
        "bind-key",
        "-n",
        key,
        "if-shell",
        "-F",
        "-t",
        "=",
        "#{mouse_any_flag}",
        "send-keys -M",
    ]
    .iter()
    .map(|s| s.to_string())
    .chain(std::iter::once(alt_branch))
    .collect()
}

/// Apply the defaults unless this server already has them.
///
/// Startup and session-create are not enough on their own. `set -g` lives in
/// the tmux SERVER, and a server can outlive — or predate — the agent that
/// configured it: one started by the user's own `tmux`, or by an older agent
/// from before the wheel bindings existed, keeps running with no mouse mode.
/// Every symptom of that is the same one, "the wheel types arrow keys", because
/// with mouse reporting off the browser terminal falls back to xterm's
/// alternate-scroll emulation before tmux ever sees the event.
///
/// So it is re-checked whenever a viewer attaches. The check reads the BINDING
/// rather than the `mouse` option: a server can have `mouse on` from someone's
/// `.tmux.conf` and still lack the context-aware wheel policy, and that
/// combination scrolls just as wrongly.
///
/// The marker has to be one only OUR binding contains. tmux ships its own
/// default `WheelUpPane` that also consults `#{mouse_any_flag}`, so checking
/// for that would match a stock server and this would never fire. `send-keys
/// -N 3` is ours alone — stock tmux drops an alt-screen app into copy-mode
/// instead, which is the behaviour being replaced.
const WHEEL_MARKER: &str = "send-keys -N 3";

pub fn ensure_server_defaults() {
    let configured = tmux(&["list-keys", "-T", "root", "WheelUpPane"])
        .is_ok_and(|out| out.contains(WHEEL_MARKER));
    if !configured {
        apply_server_defaults();
    }
}

/// The user's shell, for launching sessions as login shells.
pub fn login_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "/bin/bash".to_string())
}

/// Wrap a runtime so it runs inside a login+interactive shell.
///
/// A node started by systemd (or any service manager) has a bare PATH —
/// nothing from ~/.profile, ~/.bashrc, asdf, nvm, ~/.local/bin. Terminals
/// that don't see the user's own tools aren't terminals, so sessions start
/// the way a desktop terminal emulator starts them: as a login shell.
/// `-i` matters too, since most PATH setup lives in ~/.bashrc.
pub fn login_command(runtime: &str) -> String {
    let shell = login_shell();
    let is_shell = matches!(
        runtime,
        "bash" | "zsh" | "sh" | "fish" | "pwsh" | "dash" | "ksh"
    );
    if is_shell {
        // Login shells source the profile themselves.
        match runtime {
            "pwsh" => runtime.to_string(),
            _ => format!("{runtime} -l"),
        }
    } else {
        // Runtimes (claude/hermes/codex) inherit the sourced environment.
        // exec keeps the pane bound to the runtime, so quitting it ends the
        // window rather than dropping to a stray shell.
        format!("{shell} -l -i -c 'exec {runtime}'")
    }
}

/// Can the login shell actually find this runtime?
///
/// `which` alone lies here: a node started by systemd has a bare PATH, so a
/// runtime installed in ~/.local/bin looks missing while the login shell we
/// launch it with finds it fine. Resolve it exactly the way we run it.
pub fn runtime_available(runtime: &str) -> bool {
    let shell = login_shell();
    Command::new(&shell)
        .args(["-l", "-i", "-c", &format!("command -v {runtime}")])
        .output()
        .is_ok_and(|o| o.status.success() && !o.stdout.is_empty())
}

pub fn new_session(name: &str, cwd: &str, cols: u16, rows: u16, command: &str) -> Result<()> {
    // Preflight, so the failure names its own cause. tmux's own message for a
    // missing -c directory is terse and arrives with no session attached, and
    // a runtime that isn't installed dies so fast it just looks like the
    // terminal never opened.
    if !std::path::Path::new(cwd).is_dir() {
        anyhow::bail!("checkout {cwd} does not exist on this node");
    }
    if !runtime_available(command) {
        anyhow::bail!("runtime '{command}' is not installed on this node");
    }
    apply_server_defaults();
    let launch = login_command(command);
    let command = launch.as_str();
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
    // New terminals in this session (tmux windows) get a login shell too.
    let _ = tmux(&[
        "set-option",
        "-t",
        name,
        "default-command",
        &format!("{} -l", login_shell()),
    ]);
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

/// A terminal inside a session: tmux calls these windows. One tmux session
/// can hold many, and the attached client renders whichever is active — so
/// "more terminals in this session" is just window management.
pub fn list_windows(session: &str) -> Result<String> {
    // The separator has to be PRINTABLE. tmux rewrites control characters in
    // `-F` output — a \u{1} delimiter comes back as a literal "_", which made
    // every line unparseable and this function silently return "[]". An empty
    // window list looks like a session with no terminals, which is why the
    // strip that closes one terminal never appeared.
    //
    // `name` goes last precisely because it's the one field a user controls:
    // a window called "foo|bar" then lands in the name and parses fine.
    let raw = tmux(&[
        "list-windows",
        "-t",
        session,
        "-F",
        "#{window_index}|#{window_active}|#{window_panes}|#{window_name}",
    ])?;
    let windows: Vec<serde_json::Value> = raw
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(4, '|');
            Some(serde_json::json!({
                "index": parts.next()?.parse::<u32>().ok()?,
                "active": parts.next()? == "1",
                "panes": parts.next()?.parse::<u32>().unwrap_or(1),
                "name": parts.next().unwrap_or("shell"),
            }))
        })
        .collect();
    Ok(serde_json::to_string(&windows).unwrap_or_else(|_| "[]".into()))
}

/// Where the session is currently working — new terminals should open in the
/// workspace, not the user's home directory.
pub fn session_cwd(session: &str) -> Option<String> {
    tmux(&[
        "display-message",
        "-p",
        "-t",
        session,
        "#{pane_current_path}",
    ])
    .ok()
    .filter(|p| !p.is_empty())
}

/// Open another terminal in this session and focus it. Without an explicit
/// directory it inherits the session's current one (tmux would otherwise drop
/// you in $HOME, which is never what you want in a workspace).
pub fn new_window(session: &str, cwd: Option<&str>) -> Result<()> {
    let inherited = cwd.map(str::to_string).or_else(|| session_cwd(session));
    let mut args = vec!["new-window", "-t", session];
    if let Some(dir) = inherited.as_deref() {
        args.push("-c");
        args.push(dir);
    }
    tmux(&args)?;
    Ok(())
}

/// Split the active window, so two terminals are visible at once.
pub fn split_window(session: &str, vertical: bool) -> Result<()> {
    let cwd = session_cwd(session);
    // tmux's -h splits into left/right; -v stacks top/bottom.
    let mut args = vec![
        "split-window",
        if vertical { "-v" } else { "-h" },
        "-t",
        session,
    ];
    if let Some(dir) = cwd.as_deref() {
        args.push("-c");
        args.push(dir);
    }
    tmux(&args)?;
    Ok(())
}

pub fn select_window(session: &str, index: u32) -> Result<()> {
    tmux(&["select-window", "-t", &format!("{session}:{index}")])?;
    Ok(())
}

pub fn kill_window(session: &str, index: u32) -> Result<()> {
    tmux(&["kill-window", "-t", &format!("{session}:{index}")])?;
    Ok(())
}

pub fn rename_window(session: &str, index: u32, name: &str) -> Result<()> {
    tmux(&["rename-window", "-t", &format!("{session}:{index}"), name])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wheel policy, in the order that matters. Getting this order wrong
    /// does not fail loudly — it just means scrolling types arrow keys into
    /// someone's shell, which reads as "the terminal is broken".
    #[test]
    fn the_wheel_prefers_the_app_then_the_screen_then_scrollback() {
        for (key, arrow, fallback) in WHEEL_KEYS {
            let args = wheel_binding(key, arrow, fallback);
            let joined = args.join(" ");

            // 1. an app that asked for mouse reporting gets the raw event.
            let mouse_at = joined.find("#{mouse_any_flag}").expect("checks mouse flag");
            // 2. otherwise a full-screen app gets arrows, NOT copy-mode, which
            //    would replace the TUI with pre-app scrollback.
            let alt_at = joined
                .find("#{alternate_on}")
                .expect("checks alternate screen");
            assert!(
                mouse_at < alt_at,
                "mouse reporting must win over alt-screen: {joined}"
            );
            assert!(
                joined.contains(&format!("send-keys -N 3 {arrow}")),
                "alt-screen branch sends {arrow}: {joined}"
            );
            // 3. and a plain shell scrolls its real history.
            assert!(joined.ends_with(&format!("\"{fallback}\"")), "{joined}");
        }
        // Only scrolling UP enters copy-mode; binding it on the way down too
        // would drop you into copy-mode when you scroll back to the bottom.
        assert!(wheel_binding("WheelUpPane", "Up", WHEEL_KEYS[0].2)
            .join(" ")
            .contains("copy-mode"));
        assert!(!wheel_binding("WheelDownPane", "Down", WHEEL_KEYS[1].2)
            .join(" ")
            .contains("copy-mode"));
    }

    /// `ensure_server_defaults` decides by reading the binding back, so the
    /// marker must do two things: match what we write, and NOT match what tmux
    /// ships. Only asserting the first is how the check silently became a
    /// no-op — tmux's own default binding consults `#{mouse_any_flag}` too, so
    /// a marker of that matched every stock server and never reapplied.
    #[test]
    fn the_marker_tells_our_binding_apart_from_tmuxs_own() {
        let ours = wheel_binding("WheelUpPane", "Up", WHEEL_KEYS[0].2).join(" ");
        assert!(ours.contains(WHEEL_MARKER), "must match ours: {ours}");

        // Verbatim from `tmux list-keys -T root WheelUpPane` on a stock server.
        let stock = r##"bind-key -T root WheelUpPane if-shell -F "#{||:#{pane_in_mode},#{mouse_any_flag}}" "send -M" "copy-mode -e""##;
        assert!(
            !stock.contains(WHEEL_MARKER),
            "must NOT match tmux's default, or an unconfigured server looks configured"
        );
    }
}
