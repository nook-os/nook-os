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

use anyhow::{bail, Context, Result};
use nook_types::{AuthProfile, AuthState};
use std::path::{Path, PathBuf};
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
    /// Argv for driving this login with PIPES instead of a terminal, when the
    /// runtime supports it (MAIN-650). `None` means the only way to sign this
    /// runtime in is a session.
    managed_login: Option<&'static str>,
    parse: fn(code: Option<i32>, output: &str) -> (AuthState, Option<String>),
    /// Which stream carries this runtime's status text.
    ///
    /// Not a detail worth a flag until a runtime disagreed: **codex-cli 0.145.0
    /// writes `login status` to STDERR**, so reading stdout hands the parser an
    /// empty string and a signed-out node reports `Unknown` instead of
    /// `NotAuthorized`. Recorded per adapter rather than by merging both
    /// streams for everyone, because claude's parser reads JSON out of stdout
    /// and merging would let a stray warning corrupt it.
    status_on_stderr: bool,
    /// Where a delivered credential lands (MAIN-283). `None` for a runtime
    /// whose credential layout we do not know — the node refuses the delivery
    /// rather than guessing at a path, which is the same rule that makes
    /// `login` a fixed table rather than a wire value.
    credential: Option<CredentialRule>,
}

/// How to find one runtime's credential file, without ever taking a path from
/// the wire.
///
/// The directory is the runtime's own configuration directory — read from the
/// environment variable that runtime honours, because that is what the runtime
/// itself will read. Falling back to a fixed path under `$HOME` matters for the
/// deployed case: the fleet's Claude identity is a mounted directory named by
/// `CLAUDE_CONFIG_DIR`, and writing to `~/.claude` there would install the
/// credential somewhere nothing reads it.
#[derive(Debug, Clone, Copy)]
struct CredentialRule {
    /// The env var naming the runtime's config directory, if it has one.
    dir_env: &'static str,
    /// Relative to `$HOME`, when the env var is unset.
    default_dir: &'static str,
    /// The credential file's name inside that directory.
    file: &'static str,
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
        // The MANAGED form: no TTY, and pinned to the subscription flow.
        //
        // `--claudeai` is not a nicety. Bare `auth login` offers a console
        // (API-billing) alternative, and this fleet's rule is subscription
        // login only — a flow nobody is watching must not be able to take the
        // other branch.
        managed_login: Some("auth login --claudeai"),
        parse: parse_claude,
        status_on_stderr: false,
        credential: Some(CredentialRule {
            dir_env: "CLAUDE_CONFIG_DIR",
            default_dir: ".claude",
            file: ".credentials.json",
        }),
    },
    Adapter {
        id: "codex",
        label: "Codex CLI",
        runtime: "codex",
        probe: "login status",
        // Present so the older per-node path stays uniform; the epic obtains
        // codex's credential from the CONTROL PLANE and delivers it (MAIN-291
        // NG-1), so nothing routine runs this.
        managed_login: None,
        login: "login --device-auth",
        parse: parse_codex,
        // Measured on codex-cli 0.145.0: `login status` goes to stderr.
        status_on_stderr: true,
        credential: Some(CredentialRule {
            dir_env: "CODEX_HOME",
            default_dir: ".codex",
            file: "auth.json",
        }),
    },
    Adapter {
        id: "hermes-portal",
        label: "Hermes → Nous Portal",
        runtime: "hermes",
        probe: "portal status",
        managed_login: None,
        login: "setup --portal",
        parse: parse_hermes_portal,
        status_on_stderr: false,
        // Hermes' credential layout is not settled here, so a delivery for it
        // is refused rather than written to a guessed path (MAIN-283).
        credential: None,
    },
];

