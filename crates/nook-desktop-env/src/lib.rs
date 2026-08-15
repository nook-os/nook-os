//! What the desktop shell hands its bundled processes, and the policy it
//! supervises them with.
//!
//! It lives here rather than in the Tauri shell because the shell is outside
//! the cargo workspace: a map written there can only be asserted against
//! itself, and that is exactly how MAIN-396 shipped a control plane that never
//! booted. It put the chosen port in `NOOK_CONTROL_PORT` — a compose-side
//! variable that publishes a host port and which no Rust has ever read — and
//! set no `SESSION_SECRET`, which the process requires before it binds
//! anything. From the workspace, `nook-control`'s own suite can start the real
//! binary with this map and probe it (MAIN-434 AC-4/AC-5). MAIN-398 adds the
//! node's half — its environment, its join spec and its restart policy — for
//! the same reason and with the same payoff: `tests/desktop_local_session.rs`
//! drives a real control plane and a real node through these values and opens
//! a session on the result.
//!
//! Errors are `String` because every caller is a shell command whose failure
//! is rendered to a person, and `anyhow` here would only be converted back.

use std::fs;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The environment the bundled control plane boots with.
///
/// Pure so it can be asserted — and executed — without launching a window. The
/// values here are the ones MAIN-376 showed go wrong silently: `PUBLIC_BASE_URL`
/// and `WEB_ORIGIN` left at a default point every task-key link, invite and
/// agent-authored URL at a port nothing is serving.
///
/// Both doors bind LOOPBACK. The control plane's own defaults are `0.0.0.0:8080`
/// and `0.0.0.0:8081`, which are right for a server and wrong for a laptop:
/// they publish somebody's control plane to the coffee-shop wifi, and they are
/// fixed numbers that collide with a dev stack on the same machine — fatally,
/// since both doors are bound before either is served (MAIN-285).
pub fn control_plane_env(
    db_path: &Path,
    port: u16,
    agent_port: u16,
    secrets: &LocalSecrets,
) -> Vec<(String, String)> {
    let base = format!("http://127.0.0.1:{port}");
    vec![
        // `sqlite://` selects the engine by URL, the same way boot does
        // (MAIN-195). A virgin file is the ordinary case: migrate + seed +
        // /healthz from nothing is what `tests/sqlite_boot.rs` already proves.
        (
            "DATABASE_URL".into(),
            format!("sqlite://{}", db_path.display()),
        ),
        // Never `production`: that arm makes a ledger it cannot account for
        // fatal, which is right for a server and wrong for a desktop app whose
        // database is a file the user can corrupt.
        ("APP_ENV".into(), "desktop".into()),
        // The variable `nook_infra::Config` actually reads.
        ("CONTROL_PLANE_BIND".into(), format!("127.0.0.1:{port}")),
        ("NOOK_AGENT_BIND".into(), format!("127.0.0.1:{agent_port}")),
        // Without it the process exits at config load — "SESSION_SECRET is
        // required" — before binding anything, so the only symptom is a health
        // check that never passes.
        ("SESSION_SECRET".into(), secrets.session_secret.clone()),
        ("PUBLIC_BASE_URL".into(), base.clone()),
        ("WEB_ORIGIN".into(), base),
        // The token the bundled node enrolls with (MAIN-398 AC-1). Seeded like
        // the compose stack's `NOOK_DEV_JOIN_TOKEN` and deliberately not that
        // variable: that one also seeds the dogfood workspace, a dev identity
        // and `loops.enabled`, which on somebody's laptop is a workspace
        // pointing at a repo that is not there and loops running by surprise.
        ("NOOK_LOCAL_JOIN_TOKEN".into(), secrets.join_token.clone()),
        (EXIT_WITH_PARENT.into(), "1".into()),
    ]
}

