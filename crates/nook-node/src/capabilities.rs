//! Capability detection. The control plane never inspects a machine — the
//! node reports what it has. Detection is shell-out based and best-effort.

use nook_types::{Capabilities, GpuInfo};
use std::process::Command;
use sysinfo::System;

fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn which(bin: &str) -> bool {
    run("which", &[bin]).is_some_and(|p| !p.is_empty())
}

/// Docker and git are found on any sane PATH; runtimes often aren't. Ask the
/// login shell, since that's what actually launches them — see
/// `tmux::runtime_available`.
fn have_runtime(bin: &str) -> bool {
    which(bin) || crate::tmux::runtime_available(bin)
}

pub fn detect_gpus() -> Vec<GpuInfo> {
    // NVIDIA first; other vendors are future work.
    run("nvidia-smi", &["--query-gpu=name", "--format=csv,noheader"])
        .map(|out| {
            out.lines()
                .filter(|l| !l.trim().is_empty())
                .map(|model| GpuInfo {
                    vendor: "NVIDIA".into(),
                    model: model.trim().trim_start_matches("NVIDIA ").to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn detect_docker() -> bool {
    run("docker", &["version", "--format", "{{.Server.Version}}"]).is_some()
}

pub fn detect_tmux() -> Option<String> {
    // MAIN-108 exception: `tmux -V` reports the binary version and never touches
    // a server, so the node's `-L <socket>` is irrelevant here — unchanged.
    run("tmux", &["-V"]).map(|v| v.trim_start_matches("tmux ").to_string())
}

pub fn detect_git() -> Option<String> {
    run("git", &["--version"]).map(|v| v.split_whitespace().nth(2).unwrap_or_default().to_string())
}

/// Executables NookOS can launch inside a session.
pub const KNOWN_RUNTIMES: &[&str] = &["claude", "hermes", "codex", "bash", "zsh", "fish", "pwsh"];

pub fn detect_runtimes() -> Vec<String> {
    KNOWN_RUNTIMES
        .iter()
        .filter(|r| have_runtime(r))
        .map(|r| r.to_string())
        .collect()
}

pub fn detect() -> Capabilities {
    let mut sys = System::new_all();
    sys.refresh_all();
    Capabilities {
        hostname: System::host_name().unwrap_or_else(|| "unknown".into()),
        platform: std::env::consts::OS.to_string(),
        architecture: std::env::consts::ARCH.to_string(),
        cpus: sys.cpus().len() as u32,
        memory: sys.total_memory(),
        gpus: detect_gpus(),
        docker: detect_docker(),
        tmux: detect_tmux().is_some(),
        agent_version: Some(env!("CARGO_PKG_VERSION").to_string()),
        git: detect_git(),
        runtimes: detect_runtimes(),
        ssh_public_key: crate::ssh::public_key_for(
            crate::config::NodeConfig::load()
                .ok()
                .and_then(|c| c.ssh_key_path)
                .as_deref(),
        ),
        shared_operator: shared_operator(),
        runtime_auth: crate::runtime_auth::probe_all(),
    }
}

/// Is this the deployment's shared operator node (MAIN-125)? Read from
/// `NOOK_SHARED_OPERATOR`; the shipped operator-node container sets it, and no
/// personal node ever does.
fn shared_operator() -> bool {
    env_truthy(&std::env::var("NOOK_SHARED_OPERATOR").unwrap_or_default())
}

/// A truthy env value: `1`/`true`/`yes`/`on` (case/space-insensitive). Unset,
/// empty, `0`, and `false` are all false. Pure, so it is testable without env.
fn env_truthy(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[cfg(test)]
mod tests {
    use super::env_truthy;

    #[test]
    fn shared_operator_flag_reads_truthy_env_only() {
        for on in ["1", "true", "TRUE", " yes ", "on"] {
            assert!(env_truthy(on), "{on:?} should be truthy");
        }
        for off in ["", "0", "false", "no", "off", "  "] {
            assert!(!env_truthy(off), "{off:?} should be false");
        }
    }
}