/// The allowlisted login subcommand for a runtime, if we know one — the ONLY
/// thing a node will run for an authorize request, chosen here and never from
/// the wire (MAIN-126). `None` for an unknown runtime → refuse to launch.
/// Argv for the piped, terminal-free login, when this runtime has one.
pub fn managed_login_args(runtime: &str) -> Option<&'static str> {
    ADAPTERS
        .iter()
        .find(|a| a.runtime == runtime)
        .and_then(|a| a.managed_login)
}

pub fn login_args(runtime: &str) -> Option<&'static str> {
    ADAPTERS
        .iter()
        .find(|a| a.runtime == runtime)
        .map(|a| a.login)
}

/// The runtimes that have a login at all — which is the same set as "an AGENT
/// rather than a shell" (MAIN-647), because a shell has nothing to sign in to.
///
/// Read off the adapter table rather than written out a second time, so a
/// fourth runtime joins the readiness answer and the probe together.
pub fn authable_runtimes() -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for a in ADAPTERS {
        if !out.contains(&a.runtime) {
            out.push(a.runtime);
        }
    }
    out
}

/// Where a delivered credential for `runtime` would land, or `None` when this
/// node has no rule for it.
///
/// Exposed so a caller can report the destination without performing the write.
pub fn credential_path(runtime: &str) -> Option<PathBuf> {
    let rule = ADAPTERS
        .iter()
        .find(|a| a.runtime == runtime)
        .and_then(|a| a.credential)?;
    let dir = match std::env::var(rule.dir_env) {
        Ok(v) if !v.trim().is_empty() => PathBuf::from(v),
        // `$HOME` rather than a cached home: the node may run as a different
        // user than the one that built it, and the runtime reads whatever HOME
        // says at the time it runs.
        _ => PathBuf::from(std::env::var("HOME").ok()?).join(rule.default_dir),
    };
    Some(dir.join(rule.file))
}

/// The FILE NAME a runtime's credential is known by, with no directory.
///
/// A Pod executor needs exactly this and not [`credential_path`]: the
/// credential goes into a Secret, and the key it is stored under has to be the
/// name the runtime will look for once that Secret is projected into a job Pod.
/// Same table, so the two destinations cannot disagree about what the file is
/// called (MAIN-650).
pub fn credential_file(runtime: &str) -> Option<&'static str> {
    ADAPTERS
        .iter()
        .find(|a| a.runtime == runtime)
        .and_then(|a| a.credential.as_ref())
        .map(|rule| rule.file)
}

/// The credential this node holds for `runtime`, as bytes, if there is one.
///
/// The read half of [`install_credential`], for the executor that has to
/// PUBLISH what a login on this machine produced (MAIN-650). Same table, so it
/// cannot read from somewhere the writer would not have written.
pub fn credential_bytes(runtime: &str) -> Option<Vec<u8>> {
    std::fs::read(credential_path(runtime)?).ok()
}