/// The ports the bundled node advertises for MAIN-301 leasing (AC-5).
///
/// A choice, not a default. It continues this repo's own convention — the
/// operator node leases `4100-4199` and the dev node `4200-4299` — sits well
/// below the ephemeral range a kernel hands out on its own, and 100 ports is
/// nine concurrent sessions at the eleven listeners this repo's own workspace
/// declares. Advertising none was the alternative and is worse: a workspace
/// with a `required` listener then cannot start a session at all, while a port
/// that turns out to be busy is something the allocator already recovers from
/// (`lease_for_avoiding`).
pub const LOCAL_PORT_RANGE: &str = "4300-4399";

/// The environment both `nook join` and `nook run` get.
///
/// `NOOK_CONFIG_DIR` is the whole reason this is not the person's own
/// `~/.config/nook`: `nook join` overwrites `node.toml` without asking, and
/// that file may be the identity of a machine they joined to a real fleet.
///
/// `NOOK_INSECURE` is what lets the node talk to `http://127.0.0.1`. The guard
/// it opens exists because "the node's credential and every session's terminal
/// output would cross the network in the clear" — and here there is no network:
/// both ends are one process tree on one machine, and the control plane binds
/// loopback precisely so nothing else can reach it (MAIN-434). It is scoped to
/// this map rather than exported, so it opens nothing for the person's own CLI.
pub fn node_env(config_dir: &Path) -> Vec<(String, String)> {
    vec![
        ("NOOK_CONFIG_DIR".into(), config_dir.display().to_string()),
        ("NOOK_PORT_RANGE".into(), LOCAL_PORT_RANGE.into()),
        ("NOOK_INSECURE".into(), "1".into()),
        (EXIT_WITH_PARENT.into(), "1".into()),
    ]
}

/// The join spec handed to `nook join --config <file>`.
///
/// A file rather than `--token` on the command line: process arguments are
/// readable by every other user on the machine, and this one enrolls a node.
/// Neither value needs escaping — the token is alphanumeric by generation and
/// the URL is a loopback address this crate built.
pub fn join_spec_toml(base_url: &str, token: &str) -> String {
    format!("server = \"{base_url}\"\ntoken = \"{token}\"\n")
}

/// How long the node must stay up before a restart stops counting as a flap.
pub const NODE_SETTLED: Duration = Duration::from_secs(30);

/// Consecutive fast failures before the UI is told, rather than being shown a
/// node that keeps almost-starting (AC-2).
pub const NODE_FLAPPING_AFTER: u32 = 3;

/// Backoff before the next restart: 1s doubling to a 32s ceiling.
///
/// A node that cannot start at all — a missing binary, a control plane that
/// went away — must not spin a core while the window sits there.
pub fn restart_delay(consecutive_failures: u32) -> Duration {
    Duration::from_secs(1 << consecutive_failures.min(5))
}

/// Whether this many consecutive fast failures is something the person should
/// be told about.
pub fn is_flapping(consecutive_failures: u32) -> bool {
    consecutive_failures >= NODE_FLAPPING_AFTER
}

/// Asks a bundled process to stop when the app that started it does (MAIN-400
/// AC-1/AC-2).
///
/// Set only by the two maps above, so it reaches the control plane and the node
/// a desktop install owns and nothing else: a node under systemd or compose has
/// no parent whose death means anything, and must not acquire one.
pub const EXIT_WITH_PARENT: &str = "NOOK_EXIT_WITH_PARENT";

/// How often an opted-in process checks whether it still has the parent it
/// started under. Half a second is imperceptible to a person quitting the app
/// and costs one syscall in that time.
const ORPHAN_CHECK: Duration = Duration::from_millis(500);

/// Whether a process that started under `started_under` has been orphaned.
///
/// Compared against the ORIGINAL parent rather than against pid 1, because pid
/// 1 is not where an orphan lands when a subreaper is in the way — a systemd
/// user session and most container init are subreapers, and there the reparent
/// target is their pid. "My parent is not who it was" is true in every one of
/// those cases and needs to know none of them.
pub fn orphaned(started_under: u32, parent_now: u32) -> bool {
    parent_now != started_under
}

