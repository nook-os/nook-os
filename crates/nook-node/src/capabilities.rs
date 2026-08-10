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
        loop_kinds: loop_kinds(),
        max_loop_jobs: Some(max_loop_jobs()),
        max_loop_jobs_pinned: max_loop_jobs_pinned(),
        port_range: port_range(),
        runtime_auth: crate::runtime_auth::probe_all(),
    }
}

/// Every loop stage a node may be configured to run (MAIN-142). `build` is
/// listed so it can be *validated* — a node may name it, and the control plane
/// will still refuse to place build work on a shared operator.
pub const KNOWN_LOOP_KINDS: &[&str] = &["spec", "decompose", "review", "epic-run", "build"];

/// Which loop stages this node accepts, from `NOOK_LOOP_KINDS` (CSV).
///
/// Unset or empty means NONE — a node opts in, and an upgrade never opts one in
/// by accident. An unrecognised kind is a hard error rather than a silent drop:
/// a typo would otherwise leave the operator believing a stage was enabled.
pub fn parse_loop_kinds(raw: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for part in raw.split(',') {
        let k = part.trim().to_ascii_lowercase();
        if k.is_empty() {
            continue;
        }
        if !KNOWN_LOOP_KINDS.contains(&k.as_str()) {
            return Err(format!(
                "NOOK_LOOP_KINDS: unknown loop kind {k:?} — expected any of {}",
                KNOWN_LOOP_KINDS.join(", ")
            ));
        }
        if !out.contains(&k) {
            out.push(k);
        }
    }
    Ok(out)
}

fn loop_kinds() -> Vec<String> {
    let raw = std::env::var("NOOK_LOOP_KINDS").unwrap_or_default();
    match parse_loop_kinds(&raw) {
        Ok(kinds) => kinds,
        // Reported as none rather than guessed. The boot check in `main` is what
        // makes this loud; this arm exists so a mid-run re-report cannot panic.
        Err(e) => {
            tracing::error!("{e} — reporting no loop kinds");
            Vec::new()
        }
    }
}

/// Default concurrent loop jobs when `NOOK_MAX_LOOP_JOBS` is unset. Two, so an
/// operator can review one ticket while another runs, without a single slow job
/// stalling the queue.
pub const DEFAULT_MAX_LOOP_JOBS: u32 = 2;

/// How many loop jobs this node holds at once, from `NOOK_MAX_LOOP_JOBS`. `0`
/// disables claiming; an unparseable value falls back to the default rather
/// than to zero, because silently disabling a node is the worse failure.
///
/// This is what the node ADVERTISES, and since MAIN-508 it is no longer the
/// last word: an operator's central value wins over it, so the number can be
/// retuned without the restart that strands every in-flight build. A host that
/// must keep the decision locally sets `NOOK_MAX_LOOP_JOBS_PINNED`.
pub fn parse_max_loop_jobs(raw: &str) -> u32 {
    let raw = raw.trim();
    if raw.is_empty() {
        return DEFAULT_MAX_LOOP_JOBS;
    }
    raw.parse().unwrap_or(DEFAULT_MAX_LOOP_JOBS)
}

fn max_loop_jobs() -> u32 {
    parse_max_loop_jobs(&std::env::var("NOOK_MAX_LOOP_JOBS").unwrap_or_default())
}

/// Does this host insist on its own capacity (MAIN-508)? From
/// `NOOK_MAX_LOOP_JOBS_PINNED`.
///
/// Off by default, and deliberately a SEPARATE variable from the number: "2
/// jobs" and "and nobody else may say otherwise" are two statements, and every
/// existing machine already makes the first. Folding the pin into the value
/// would have pinned every node that ever set the number, which is the whole
/// population this card is about.
fn max_loop_jobs_pinned() -> bool {
    env_truthy(&std::env::var("NOOK_MAX_LOOP_JOBS_PINNED").unwrap_or_default())
}

/// The port range this node offers sessions (MAIN-301), from
/// `NOOK_PORT_RANGE` as `start-end`.
///
/// Unset means NONE, and that is deliberate: a guessed range would hand out
/// ports something else on the machine is already listening on. An operator
/// opts in, here or from the Nodes page.
pub fn parse_port_range(raw: &str) -> Option<(u16, u16)> {
    let (a, b) = raw.trim().split_once('-')?;
    let (start, end) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
    // A backwards or empty range is a typo, not a range. Reporting none is the
    // safe reading: no ports leased beats ports leased from nowhere.
    if start == 0 || end < start {
        return None;
    }
    Some((start, end))
}

fn port_range() -> Option<(u16, u16)> {
    parse_port_range(&std::env::var("NOOK_PORT_RANGE").unwrap_or_default())
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
    use super::{env_truthy, parse_loop_kinds, parse_max_loop_jobs, DEFAULT_MAX_LOOP_JOBS};

    #[test]
    fn shared_operator_flag_reads_truthy_env_only() {
        for on in ["1", "true", "TRUE", " yes ", "on"] {
            assert!(env_truthy(on), "{on:?} should be truthy");
        }
        for off in ["", "0", "false", "no", "off", "  "] {
            assert!(!env_truthy(off), "{off:?} should be false");
        }
    }

    /// Opting in is explicit: no configuration means the node runs nothing.
    /// The alternative — an empty list meaning "all" — would enrol every
    /// existing machine in agent work the moment this shipped.
    #[test]
    fn no_configuration_means_no_loop_kinds() {
        assert_eq!(parse_loop_kinds("").unwrap(), Vec::<String>::new());
        assert_eq!(parse_loop_kinds("  , ,").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn kinds_parse_case_and_space_insensitively_without_duplicates() {
        assert_eq!(
            parse_loop_kinds(" Spec, decompose ,SPEC, epic-run").unwrap(),
            vec!["spec", "decompose", "epic-run"]
        );
    }

    /// A typo must not read as "that stage is off" — the operator would believe
    /// they had enabled it. Refused at parse, and the boot check makes it loud.
    #[test]
    fn an_unknown_kind_is_refused_rather_than_dropped() {
        let err = parse_loop_kinds("spec,reviewww").expect_err("unknown kind");
        assert!(
            err.contains("reviewww"),
            "the message names the typo: {err}"
        );
        assert!(
            err.contains("epic-run"),
            "and lists what was expected: {err}"
        );
    }

    /// `build` parses — a node may declare it. The wall that stops a shared
    /// operator running build work is the control plane's, not this list's.
    #[test]
    fn build_is_a_valid_kind_to_declare() {
        assert_eq!(parse_loop_kinds("build").unwrap(), vec!["build"]);
    }

    /// Zero is a real answer (stop claiming); junk is not, and falling back to
    /// zero would silently take a node out of service.
    #[test]
    fn capacity_defaults_rather_than_zeroes_on_junk() {
        assert_eq!(parse_max_loop_jobs(""), DEFAULT_MAX_LOOP_JOBS);
        assert_eq!(parse_max_loop_jobs("  "), DEFAULT_MAX_LOOP_JOBS);
        assert_eq!(parse_max_loop_jobs("not-a-number"), DEFAULT_MAX_LOOP_JOBS);
        assert_eq!(parse_max_loop_jobs("0"), 0);
        assert_eq!(parse_max_loop_jobs(" 5 "), 5);
    }
}
