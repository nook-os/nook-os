//! tmux invocation layer. Plain tmux commands (not control mode): sessions
//! are named `nook_<short session id>` and survive node restarts — tmux is
//! the buffer of record.

use anyhow::{Context, Result};
use std::process::Command;
use std::sync::OnceLock;

pub const SESSION_PREFIX: &str = nook_proto::TMUX_SESSION_PREFIX;

/// The tmux server this node talks to, as an `-L <socket>` name (MAIN-108).
/// `None` — the default, and what every unset call sees — means the user's
/// DEFAULT tmux server, byte-identical to pre-108 nodes (AC-1). A configured
/// socket gives the node a PRIVATE server, so two nodes on one host never see or
/// clobber each other's sessions (AC-5). Set once at startup from `NodeConfig`.
static TMUX_SOCKET: OnceLock<Option<String>> = OnceLock::new();

/// Plumb the node's tmux socket, once, before any session work (`main`). Later
/// calls are ignored — the value is process-wide and immutable, so no call site
/// can pick up a different server mid-run.
pub fn set_socket(socket: Option<String>) {
    let _ = TMUX_SOCKET.set(socket);
}

fn socket() -> Option<&'static str> {
    TMUX_SOCKET.get().and_then(|s| s.as_deref())
}

/// This node's tmux socket name, if a private server is configured — for the one
/// caller that spawns tmux OUTSIDE this module and so cannot use `tmux_command()`
/// (the `portable_pty` attach in `sessions.rs`). `None` = the default server.
pub fn socket_name() -> Option<&'static str> {
    socket()
}

/// A `tmux` command carrying the configured `-L <socket>` prefix (if any) ahead
/// of its subcommand. THE one place the socket flag is added (AC-2) — `tmux()`
/// and the three direct `Command` bypasses below all build from here, so a new
/// call site cannot silently land on the wrong server. `-L` is a server option
/// and must precede the tmux command, which is why it is prepended here.
///
/// The documented exceptions (AC-3) deliberately do NOT use this: `nook
/// agent-state`'s in-pane `display-message` inherits `$TMUX` (adding `-L` would
/// point it at the wrong server), and `capabilities.rs`'s `tmux -V` is
/// socket-irrelevant.
fn tmux_command() -> Command {
    let mut cmd = Command::new("tmux");
    cmd.args(socket_args(socket()));
    cmd
}

/// The `-L <socket>` prefix for a socket — empty for `None` (the default
/// server). Pure, so the present/absent construction is unit-tested without a
/// running tmux or touching the process-wide socket.
fn socket_args(socket: Option<&str>) -> Vec<String> {
    match socket {
        Some(s) => vec!["-L".into(), s.into()],
        None => Vec::new(),
    }
}

