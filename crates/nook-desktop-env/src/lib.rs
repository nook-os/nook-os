//! What the desktop shell hands its bundled control plane at launch.
//!
//! It lives here rather than in the Tauri shell because the shell is outside
//! the cargo workspace: a map written there can only be asserted against
//! itself, and that is exactly how MAIN-396 shipped a control plane that never
//! booted. It put the chosen port in `NOOK_CONTROL_PORT` — a compose-side
//! variable that publishes a host port and which no Rust has ever read — and
//! set no `SESSION_SECRET`, which the process requires before it binds
//! anything. From the workspace, `nook-control`'s own suite can start the real
//! binary with this map and probe it (MAIN-434 AC-4/AC-5).
//!
//! Errors are `String` because every caller is a shell command whose failure
//! is rendered to a person, and `anyhow` here would only be converted back.

use std::fs;
use std::path::Path;

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
    ]
}

/// What a local install generates once and then keeps.
///
/// Kept rather than regenerated because it is an agreement across restarts: a
/// fresh `session_secret` invalidates every browser session, so the person is
/// signed out every time they reopen the app.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct LocalSecrets {
    pub session_secret: String,
}

/// The file `load_or_create_secrets` keeps them in.
pub const SECRETS_FILE: &str = "local-secrets.json";

/// Read this install's secrets, generating them on first run.
///
/// A file that is missing, unreadable or half-written is replaced rather than
/// treated as fatal: it is regenerable state, and refusing to start over it
/// would strand an install that nothing else is wrong with. The cost is the
/// one sign-out that AC-3 is about, on a launch that was already broken.
pub fn load_or_create_secrets(dir: &Path) -> Result<LocalSecrets, String> {
    let path = dir.join(SECRETS_FILE);
    if let Some(s) = fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str::<LocalSecrets>(&t).ok())
        .filter(|s| !s.session_secret.is_empty())
    {
        return Ok(s);
    }
    let secrets = LocalSecrets {
        session_secret: random_secret(48),
    };
    let text = serde_json::to_string(&secrets).map_err(|e| e.to_string())?;
    fs::write(&path, text).map_err(|e| format!("could not write {}: {e}", path.display()))?;
    restrict_to_owner(&path);
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

    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("nook-desktop-env-{tag}-{}", random_secret(8)));
        fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }
}
