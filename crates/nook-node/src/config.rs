//! Node configuration, persisted at `~/.config/nook/node.toml` after join.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConfig {
    pub server: String,
    pub node_id: String,
    pub node_name: String,
    pub node_token: String,
    pub workspace_roots: Vec<String>,
    /// Private key used for git operations (clones of private repos). When
    /// unset, the node's own generated key (~/.config/nook/id_ed25519) is
    /// used. Set by `nook setup` when you pick an existing ~/.ssh key.
    #[serde(default)]
    pub ssh_key_path: Option<String>,
}

pub fn config_path() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("NOOK_CONFIG_DIR") {
        return Ok(PathBuf::from(dir).join("node.toml"));
    }
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config/nook/node.toml"))
}

impl NodeConfig {
    pub fn load() -> Result<Self> {
        let path = config_path()?;
        let raw = std::fs::read_to_string(&path).with_context(|| {
            format!(
                "no node config at {} — run `nook join` first",
                path.display()
            )
        })?;
        Ok(toml::from_str(&raw)?)
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, toml::to_string_pretty(self)?)?;
        Ok(())
    }
}

/// A person's credential for this CLI, at `~/.config/nook/auth.toml`.
///
/// Kept apart from `node.toml` on purpose: node.toml is the machine's identity,
/// written by `nook join` and owned by the service; this is *yours*, written by
/// `nook login`, and it is what lets the CLI drive machines other than this one
/// (a node token is confined to its own machine by design).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    /// Control plane URL. Falls back to node.toml's when this file is written
    /// on a machine that has already joined.
    #[serde(default)]
    pub server: Option<String>,
    pub token: String,
}

pub fn auth_path() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("NOOK_CONFIG_DIR") {
        return Ok(PathBuf::from(dir).join("auth.toml"));
    }
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config/nook/auth.toml"))
}

impl AuthConfig {
    pub fn load() -> Result<Self> {
        let path = auth_path()?;
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("no login at {}", path.display()))?;
        Ok(toml::from_str(&raw)?)
    }

    pub fn save(&self) -> Result<()> {
        let path = auth_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, toml::to_string_pretty(self)?)?;
        // It's a password. Nobody else on this machine needs to read it.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }
}

// ── Transport security ──────────────────────────────────────────────────────
//
// A node carries a credential that lets it act as a machine in someone's
// fleet, and it streams terminal output — including whatever a session prints.
// Sending that over plaintext is not a configuration preference, it is a
// vulnerability, so the default is to refuse rather than to warn and continue.
//
// The escape hatch exists for exactly one case: the docker-compose dev stack,
// where the control plane is `http://control-plane:8080` on a container
// network. It is deliberately awkward — an explicit opt-in, a warning on every
// start, and a hard refusal in production — because the two real answers for
// self-hosting without public DNS are a pinned CA fingerprint in the join
// token and an operator-supplied CA certificate, not turning encryption off.

/// Did the operator explicitly ask for an unencrypted/unverified control-plane
/// connection? `NOOK_INSECURE=1` or `--insecure-skip-verify`.
pub fn insecure_requested(flag: bool) -> bool {
    flag || matches!(
        std::env::var("NOOK_INSECURE").ok().as_deref(),
        Some("1") | Some("true")
    )
}

/// Refuse a plaintext control plane unless the hatch is open.
///
/// Returns whether the hatch is in use, so the caller can warn once at startup.
/// `APP_ENV=production` refuses outright — mirroring how the control plane
/// rejects `AUTH_DEV_MODE` in production, so a machine that thinks it is
/// production cannot be talked into plaintext by an environment variable.
pub fn check_server_security(server: &str, insecure_flag: bool) -> Result<bool> {
    let plaintext = !server.trim().to_ascii_lowercase().starts_with("https://");
    if !plaintext {
        return Ok(false);
    }
    let insecure = insecure_requested(insecure_flag);
    let production = std::env::var("APP_ENV").ok().as_deref() == Some("production");

    if insecure && production {
        anyhow::bail!(
            "refusing an unencrypted connection to {server}: NOOK_INSECURE is set but \
             APP_ENV=production. Point the node at an https:// control plane."
        );
    }
    if !insecure {
        anyhow::bail!(
            "refusing an unencrypted connection to {server}.\n\n\
             The node's credential and every session's terminal output would cross \
             the network in the clear.\n\n\
             Fix it one of these ways:\n\
             • point --server at an https:// URL (the normal answer)\n\
             • for LOCAL DEV ONLY, re-run with --insecure-skip-verify or NOOK_INSECURE=1"
        );
    }
    Ok(true)
}

/// Say it every time, not just once — an insecure link that has been running
/// for months should still be announcing itself.
pub fn warn_if_insecure(insecure_in_use: bool, server: &str) {
    if insecure_in_use {
        tracing::warn!(
            %server,
            "INSECURE: talking to the control plane over an unencrypted connection \
             (NOOK_INSECURE). The node token and all terminal output are in the clear. \
             Local development only — never for a real fleet."
        );
    }
}

/// Expand a leading `~` against $HOME.
pub fn expand_path(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    p.to_string()
}

#[cfg(test)]
mod security_tests {
    use super::*;

    /// The env vars these tests set are process-global, so they run serially.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn clear() {
        std::env::remove_var("NOOK_INSECURE");
        std::env::remove_var("APP_ENV");
    }

    #[test]
    fn https_needs_no_hatch() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        // Not insecure, and no error: the ordinary path stays silent.
        assert_eq!(
            check_server_security("https://nook.example.com", false).unwrap(),
            false
        );
        // Even in production, and even with the hatch set — https is just fine.
        std::env::set_var("APP_ENV", "production");
        std::env::set_var("NOOK_INSECURE", "1");
        assert_eq!(
            check_server_security("https://nook.example.com", false).unwrap(),
            false
        );
        clear();
    }

    #[test]
    fn plaintext_is_refused_by_default() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        let err = check_server_security("http://control-plane:8080", false).unwrap_err();
        assert!(err
            .to_string()
            .contains("refusing an unencrypted connection"));
        // A bare host is plaintext too — the old code silently made it ws://.
        assert!(check_server_security("nook.example.com", false).is_err());
        clear();
    }

    #[test]
    fn hatch_opens_only_when_asked() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        // Via the flag...
        assert!(check_server_security("http://localhost:8080", true).unwrap());
        // ...or the env var.
        std::env::set_var("NOOK_INSECURE", "1");
        assert!(check_server_security("http://localhost:8080", false).unwrap());
        clear();
    }

    #[test]
    fn production_refuses_the_hatch() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        std::env::set_var("APP_ENV", "production");
        std::env::set_var("NOOK_INSECURE", "1");
        let err = check_server_security("http://control-plane:8080", true).unwrap_err();
        assert!(
            err.to_string().contains("APP_ENV=production"),
            "production must refuse the hatch outright, got: {err}"
        );
        clear();
    }
}
