//! Agent-authorization probing (MAIN-126, part 1).
//!
//! A node-owned registry of ALLOWLISTED probe commands: the control plane and
//! the UI never supply an executable or its arguments — the node alone decides
//! what to run, from the fixed table below. Each adapter probes one runtime's
//! authorization state by asking that runtime's own CLI (never by looking for a
//! credential file), and maps the result to a four-state [`AuthState`]:
//!
//! - `Unavailable` — the runtime binary is not installed on the session PATH.
//! - `Authorized` — the probe positively confirms a login.
//! - `NotAuthorized` — the probe positively confirms signed-out.
//! - `Unknown` — the probe hung, could not run, or its output was unrecognised.
//!
//! Probes run through the same login shell that launches loop sessions, so they
//! see the same user, HOME, and profile — the environment the credentials
//! actually live in. The login flow (`claude auth login`, `hermes setup
//! --portal`) is deliberately NOT here yet; that is part 2 (AC-2/AC-4).

use nook_types::{AuthProfile, AuthState};
use std::process::Command;
use std::time::Duration;

/// A probe that has not answered within this long is reported `Unknown` rather
/// than being allowed to stall a node's connect.
const PROBE_TIMEOUT: Duration = Duration::from_secs(6);

/// One allowlisted authorization profile and how to probe it. `probe` is a
/// fixed argv fragment — it is never constructed from anything off the wire.
struct Adapter {
    id: &'static str,
    label: &'static str,
    runtime: &'static str,
    probe: &'static str,
    /// The allowlisted LOGIN subcommand — the flow the Authorize button runs in
    /// a session (MAIN-126). Fixed here; never taken from the wire.
    login: &'static str,
    parse: fn(code: Option<i32>, stdout: &str) -> (AuthState, Option<String>),
}

/// The registry. Hermes supports several providers; this ticket scopes it to
/// the Nous Portal subscription profile only (`hermes portal status`), so there
/// is no universal "Hermes authorized" state — one profile per named target.
const ADAPTERS: &[Adapter] = &[
    Adapter {
        id: "claude",
        label: "Claude Code",
        runtime: "claude",
        probe: "auth status",
        login: "auth login",
        parse: parse_claude,
    },
    Adapter {
        id: "hermes-portal",
        label: "Hermes → Nous Portal",
        runtime: "hermes",
        probe: "portal status",
        login: "setup --portal",
        parse: parse_hermes_portal,
    },
];

/// The allowlisted login subcommand for a runtime, if we know one — the ONLY
/// thing a node will run for an authorize request, chosen here and never from
/// the wire (MAIN-126). `None` for an unknown runtime → refuse to launch.
pub fn login_args(runtime: &str) -> Option<&'static str> {
    ADAPTERS
        .iter()
        .find(|a| a.runtime == runtime)
        .map(|a| a.login)
}

/// Probe every allowlisted profile, in registry order. Best-effort and never
/// fatal — this runs during capability detection on connect.
pub fn probe_all() -> Vec<AuthProfile> {
    ADAPTERS.iter().map(profile_for).collect()
}

fn profile_for(a: &Adapter) -> AuthProfile {
    // Presence first, through the login shell, so a runtime that lives only on
    // the interactive PATH is still found — and "not installed" (Unavailable)
    // stays distinct from "installed but signed out" (NotAuthorized).
    let (state, identity) = if !crate::tmux::runtime_available(a.runtime) {
        (AuthState::Unavailable, None)
    } else {
        match run_probe(a.runtime, a.probe) {
            Some((code, stdout)) => (a.parse)(code, &stdout),
            None => (AuthState::Unknown, None),
        }
    };
    AuthProfile {
        id: a.id.into(),
        label: a.label.into(),
        runtime: a.runtime.into(),
        state,
        identity,
    }
}

/// Run `<runtime> <probe>` through the login shell, bounded by [`PROBE_TIMEOUT`].
/// Returns `(exit code, stdout)`, or `None` if it could not run or timed out.
/// The command string is built only from the fixed adapter table, never input.
fn run_probe(runtime: &str, probe: &str) -> Option<(Option<i32>, String)> {
    let shell = crate::tmux::login_shell();
    let cmd = format!("{runtime} {probe}");
    let (tx, rx) = std::sync::mpsc::channel();
    // A hung CLI must not stall connect: run it on a throwaway thread and stop
    // waiting after the timeout (the thread, and its child, are left to exit).
    std::thread::spawn(move || {
        let out = Command::new(&shell).args(["-l", "-i", "-c", &cmd]).output();
        let _ = tx.send(out);
    });
    match rx.recv_timeout(PROBE_TIMEOUT) {
        Ok(Ok(out)) => Some((
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).to_string(),
        )),
        _ => None,
    }
}