/// Install a credential payload where `runtime` expects it (MAIN-283 AC-2).
///
/// The payload is **opaque** — this neither parses nor validates it. What is
/// decided here is only the destination, from the fixed table above, never from
/// the wire.
///
/// The write is atomic and private, in that order: the temporary file is
/// created in the SAME directory as the target (so the rename cannot cross a
/// filesystem and degrade into a copy), its mode is set to `0600` **before**
/// any bytes are written, and only then is it renamed over the target. A reader
/// therefore sees either the old credential or the new one, never a truncated
/// file, and never a world-readable one — not even for the instant between
/// create and chmod.
pub fn install_credential(runtime: &str, payload: &[u8]) -> Result<PathBuf> {
    let Some(target) = credential_path(runtime) else {
        bail!("this node has no credential rule for runtime `{runtime}`");
    };
    let dir = target
        .parent()
        .context("the credential path has no parent directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;

    // Named for the target so two concurrent deliveries for different runtimes
    // cannot collide, and with the process id so two for the SAME runtime
    // cannot either.
    let tmp = dir.join(format!(
        ".{}.nook-{}.tmp",
        target
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("credential"),
        std::process::id()
    ));
    // Best effort: a leftover from a killed process must not fail this one.
    let _ = std::fs::remove_file(&tmp);

    let result = write_private(&tmp, payload).and_then(|_| {
        std::fs::rename(&tmp, &target)
            .with_context(|| format!("cannot move the credential into {}", target.display()))
    });

    if result.is_err() {
        // No partial file left behind (AC-5): the target keeps whatever it had.
        let _ = std::fs::remove_file(&tmp);
    }
    result?;
    Ok(target)
}

/// Create `path` mode 0600 and write `payload` into it, flushed to disk.
fn write_private(path: &Path, payload: &[u8]) -> Result<()> {
    use std::io::Write;

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Set at CREATION, not afterwards. A create-then-chmod leaves a window
        // in which the credential is readable by anyone on the box.
        opts.mode(0o600);
    }
    let mut f = opts
        .open(path)
        .with_context(|| format!("cannot create {}", path.display()))?;
    f.write_all(payload)
        .with_context(|| format!("cannot write {}", path.display()))?;
    // Durable before the rename, so a crash cannot leave the target pointing at
    // an empty file.
    f.sync_all()
        .with_context(|| format!("cannot flush {}", path.display()))?;
    Ok(())
}

/// Probe one runtime and report whether it is now authorized (MAIN-283 AC-5).
///
/// A credential that was written but that the runtime does not accept is a
/// failed delivery, not a successful one — the write is not the outcome anybody
/// cares about.
pub fn is_authorized(runtime: &str) -> bool {
    ADAPTERS
        .iter()
        .filter(|a| a.runtime == runtime)
        .map(profile_for)
        .any(|p| p.state == AuthState::Authorized)
}

/// Probe every allowlisted profile, in registry order. Best-effort and never
/// fatal — this runs during capability detection on connect.
pub fn probe_all() -> Vec<AuthProfile> {
    delivered_by_executor(ADAPTERS.iter().map(profile_for).collect())
}

/// A node in Pod-executor mode reports what a JOB POD can authenticate as, not
/// what this process can (MAIN-669 AC-6). The rule is
/// [`crate::k8s_exec::delivered_runtime_auth`]; this is only where it is asked.
///
/// Applied here rather than in `capabilities::detect` because THREE call sites
/// push a fresh probe — the connect, a credential delivery, an authorize
/// session ending — and a reported state that depended on which of them ran
/// last would flip the dispatcher's gate at random.
#[cfg(feature = "kubernetes")]
fn delivered_by_executor(probed: Vec<AuthProfile>) -> Vec<AuthProfile> {
    // Publish what this node holds, if it changed (MAIN-650). Here because this
    // is the ONE place all three probe pushes pass through — the connect, a
    // credential delivery, and an authorize session ending — so a login
    // performed by any of them reaches job Pods without a fourth call site to
    // keep in step. Cheap and idempotent: a local read and a hash compare, and
    // the apiserver only when the bytes actually differ.
    crate::k8s_exec::spawn_credential_sync();
    match crate::k8s_exec::ExecutorConfig::from_env() {
        Ok(Some(cfg)) => crate::k8s_exec::delivered_runtime_auth(probed, &cfg),
        // Not a Pod executor, or one so misconfigured it will run nothing —
        // `capabilities::sandbox_capability` reports that state, and inventing
        // an authorization on top of it would say the node is ready to claim.
        _ => probed,
    }
}

#[cfg(not(feature = "kubernetes"))]
fn delivered_by_executor(probed: Vec<AuthProfile>) -> Vec<AuthProfile> {
    probed
}

/// Which of the two captured streams this adapter's parser should read.
///
/// A named function rather than an inline `if` so it can be asserted without a
/// runtime installed: the *fact* that codex reports on stderr is proved by the
/// behavioural test in nook-control (which needs a real codex), but that we act
/// on it must hold on every machine.
fn status_text<'a>(a: &Adapter, stdout: &'a str, stderr: &'a str) -> &'a str {
    if a.status_on_stderr {
        stderr
    } else {
        stdout
    }
}