/// Exit if the process that started this one goes away, when it asked for that
/// ([`EXIT_WITH_PARENT`]).
///
/// The desktop shell kills both sidecars on a clean quit, which is faster and
/// more direct than this. This is the half that covers the quit it never gets
/// to run: a `kill -9`, an OS force-quit, a panic in the shell. Without it the
/// control plane outlives the window, keeps the SQLite single-instance lock,
/// and the next launch is refused by the guard — the app looks permanently
/// broken after one force-quit (AC-2).
///
/// Called at the top of both binaries rather than only under `serve`/`run`: a
/// short-lived `nook join` is a child of the shell too, and a check that costs
/// one environment read is not worth making conditional.
pub fn exit_when_orphaned() {
    if std::env::var(EXIT_WITH_PARENT).is_ok_and(|v| !v.is_empty()) {
        watch_parent();
    }
}

#[cfg(unix)]
fn watch_parent() {
    // Read once, up front: after the parent dies this can only report the
    // reaper, so the value is meaningless unless it was taken while the parent
    // was still there.
    let started_under = unsafe { libc::getppid() } as u32;
    std::thread::spawn(move || loop {
        std::thread::sleep(ORPHAN_CHECK);
        let parent_now = unsafe { libc::getppid() } as u32;
        if orphaned(started_under, parent_now) {
            // stderr rather than tracing: this runs on any binary that opts in,
            // including one that has not installed a subscriber yet.
            eprintln!("the process that started this one (pid {started_under}) is gone — exiting");
            // Immediate and unconditional. Every durable thing here is a file
            // the kernel closes for us, and the one that matters is the
            // single-instance lock: it is released by the exit, which is what
            // lets the next launch in.
            std::process::exit(0);
        }
    });
}

/// Windows ships as a client app with no bundled sidecars at all
/// (`tauri.windows.conf.json`), so there is no orphan to prevent and nothing
/// here to be wrong about.
#[cfg(not(unix))]
fn watch_parent() {}

/// Append to a bounded tail. A boot log is unbounded and a reader wants its
/// end, which is where the failure is.
///
/// The cut is walked FORWARD to a char boundary before splitting. `split_off`
/// panics on an index inside a multi-byte character, and a raw
/// `len() - TAIL_KEEP` is a byte offset that lands there sooner or later — this
/// tree's own log lines are full of `—` and `…`. The consequence is not a lost
/// log line: this runs inside `supervise_local_node`'s task, so the panic would
/// kill the supervisor silently — leaving the node unsupervised with no
/// complaint recorded, which is the node-is-gone-but-the-UI-looks-fine state
/// AC-2 exists to prevent.
pub fn push_tail(buf: &mut String, line: &str) {
    buf.push_str(line);
    if buf.len() > TAIL_LIMIT {
        let mut cut = buf.len() - TAIL_KEEP;
        // Forward, so the tail can only get shorter than TAIL_KEEP, never
        // longer; `buf.len()` is always a boundary, so this terminates.
        while !buf.is_char_boundary(cut) {
            cut += 1;
        }
        *buf = buf.split_off(cut);
    }
}

const TAIL_LIMIT: usize = 16_384;
const TAIL_KEEP: usize = 8_192;

/// What a local install generates once and then keeps.
///
/// Kept rather than regenerated because both fields are agreements that outlive
/// a launch. A fresh `session_secret` invalidates every browser session, so the
/// person is signed out every time they reopen the app; a fresh `join_token`
/// would be a token the seeded database has never heard of, so the node it is
/// handed to could not enrol.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct LocalSecrets {
    pub session_secret: String,
    /// The bundled node's join token (MAIN-398 AC-1) — generated here, seeded
    /// into the control plane through its environment, handed to `nook join`
    /// through a `0600` file, and never shown to or typed by anyone.
    ///
    /// `default` so an install written before this field reads back with the
    /// session secret it already had rather than being treated as corrupt: the
    /// missing token is filled in below, and the sign-out that regenerating the
    /// whole file would cost is exactly what MAIN-434 AC-3 forbids.
    #[serde(default)]
    pub join_token: String,
}

