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
        .filter(|r| which(r))
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
        git: detect_git(),
        runtimes: detect_runtimes(),
        ssh_public_key: crate::ssh::ensure_key(),
    }
}
