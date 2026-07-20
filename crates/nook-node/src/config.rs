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

/// Expand a leading `~` against $HOME.
pub fn expand_path(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    p.to_string()
}