fn tmux(args: &[&str]) -> Result<String> {
    let out = tmux_command()
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

/// Live NookOS-managed tmux sessions on this machine — on THIS node's server
/// only (AC-5), via the configured socket.
pub fn list_nook_sessions() -> Vec<String> {
    // tmux exits non-zero when no server is running — that's just "empty".
    tmux_command()
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
    tmux_command()
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

#[allow(clippy::too_many_arguments)]
pub fn new_session(
    name: &str,
    cwd: &str,
    cols: u16,
    rows: u16,
    command: &str,
    session_id: &str,
    // The ports the control plane leased this session (MAIN-301), each with
    // the env var its WORKSPACE declared for it — `PORT`, `API_PORT`,
    // `NOOK_PORT`, whatever the repo asked for. This end neither chooses nor
    // recognises the names; it exports what it was handed, which is what keeps
    // the node as framework-agnostic as the broker. Empty when the node
    // advertises no range or the workspace declares no listeners.
    ports: &[nook_types::LeasedPort],
) -> Result<()> {
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
    let port_s: Vec<(String, String)> = ports
        .iter()
        .map(|p| (p.env.clone(), p.port.to_string()))
        .collect();
    let extra: Vec<(&str, &str)> = port_s
        .iter()
        .map(|(env, port)| (env.as_str(), port.as_str()))
        .collect();
    spawn(
        name,
        cwd,
        cols,
        rows,
        &login_command(command),
        session_id,
        &extra,
    )
}

/// Start a runtime's LOGIN flow in a session (MAIN-126): the same PTY machinery
/// as `new_session`, but the pane runs `<runtime> <login_args>` (e.g. `claude
/// auth login`) rather than the bare runtime. `login_args` is the ALLOWLISTED
/// subcommand the node chose — never a wire value. Runs in the home directory,
/// through the login shell, so it sees the same environment loop sessions do.
pub fn new_auth_session(
    name: &str,
    cwd: &str,
    cols: u16,
    rows: u16,
    runtime: &str,
    login_args: &str,
    session_id: &str,
) -> Result<()> {
    if !std::path::Path::new(cwd).is_dir() {
        anyhow::bail!("home directory {cwd} does not exist on this node");
    }
    if !runtime_available(runtime) {
        anyhow::bail!("runtime '{runtime}' is not installed on this node");
    }
    // `exec` binds the pane to the login command, so quitting it ends the
    // session — which is the "authorization done, clean up" signal (AC-4).
    let launch = format!("{} -l -i -c 'exec {runtime} {login_args}'", login_shell());
    spawn(name, cwd, cols, rows, &launch, session_id, &[])
}

/// Start a loop-job session (MAIN-161): the same PTY machinery as
/// `new_session`, but `NOOK_JOB_ID` is injected into the session environment
/// alongside `NOOK_SESSION_ID` so anything running inside — the `nook` CLI, the
/// finish hook — can tie its output back to the loop job that owns it.
#[allow(clippy::too_many_arguments)]
pub fn new_job_session(
    name: &str,
    cwd: &str,
    cols: u16,
    rows: u16,
    runtime: &str,
    session_id: &str,
    job_id: &str,
    status_file: &str,
    seed: Option<&str>,
) -> Result<()> {
    if !std::path::Path::new(cwd).is_dir() {
        anyhow::bail!("checkout {cwd} does not exist on this node");
    }
    if !runtime_available(runtime) {
        anyhow::bail!("runtime '{runtime}' is not installed on this node");
    }
    // The human's opening brief (MAIN-231) rides the environment verbatim —
    // line breaks and all — so anything in the session can read the original,
    // not just the flattened line typed at the runtime.
    let mut env: Vec<(&str, &str)> = vec![("NOOK_JOB_ID", job_id)];
    if let Some(seed) = seed.filter(|s| !s.trim().is_empty()) {
        env.push(("NOOK_JOB_SEED", seed));
    }
    spawn(
        name,
        cwd,
        cols,
        rows,
        &job_launch_command(runtime, status_file),
        session_id,
        &env,
    )
}

/// A loop job's launch command: run the runtime, then write its exit status to
/// `status_file`. Unlike [`login_command`]'s `exec`, the shell survives the
/// runtime so it can record the code — tmux itself cannot surface an exit code
/// (AC-4 crash honesty needs it), and reading a status file the shell wrote is
/// not interpreting session output (NG-2). The pane still ends when the runtime
/// returns, since the `-c` command completes.
fn job_launch_command(runtime: &str, status_file: &str) -> String {
    let shell = login_shell();
    format!("{shell} -l -i -c '{runtime}; echo $? > {status_file}'")
}

/// Type a line into a session and press Enter — how a loop job drives its skill
/// (e.g. `/nook-spec MAIN-42`) once the runtime is up. `-l` sends the text
/// literally so slashes and spaces reach the runtime unmangled; Enter is a
/// separate key press.
pub fn send_keys(session: &str, line: &str) -> Result<()> {
    tmux(&["send-keys", "-t", session, "-l", line])?;
    tmux(&["send-keys", "-t", session, "Enter"])?;
    Ok(())
}

/// The shared tmux `new-session` spawn: create a detached session running
/// `launch`, with the UTF-8 locale, the session-id env, the sizing/rename
/// options every entry point wants, plus any `extra_env` a caller needs (a loop
/// job's `NOOK_JOB_ID`, say).
fn spawn(
    name: &str,
    cwd: &str,
    cols: u16,
    rows: u16,
    launch: &str,
    session_id: &str,
    extra_env: &[(&str, &str)],
) -> Result<()> {
    apply_server_defaults();
    let cols_s = cols.to_string();
    let rows_s = rows.to_string();
    let sid = format!("NOOK_SESSION_ID={session_id}");
    let mut args: Vec<&str> = vec![
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
        // Who this session is, in the environment, so anything running inside
        // it — `nook` in particular — can ask "which session am I, and
        // therefore which workspace am I confined to" without a lookup by tmux
        // name. Used to scope task claims to the checkout the agent is in, and
        // by the UI's agent-state signal.
        "-e",
        &sid,
    ];
    // Owned strings for any extra env, kept alive alongside `args`.
    let extra: Vec<String> = extra_env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    for pair in &extra {
        args.push("-e");
        args.push(pair);
    }
    args.push("-x");
    args.push(&cols_s);
    args.push("-y");
    args.push(&rows_s);
    args.push(launch);
    tmux(&args)?;
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

/// Detach every client currently attached to a session.
///
/// Called immediately before this node attaches its own, which makes "exactly
/// one client per session" true rather than merely intended. Every reconnect
/// builds a fresh session manager with an empty map, so the next attach spawned
/// ANOTHER `tmux attach` and nothing ever ended the previous one — a handful of
/// page refreshes left three clients on one session, and the symptoms are what
/// you would expect of a shared terminal nobody owns: input lands, live output
/// stops reaching the browser, and every attach gets slower as tmux redraws for
/// all of them (MAIN-363).
///
/// Detaching a client does NOT touch the session or its processes — that is the
/// whole reason tmux is the buffer of record here. Failure is ignored: a
/// session with no clients is the state we wanted anyway.
pub fn detach_clients(name: &str) {
    let _ = tmux(&["detach-client", "-s", name]);
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
    let ttys = tmux_command()
        .args(["list-clients", "-t", session, "-F", "#{client_tty}"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    for tty in ttys.lines().filter(|t| !t.is_empty()) {
        let _ = tmux_command().args(["refresh-client", "-t", tty]).output();
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

    /// The socket flag is present exactly when a socket is configured, and the
    /// default-server path (None) adds no `-L` at all — the byte-identical
    /// pre-108 behaviour (AC-1/AC-2).
    #[test]
    fn socket_args_present_only_when_configured() {
        assert_eq!(
            socket_args(Some("nook-crimson")),
            vec!["-L".to_string(), "nook-crimson".to_string()]
        );
        assert!(
            socket_args(None).is_empty(),
            "the default server gets no -L flag"
        );
    }

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
