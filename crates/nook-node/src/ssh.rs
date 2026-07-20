//! Node SSH identity. The keypair is generated locally and the private key
//! never leaves this machine — only the public key is reported upward so it
//! can be added as a deploy key on a git host.

use std::path::PathBuf;
use std::process::Command;

pub fn key_path() -> Option<PathBuf> {
    crate::config::config_path()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("id_ed25519")))
}

/// Ensure the keypair exists; return the public key. Best-effort — a node
/// without ssh-keygen still works over https.
pub fn ensure_key() -> Option<String> {
    let key = key_path()?;
    if !key.exists() {
        if let Some(dir) = key.parent() {
            std::fs::create_dir_all(dir).ok()?;
        }
        let hostname = sysinfo::System::host_name().unwrap_or_else(|| "node".into());
        let out = Command::new("ssh-keygen")
            .args(["-t", "ed25519", "-N", "", "-C"])
            .arg(format!("nook@{hostname}"))
            .arg("-f")
            .arg(&key)
            .output()
            .ok()?;
        if !out.status.success() {
            tracing::warn!(
                "ssh-keygen failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
            return None;
        }
    }
    std::fs::read_to_string(key.with_extension("pub"))
        .ok()
        .map(|s| s.trim().to_string())
}

/// GIT_SSH_COMMAND for git network operations. Uses an explicit key when the
/// control plane supplied one (tenant credential in a transient 0600 file),
/// otherwise this node's own key if present.
pub fn git_ssh_command(explicit_key: Option<&std::path::Path>) -> Option<String> {
    let key = match explicit_key {
        Some(k) => k.to_path_buf(),
        None => {
            let own = key_path()?;
            if !own.exists() {
                return None;
            }
            own
        }
    };
    Some(format!(
        "ssh -i {} -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new",
        key.display()
    ))
}