/// The file `load_or_create_secrets` keeps them in.
pub const SECRETS_FILE: &str = "local-secrets.json";

/// Read this install's secrets, filling in whatever is missing.
///
/// A file that is missing, unreadable or half-written is replaced rather than
/// treated as fatal: it is regenerable state, and refusing to start over it
/// would strand an install that nothing else is wrong with. The cost is the
/// one sign-out that AC-3 is about, on a launch that was already broken.
///
/// Field by field rather than all-or-nothing, so an install that predates a
/// field keeps every value it already has. Rewriting the file wholesale is how
/// adding the join token would have signed everybody out once.
pub fn load_or_create_secrets(dir: &Path) -> Result<LocalSecrets, String> {
    let path = dir.join(SECRETS_FILE);
    let mut secrets = fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str::<LocalSecrets>(&t).ok())
        .unwrap_or_default();

    let mut changed = false;
    if secrets.session_secret.is_empty() {
        secrets.session_secret = random_secret(48);
        changed = true;
    }
    if secrets.join_token.is_empty() {
        secrets.join_token = format!("nook_join_{}", random_secret(32));
        changed = true;
    }
    if changed {
        let text = serde_json::to_string(&secrets).map_err(|e| e.to_string())?;
        fs::write(&path, text).map_err(|e| format!("could not write {}: {e}", path.display()))?;
        restrict_to_owner(&path);
    }
    Ok(secrets)
}

/// A credential from the OS CSPRNG. Alphanumeric so it survives a shell
/// environment and a config file without escaping.
///
/// The length is a floor as much as a size: config validation refuses a
/// `SESSION_SECRET` under 32 characters.
pub fn random_secret(len: usize) -> String {
    use rand::distr::Alphanumeric;
    use rand::Rng;
    rand::rng()
        .sample_iter(&Alphanumeric)
        .take(len)
        .map(char::from)
        .collect()
}