/// `claude auth status`: exits 0 with JSON when logged in, 1 when logged out
/// (subscription OAuth — never an API key). Any other exit is unrecognised, so
/// `Unknown` rather than a guess.
fn parse_claude(code: Option<i32>, stdout: &str) -> (AuthState, Option<String>) {
    match code {
        Some(0) => (AuthState::Authorized, identity_from_json(stdout)),
        Some(1) => (AuthState::NotAuthorized, None),
        _ => (AuthState::Unknown, None),
    }
}

/// `hermes portal status`. Its exact output is not pinned down here, so this is
/// deliberately conservative (the ticket's instruction): only a clear signed-in
/// marker on a clean exit is `Authorized`, only a clear signed-out marker is
/// `NotAuthorized`, and anything else — the common case for an unexpected
/// format — is `Unknown`.
fn parse_hermes_portal(code: Option<i32>, stdout: &str) -> (AuthState, Option<String>) {
    let lc = stdout.to_lowercase();
    let signed_out = [
        "not authenticated",
        "not logged in",
        "signed out",
        "no active",
    ]
    .iter()
    .any(|m| lc.contains(m));
    let signed_in = [
        "logged in as",
        "authenticated as",
        "active profile",
        "signed in as",
    ]
    .iter()
    .any(|m| lc.contains(m));
    if signed_out {
        (AuthState::NotAuthorized, None)
    } else if signed_in && code == Some(0) {
        (AuthState::Authorized, first_email(stdout))
    } else {
        (AuthState::Unknown, None)
    }
}

/// Pull a signed-in account out of a probe's JSON, best-effort: the first of a
/// few likely string fields. Absent or non-JSON → no identity (still authorized
/// if the exit code said so).
fn identity_from_json(stdout: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
    for key in ["email", "account", "login", "user", "name"] {
        if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// The first email-shaped token in some text — a last-resort identity when the
/// output is not JSON.
fn first_email(text: &str) -> Option<String> {
    text.split_whitespace()
        .find(|w| {
            let w = w.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '@' && c != '.');
            w.contains('@') && w.contains('.') && !w.starts_with('@')
        })
        .map(|w| {
            w.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '@' && c != '.')
                .to_string()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_status_maps_exit_codes_to_states() {
        // Logged in: exit 0 with JSON → Authorized, identity pulled from JSON.
        let (state, id) = parse_claude(Some(0), r#"{"email":"pm@example.com"}"#);
        assert_eq!(state, AuthState::Authorized);
        assert_eq!(id.as_deref(), Some("pm@example.com"));

        // Logged out: exit 1 → NotAuthorized.
        assert_eq!(parse_claude(Some(1), "").0, AuthState::NotAuthorized);

        // Anything else (a crash, a signal) → Unknown, never a guess.
        assert_eq!(parse_claude(Some(2), "boom").0, AuthState::Unknown);
        assert_eq!(parse_claude(None, "").0, AuthState::Unknown);

        // Authorized but no parseable identity is still Authorized.
        let (state, id) = parse_claude(Some(0), "not json");
        assert_eq!(state, AuthState::Authorized);
        assert_eq!(id, None);
    }

    #[test]
    fn hermes_portal_is_conservative() {
        // A clear signed-in marker on a clean exit → Authorized.
        let (state, id) = parse_hermes_portal(Some(0), "Logged in as pm@example.com (Nous Portal)");
        assert_eq!(state, AuthState::Authorized);
        assert_eq!(id.as_deref(), Some("pm@example.com"));

        // A clear signed-out marker → NotAuthorized.
        assert_eq!(
            parse_hermes_portal(Some(0), "Not authenticated").0,
            AuthState::NotAuthorized
        );

        // Unexpected output → Unknown, not a guess (the conservative default).
        assert_eq!(
            parse_hermes_portal(Some(0), "some future format").0,
            AuthState::Unknown
        );
        // A positive-looking word but a non-zero exit is not enough → Unknown.
        assert_eq!(
            parse_hermes_portal(Some(3), "logged in as x@y.com").0,
            AuthState::Unknown
        );
    }

    #[test]
    fn identity_reads_the_first_present_field() {
        assert_eq!(
            identity_from_json(r#"{"account":"acct@x.com","email":""}"#).as_deref(),
            Some("acct@x.com"),
        );
        assert_eq!(identity_from_json("[]"), None);
        assert_eq!(identity_from_json("garbage"), None);
    }
}