fn profile_for(a: &Adapter) -> AuthProfile {
    // Presence first, through the login shell, so a runtime that lives only on
    // the interactive PATH is still found — and "not installed" (Unavailable)
    // stays distinct from "installed but signed out" (NotAuthorized).
    let (state, identity) = if !crate::tmux::runtime_available(a.runtime) {
        (AuthState::Unavailable, None)
    } else {
        match run_probe(a.runtime, a.probe) {
            Some((code, stdout, stderr)) => (a.parse)(code, status_text(a, &stdout, &stderr)),
            None => (AuthState::Unknown, None),
        }
    };
    AuthProfile {
        id: a.id.into(),
        label: a.label.into(),
        runtime: a.runtime.into(),
        state,
        identity,
        // A session on this machine is the right flow by default — it is the
        // only one for a runtime with no device-flow descriptor. The executor
        // overrides it where a terminal here would sign in the wrong place
        // (`delivered_by_executor`).
        device_flow: false,
    }
}

/// Run `<runtime> <probe>` through the login shell, bounded by [`PROBE_TIMEOUT`].
/// Returns `(exit code, stdout)`, or `None` if it could not run or timed out.
/// The command string is built only from the fixed adapter table, never input.
fn run_probe(runtime: &str, probe: &str) -> Option<(Option<i32>, String, String)> {
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
            String::from_utf8_lossy(&out.stderr).to_string(),
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

/// `codex login status`, measured against **codex-cli 0.145.0** (MAIN-291 AC-4).
///
/// | state                | output                            | exit |
/// |----------------------|-----------------------------------|------|
/// | authorized (OAuth)   | `Logged in using ChatGPT`         | 0    |
/// | authorized (API key) | `Logged in using an API key - ***`| 0    |
/// | signed out           | `Not logged in`                   | 1    |
/// | unreadable auth.json | `Error checking login status: …`  | 1    |
///
/// **Exit 1 is two different things**, which is why this does not simply mirror
/// [`parse_claude`]'s `1 => NotAuthorized`. A corrupt or half-written
/// `auth.json` exits 1 exactly as a clean sign-out does, and calling that
/// `NotAuthorized` would report a broken credential as an ordinary signed-out
/// node — inviting a re-delivery that would fail the same way. Only the
/// explicit sign-out line is `NotAuthorized`; an error is `Unknown`, which is
/// what the four-state model has for "the probe could not tell us".
fn parse_codex(code: Option<i32>, stdout: &str) -> (AuthState, Option<String>) {
    let lc = stdout.to_lowercase();
    match code {
        // The identity is the auth MODE codex reports, which is the useful part
        // and the only part it offers. codex redacts the key itself in the
        // API-key form, so nothing secret is carried through.
        Some(0) if lc.contains("logged in") => (
            AuthState::Authorized,
            stdout
                .split_once("Logged in using ")
                .map(|(_, rest)| rest.trim().to_string())
                .filter(|s| !s.is_empty()),
        ),
        Some(0) => (AuthState::Authorized, None),
        Some(1) if lc.contains("not logged in") => (AuthState::NotAuthorized, None),
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

    /// `codex login status`, against the contract measured on codex-cli
    /// 0.145.0 (MAIN-291 AC-4).
    #[test]
    fn codex_status_maps_the_measured_exit_codes() {
        // Authorized, both login styles codex reports.
        let (state, id) = parse_codex(Some(0), "Logged in using ChatGPT");
        assert_eq!(state, AuthState::Authorized);
        assert_eq!(id.as_deref(), Some("ChatGPT"));

        let (state, id) = parse_codex(Some(0), "Logged in using an API key - ***");
        assert_eq!(state, AuthState::Authorized);
        assert_eq!(
            id.as_deref(),
            Some("an API key - ***"),
            "codex redacts the key itself, so the mode is safe to surface"
        );

        // Signed out — the ONLY exit-1 shape that means signed out.
        assert_eq!(
            parse_codex(Some(1), "Not logged in").0,
            AuthState::NotAuthorized
        );
    }

    /// The distinction this parser exists for: exit 1 is *also* how codex
    /// reports an unreadable `auth.json`. Calling that `NotAuthorized` would
    /// present a corrupt credential as an ordinary signed-out node and invite a
    /// re-delivery that fails identically.
    #[test]
    fn a_broken_credential_is_unknown_not_signed_out() {
        for msg in [
            "Error checking login status: missing field `refresh_token` at line 1 column 212",
            "Error checking login status: invalid ID token format at line 1 column 55",
        ] {
            assert_eq!(
                parse_codex(Some(1), msg).0,
                AuthState::Unknown,
                "an error must not read as a clean sign-out: {msg}"
            );
        }
        // Unrecognised output, and a signal death, stay Unknown too.
        assert_eq!(parse_codex(Some(1), "").0, AuthState::Unknown);
        assert_eq!(parse_codex(None, "Not logged in").0, AuthState::Unknown);
    }

    /// The wiring, not the fact: codex's status is read from stderr and every
    /// other runtime's from stdout.
    ///
    /// Worth its own test because getting this wrong is invisible in the happy
    /// path — an authorized codex exits 0 either way — and only shows up as a
    /// signed-out node reporting `Unknown`. That is precisely the bug this
    /// caught during MAIN-291.
    #[test]
    fn codex_status_is_read_from_stderr_and_the_others_from_stdout() {
        let find = |r: &str| ADAPTERS.iter().find(|a| a.runtime == r).unwrap();

        assert_eq!(
            status_text(find("codex"), "on-stdout", "on-stderr"),
            "on-stderr",
            "codex-cli writes login status to stderr"
        );
        for runtime in ["claude", "hermes"] {
            assert_eq!(
                status_text(find(runtime), "on-stdout", "on-stderr"),
                "on-stdout",
                "{runtime} reports on stdout; merging the streams could corrupt \
                 claude's JSON"
            );
        }

        // End to end through the parser: the signed-out line on the stream
        // codex really uses must reach `parse_codex`.
        let codex = find("codex");
        let text = status_text(codex, "", "Not logged in");
        assert_eq!(
            (codex.parse)(Some(1), text).0,
            AuthState::NotAuthorized,
            "reading the wrong stream turns a clean sign-out into Unknown"
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

/// Credential delivery (MAIN-283).
///
/// These drive the real filesystem through `CLAUDE_CONFIG_DIR`, because the
/// destination rule IS the subject: a test that passed a path in would be
/// asserting its own argument. Each test points the env var at its own
/// temporary directory.
///
/// `#[serial]`-free by construction: the env var is process-global, so the
/// tests share one lock rather than racing over it.
#[cfg(test)]
mod credential_tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// `CLAUDE_CONFIG_DIR` is process-global; these tests take turns.
    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// A scratch directory, and `CLAUDE_CONFIG_DIR` pointed at it.
    fn scratch(name: &str) -> (PathBuf, MutexGuard<'static, ()>) {
        let guard = env_lock();
        let dir = std::env::temp_dir().join(format!(
            "nook-283-{name}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("CLAUDE_CONFIG_DIR", &dir);
        (dir, guard)
    }

    #[cfg(unix)]
    fn mode_of(p: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    /// AC-2: the payload lands where the runtime reads it, and only the owner
    /// can read it.
    #[test]
    fn a_claude_credential_lands_in_the_config_dir_at_0600() {
        let (dir, _g) = scratch("lands");

        let path = install_credential("claude", br#"{"token":"abc"}"#).expect("install");

        assert_eq!(
            path,
            dir.join(".credentials.json"),
            "the destination comes from CLAUDE_CONFIG_DIR, not from the caller"
        );
        assert_eq!(std::fs::read(&path).unwrap(), br#"{"token":"abc"}"#);
        #[cfg(unix)]
        assert_eq!(mode_of(&path), 0o600, "a credential is not world-readable");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The payload is opaque: bytes that are not JSON, not UTF-8 even, are
    /// written through unaltered. This end does not get to have an opinion.
    #[test]
    fn the_payload_is_written_through_byte_for_byte() {
        let (dir, _g) = scratch("opaque");
        let payload: &[u8] = &[
            0x00, 0xff, 0xfe, b'n', b'o', b't', b' ', b'j', b's', b'o', b'n',
        ];

        let path = install_credential("claude", payload).expect("install");

        assert_eq!(std::fs::read(&path).unwrap(), payload);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// AC-5: re-delivering is idempotent — same bytes, same mode, one file, no
    /// temporary left behind.
    #[test]
    fn re_delivery_is_idempotent() {
        let (dir, _g) = scratch("idempotent");

        let first = install_credential("claude", b"one").expect("first");
        let second = install_credential("claude", b"one").expect("second");
        assert_eq!(first, second);
        assert_eq!(std::fs::read(&first).unwrap(), b"one");

        // And a DIFFERENT payload replaces it rather than appending or failing.
        install_credential("claude", b"two").expect("replacement");
        assert_eq!(std::fs::read(&first).unwrap(), b"two");
        #[cfg(unix)]
        assert_eq!(mode_of(&first), 0o600, "the replacement is private too");

        let left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(
            left,
            vec![".credentials.json".to_string()],
            "no temporary file survives a delivery"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// AC-5: a destination that cannot be created fails cleanly, leaves nothing
    /// behind, and does not destroy the credential that was already there.
    ///
    /// The unwritable case is a *file* standing where the directory must be,
    /// not a chmod: these tests run as root in the container, and root ignores
    /// permission bits — a `0500` directory would have silently succeeded and
    /// the test would have been asserting nothing.
    #[test]
    fn an_uncreatable_destination_fails_without_a_partial_write() {
        let (dir, _g) = scratch("uncreatable");

        // Seed a good credential, so the assertion is that a failed delivery
        // PRESERVED it — not merely that nothing existed.
        let good = install_credential("claude", b"original").expect("seed");

        // Point the config dir at the seeded FILE. `create_dir_all` cannot make
        // a directory out of it, for any user.
        std::env::set_var("CLAUDE_CONFIG_DIR", &good);
        let err = install_credential("claude", b"replacement")
            .expect_err("a file is not a directory, for root or anyone else");
        assert!(
            format!("{err:#}").contains("cannot create"),
            "the failure says what it could not do: {err:#}"
        );

        std::env::set_var("CLAUDE_CONFIG_DIR", &dir);
        assert_eq!(
            std::fs::read(&good).unwrap(),
            b"original",
            "the existing credential survived the failed delivery"
        );
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert_eq!(
            left.len(),
            1,
            "no partial or temporary file was left behind"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// AC-2, the atomicity itself: a reader never sees the credential missing
    /// or half-written while it is being replaced.
    ///
    /// This is the assertion the temp+rename exists for, and the one the other
    /// tests do NOT make — mutation testing found that replacing the whole
    /// dance with a plain `remove` + `write` passed every one of them. The
    /// runtime reads this file whenever it likes; a window in which the file is
    /// absent is a window in which the runtime is spuriously signed out.
    ///
    /// Concurrency, not a mock: a reader spins on the path while a writer
    /// replaces it, and any observation of "missing" or "short" is a failure.
    #[test]
    fn a_reader_never_observes_the_credential_missing_during_a_replacement() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::Arc;

        let (dir, _g) = scratch("atomic");
        let payload = vec![b'x'; 4096];
        let target = install_credential("claude", &payload).expect("seed");

        let stop = Arc::new(AtomicBool::new(false));
        let bad = Arc::new(AtomicUsize::new(0));
        let reads = Arc::new(AtomicUsize::new(0));

        let reader = {
            let (stop, bad, reads, target) =
                (stop.clone(), bad.clone(), reads.clone(), target.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match std::fs::read(&target) {
                        // Every version of this file is 4096 bytes, so a short
                        // read is a torn one.
                        Ok(v) if v.len() == 4096 => {}
                        _ => {
                            bad.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    reads.fetch_add(1, Ordering::Relaxed);
                }
            })
        };

        for _ in 0..300 {
            install_credential("claude", &payload).expect("replace");
        }
        stop.store(true, Ordering::Relaxed);
        reader.join().unwrap();

        assert!(
            reads.load(Ordering::Relaxed) > 0,
            "the reader never ran, so this asserted nothing"
        );
        assert_eq!(
            bad.load(Ordering::Relaxed),
            0,
            "the credential was missing or truncated during {} reads — the \
             replacement is not atomic",
            reads.load(Ordering::Relaxed)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A runtime with no credential rule is refused, not written to a guessed
    /// path — the same reason `login_args` returns `None` rather than a default.
    #[test]
    fn a_runtime_without_a_rule_is_refused() {
        let (dir, _g) = scratch("norule");
        assert!(credential_path("hermes").is_none());
        assert!(credential_path("nonesuch").is_none());

        let err = install_credential("hermes", b"x").expect_err("refused");
        assert!(format!("{err:#}").contains("hermes"), "{err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// MAIN-291 AC-3: codex now HAS a rule, so a delivery for it is accepted
    /// where it used to be refused. Asserted through the same real-filesystem
    /// path claude's test uses, with `CODEX_HOME` as the subject.
    #[test]
    fn a_codex_credential_lands_in_codex_home_at_0600() {
        let _g = env_lock();
        let dir = std::env::temp_dir().join(format!(
            "nook-291-codex-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("CODEX_HOME", &dir);

        let path = install_credential("codex", br#"{"tokens":{}}"#)
            .expect("codex is no longer refused (AC-3)");
        assert_eq!(
            path,
            dir.join("auth.json"),
            "codex reads auth.json under CODEX_HOME"
        );
        assert_eq!(std::fs::read(&path).unwrap(), br#"{"tokens":{}}"#);
        #[cfg(unix)]
        assert_eq!(mode_of(&path), 0o600, "a credential is owner-only");

        std::env::remove_var("CODEX_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With `CODEX_HOME` unset the rule falls back to `$HOME/.codex/auth.json`
    /// — the path a developer's own codex uses.
    #[test]
    fn codex_falls_back_to_the_dot_codex_directory() {
        let _g = env_lock();
        std::env::remove_var("CODEX_HOME");
        let home = PathBuf::from(std::env::var("HOME").expect("HOME"));
        assert_eq!(
            credential_path("codex"),
            Some(home.join(".codex").join("auth.json"))
        );
    }

    /// The env var wins over the `$HOME` default: on the deployed node the
    /// fleet's identity is a mounted directory, and writing to `~/.claude`
    /// there would install the credential where nothing reads it.
    #[test]
    fn the_config_dir_env_var_wins_over_the_home_default() {
        let (dir, _g) = scratch("envwins");
        assert_eq!(
            credential_path("claude").unwrap(),
            dir.join(".credentials.json")
        );

        // Unset, and it falls back under HOME rather than failing.
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        let fallback = credential_path("claude").expect("a HOME default exists");
        assert!(
            fallback.ends_with(".claude/.credentials.json"),
            "unexpected fallback: {}",
            fallback.display()
        );
        // An empty value is not a directory — treat it as unset rather than
        // writing to the filesystem root.
        std::env::set_var("CLAUDE_CONFIG_DIR", "   ");
        assert_eq!(credential_path("claude").unwrap(), fallback);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