/// Best-effort `0600`. A failure is not fatal — the file is already inside the
/// app-data directory, and refusing to start over a chmod would cost more than
/// the exposure it prevents.
pub fn restrict_to_owner(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secrets() -> LocalSecrets {
        LocalSecrets {
            session_secret: "s3cr3t".into(),
            join_token: "nook_join_t0ken".into(),
        }
    }

    fn env_of(port: u16, agent_port: u16) -> Vec<(String, String)> {
        control_plane_env(Path::new("/tmp/x/nook.db"), port, agent_port, &secrets())
    }

    fn get(env: &[(String, String)], k: &str) -> String {
        env.iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| panic!("{k} is not set"))
    }

    /// AC-1/AC-2: the ports land in the two variables the control plane reads,
    /// on loopback.
    ///
    /// `tests/desktop_local_boot.rs` in nook-control is the half that cannot be
    /// argued with — it boots the binary on this map. This one names the
    /// variables, so a regression says which one moved.
    #[test]
    fn both_doors_bind_the_chosen_ports_on_loopback() {
        let env = env_of(41007, 41008);
        assert_eq!(get(&env, "CONTROL_PLANE_BIND"), "127.0.0.1:41007");
        assert_eq!(get(&env, "NOOK_AGENT_BIND"), "127.0.0.1:41008");
    }

    /// AC-1: the compose-side variable is GONE, not kept alongside. Two names
    /// for the port is how one of them stayed wrong without anyone noticing.
    #[test]
    fn the_compose_host_publish_variable_is_not_set_at_all() {
        let env = env_of(41007, 41008);
        assert!(
            !env.iter().any(|(n, _)| n == "NOOK_CONTROL_PORT"),
            "NOOK_CONTROL_PORT publishes a host port in compose; the process never reads it"
        );
        for (_, v) in &env {
            assert!(!v.contains("8080"), "no 8080 literal may survive: {v}");
            assert!(
                !v.starts_with("0.0.0.0"),
                "a desktop control plane is not published to the network: {v}"
            );
        }
    }

    /// AC-3: supplied at all — without it the child exits before binding.
    #[test]
    fn the_session_secret_is_supplied() {
        assert_eq!(get(&env_of(1, 2), "SESSION_SECRET"), "s3cr3t");
    }

    /// AC-4: the URLs carry the CHOSEN port (MAIN-396 AC-4, still true).
    #[test]
    fn the_public_urls_carry_the_chosen_port() {
        let env = env_of(41007, 41008);
        assert_eq!(get(&env, "PUBLIC_BASE_URL"), "http://127.0.0.1:41007");
        assert_eq!(get(&env, "WEB_ORIGIN"), "http://127.0.0.1:41007");
    }

    /// A SQLite URL at the app-data path, and an APP_ENV that is not
    /// production — a desktop database is a file the user can corrupt, and the
    /// production arm makes an unaccountable ledger fatal.
    #[test]
    fn the_database_is_sqlite_at_the_given_path_and_env_is_not_production() {
        let env = control_plane_env(
            Path::new("/home/a/.local/share/nook/nook.db"),
            1,
            2,
            &secrets(),
        );
        assert_eq!(
            get(&env, "DATABASE_URL"),
            "sqlite:///home/a/.local/share/nook/nook.db"
        );
        assert_ne!(get(&env, "APP_ENV"), "production");
    }

    /// AC-3: the same secret comes back on the next launch. A fresh one each
    /// start signs the person out of the app they just closed.
    #[test]
    fn the_session_secret_survives_a_relaunch() {
        let dir = scratch_dir("persists");
        let first = load_or_create_secrets(&dir).expect("first launch");
        let second = load_or_create_secrets(&dir).expect("second launch");
        assert_eq!(first.session_secret, second.session_secret);
        assert!(
            first.session_secret.len() >= 32,
            "config validation refuses a secret under 32 characters"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// …and it is a credential: two installs do not share one, and it needs no
    /// escaping wherever it is carried.
    #[test]
    fn a_generated_secret_is_random_and_unescaped() {
        let a = scratch_dir("random-a");
        let b = scratch_dir("random-b");
        let one = load_or_create_secrets(&a).expect("a");
        let two = load_or_create_secrets(&b).expect("b");
        assert_ne!(one.session_secret, two.session_secret);
        assert!(
            one.session_secret
                .chars()
                .all(|c| c.is_ascii_alphanumeric()),
            "{} would need escaping",
            one.session_secret
        );
        let _ = fs::remove_dir_all(&a);
        let _ = fs::remove_dir_all(&b);
    }

    /// A corrupt file is replaced rather than fatal — it is regenerable state,
    /// and an install that cannot start is worse than one sign-out.
    #[test]
    fn a_corrupt_secrets_file_is_replaced() {
        let dir = scratch_dir("corrupt");
        fs::write(dir.join(SECRETS_FILE), "{ not json").expect("write a broken file");
        let s = load_or_create_secrets(&dir).expect("a broken file is not fatal");
        assert!(!s.session_secret.is_empty());
        assert_eq!(
            s.session_secret,
            load_or_create_secrets(&dir).expect("reread").session_secret,
            "the replacement is itself persisted"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// MAIN-398 AC-1: the control plane is told the token its node will present.
    /// Without it the seed writes no row and the join is refused.
    #[test]
    fn the_control_plane_is_seeded_with_the_nodes_join_token() {
        assert_eq!(
            get(&env_of(1, 2), "NOOK_LOCAL_JOIN_TOKEN"),
            "nook_join_t0ken"
        );
    }

    /// …and NOT through the compose variable, which drags a dogfood workspace,
    /// a dev identity and `loops.enabled` onto a personal machine.
    #[test]
    fn it_does_not_use_the_compose_stacks_join_variable() {
        assert!(
            !env_of(1, 2).iter().any(|(n, _)| n == "NOOK_DEV_JOIN_TOKEN"),
            "that variable also seeds compose-stack scaffolding"
        );
    }

    /// AC-1: the token survives a relaunch, like the session secret. A fresh
    /// one would be a token the seeded database has never heard of.
    #[test]
    fn the_join_token_survives_a_relaunch_and_is_a_join_token() {
        let dir = scratch_dir("join-persists");
        let first = load_or_create_secrets(&dir).expect("first launch");
        let second = load_or_create_secrets(&dir).expect("second launch");
        assert_eq!(first.join_token, second.join_token);
        assert!(
            first.join_token.starts_with("nook_join_"),
            "{} is not shaped like a join token",
            first.join_token
        );
        assert_ne!(
            first.join_token, first.session_secret,
            "two credentials, not one reused"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// An install written before the join token existed keeps the session
    /// secret it already had. Regenerating the file wholesale would sign the
    /// person out of the app they had just been using (MAIN-434 AC-3).
    #[test]
    fn adding_the_join_token_does_not_rotate_an_existing_session_secret() {
        let dir = scratch_dir("upgrade");
        let old = "a-session-secret-from-before-main-398-0000000000";
        fs::write(
            dir.join(SECRETS_FILE),
            serde_json::json!({ "session_secret": old }).to_string(),
        )
        .expect("write a pre-398 file");

        let s = load_or_create_secrets(&dir).expect("an older file still loads");
        assert_eq!(s.session_secret, old, "nobody is signed out by an upgrade");
        assert!(!s.join_token.is_empty(), "the missing token is filled in");
        assert_eq!(
            s.join_token,
            load_or_create_secrets(&dir).expect("reread").join_token,
            "and persisted, not regenerated every launch"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// AC-5: the node advertises a range, and it is not one another node in
    /// this repo's own fleet already claims.
    #[test]
    fn the_node_advertises_its_own_port_range() {
        let env = node_env(Path::new("/home/a/.local/share/nook/node"));
        let range = env
            .iter()
            .find(|(n, _)| n == "NOOK_PORT_RANGE")
            .map(|(_, v)| v.clone())
            .expect("a range is advertised");
        assert_eq!(range, "4300-4399");
        for taken in ["4100-4199", "4200-4299"] {
            assert_ne!(range, taken, "the operator and dev nodes hold that one");
        }
    }

    /// AC-1: the node's identity goes in the app's own directory. `nook join`
    /// overwrites `node.toml` without asking, and `~/.config/nook` may be a
    /// machine the person joined to a real fleet.
    #[test]
    fn the_node_config_dir_is_the_one_we_were_given() {
        let env = node_env(Path::new("/home/a/.local/share/nook/node"));
        assert_eq!(
            env.iter()
                .find(|(n, _)| n == "NOOK_CONFIG_DIR")
                .map(|(_, v)| v.as_str()),
            Some("/home/a/.local/share/nook/node")
        );
    }

    /// The join spec is a file, not argv — process arguments are readable by
    /// every other user on the machine, and this one enrols a node.
    #[test]
    fn the_join_spec_carries_the_server_and_token() {
        let spec = join_spec_toml("http://127.0.0.1:41007", "nook_join_abc");
        let parsed: toml::Value = toml::from_str(&spec).expect("valid TOML");
        assert_eq!(parsed["server"].as_str(), Some("http://127.0.0.1:41007"));
        assert_eq!(parsed["token"].as_str(), Some("nook_join_abc"));
    }

    /// AC-2: backoff climbs and then stops climbing. Without a ceiling a long
    /// outage waits hours; without any wait a node that cannot start spins a
    /// core behind a window that looks idle.
    #[test]
    fn restarts_back_off_to_a_ceiling() {
        assert_eq!(restart_delay(0), Duration::from_secs(1));
        assert_eq!(restart_delay(1), Duration::from_secs(2));
        assert_eq!(restart_delay(5), Duration::from_secs(32));
        assert_eq!(
            restart_delay(1000),
            Duration::from_secs(32),
            "the ceiling holds however long it has been failing"
        );
    }

    /// AC-2: a node that keeps almost-starting is surfaced rather than left
    /// looking online — but a single restart is not yet news.
    #[test]
    fn only_repeated_failure_is_surfaced() {
        assert!(!is_flapping(0));
        assert!(!is_flapping(NODE_FLAPPING_AFTER - 1));
        assert!(is_flapping(NODE_FLAPPING_AFTER));
    }

    /// MAIN-400 AC-1/AC-2: both bundled processes are asked to stop when the
    /// app that started them does.
    ///
    /// The shell kills them on a clean quit; this variable is what covers the
    /// quit it never gets to run. Without it on the CONTROL PLANE, a force-quit
    /// leaves a process holding the SQLite single-instance lock and the next
    /// launch is refused — the app looks permanently broken after one crash.
    #[test]
    fn both_bundled_processes_exit_with_the_app() {
        assert_eq!(get(&env_of(1, 2), EXIT_WITH_PARENT), "1");
        assert_eq!(
            get(
                &node_env(Path::new("/home/a/.local/share/nook/node")),
                EXIT_WITH_PARENT
            ),
            "1"
        );
    }

    /// …and only there. A node under systemd or compose is started by an init
    /// whose "death" is not a signal to stop; asking it to exit with its parent
    /// would be asking it to exit on a restart of that supervisor.
    #[test]
    fn nothing_else_opts_a_process_into_it() {
        assert_eq!(EXIT_WITH_PARENT, "NOOK_EXIT_WITH_PARENT");
    }

    /// The orphan test is "my parent changed", not "my parent is pid 1".
    ///
    /// A systemd user session and most container init are subreapers, so an
    /// orphan there is reparented to THEM and never to 1 — a `== 1` check would
    /// simply never fire on the machines most likely to run this.
    #[test]
    fn an_orphan_is_recognised_by_the_parent_having_changed() {
        assert!(!orphaned(4321, 4321), "the parent is still there");
        assert!(orphaned(4321, 1), "reparented to init");
        assert!(orphaned(4321, 99), "reparented to a subreaper, not to init");
    }

    /// The log kept for the UI is bounded, and it is the END that is kept —
    /// that is where the failure is.
    ///
    /// The lines carry `—` and `…` on purpose. An all-ASCII log never puts the
    /// cut inside a character, so an ASCII-only version of this test passes
    /// against a `split_off` that panics on every real one this tree emits.
    #[test]
    fn the_kept_log_is_a_bounded_tail() {
        let mut buf = String::new();
        for i in 0..4000 {
            push_tail(&mut buf, &format!("line {i} — waiting for the node…\n"));
        }
        assert!(buf.len() <= TAIL_LIMIT, "{} bytes kept", buf.len());
        assert!(
            buf.ends_with("line 3999 — waiting for the node…\n"),
            "the tail must end at the last thing said"
        );
        assert!(!buf.contains("line 0 —"), "the head is what gets dropped");
    }

    /// The cut lands INSIDE a multi-byte character unless it is walked to a
    /// boundary — `String::split_off` panics there, and it would take the node
    /// supervisor down with it.
    #[test]
    fn a_multi_byte_character_across_the_cut_does_not_panic() {
        // Nothing but 3-byte characters, so the buffer's length is always a
        // multiple of 3 while `TAIL_KEEP` is not — the raw cut is therefore
        // GUARANTEED to land inside a character rather than merely likely to.
        // A fixed count, not `while len < limit`: the trim shrinks the buffer,
        // so that condition would never stop being true.
        let pushes = (TAIL_LIMIT / '…'.len_utf8()) * 2;
        let mut buf = String::new();
        for _ in 0..pushes {
            push_tail(&mut buf, "…");
        }
        assert!(buf.len() <= TAIL_LIMIT, "{} bytes kept", buf.len());
        assert!(
            buf.chars().all(|c| c == '…'),
            "the survivors must still be whole characters"
        );
    }

    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("nook-desktop-env-{tag}-{}", random_secret(8)));
        fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }
}
