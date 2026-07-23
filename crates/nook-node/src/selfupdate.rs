//! Replacing this agent's own binary.
//!
//! Two ways in: the control plane asks (someone pressed a button), or the agent
//! notices on reconnect that it is not the version the control plane expects.
//! Neither polls anything — a node reconnects whenever the control plane
//! restarts, which is exactly when a fleet needs updating, so a deploy carries
//! the news for free.
//!
//! **The node follows the control plane, never GitHub.** That is what keeps
//! version skew from happening at all: the operator decides what the fleet runs
//! by deciding what they deploy, and a release published upstream changes
//! nothing until they do. A node that chased the newest tag could outrun the
//! control plane it has to speak to.

use anyhow::{bail, Context, Result};

use crate::config::NodeConfig;

/// Whether anything would start this process again.
///
/// Told to update while unsupervised, an agent would replace its binary, exit,
/// and never return — and doing that across a fleet takes every machine down at
/// once. So the answer has to be recorded, not guessed.
///
/// Pure on purpose: the runtime probe is supplied rather than read here. The
/// probe reads a process-global environment variable, and a version that read
/// it directly would depend on whatever started the test process — GitHub's
/// runners run under systemd, so `INVOCATION_ID` is set there and every
/// "unsupervised" assertion flipped on CI while passing in a bare container.
/// Threading the probe through keeps the decision testable without touching the
/// environment, and without one test's `set_var` racing another's `remove_var`.
fn supervised_with(cfg: &NodeConfig, supervisor_detected: bool) -> bool {
    if matches!(
        cfg.service.as_deref(),
        Some("systemd-user") | Some("systemd-system") | Some("launchd") | Some("supervisord")
    ) {
        return true;
    }
    // A node enrolled before `service` existed is supervised in reality and
    // silent about it in config, so it could never self-update — it would sit
    // one version behind forever while its logs said nothing. Every node in
    // this fleet was in that state, which is not an edge case, it is what
    // upgrading an old install looks like.
    //
    // `docker` is the one answer that must NOT be second-guessed: a container
    // IS restarted by its runtime, so detection would say yes, and rewriting a
    // binary inside a layer that vanishes on the next `up` is exactly wrong.
    if cfg.service.as_deref() == Some("docker") {
        return false;
    }
    supervisor_detected
}

/// Did a service manager start this process?
///
/// Asked of the runtime rather than of config, because config can be stale and
/// the runtime cannot. systemd sets `INVOCATION_ID` for every unit it starts —
/// it is per-invocation and nothing else sets it. launchd does not offer an
/// equivalent, so a launchd node without the config field keeps the old
/// behaviour: refuse, and say why.
fn supervisor_detected() -> bool {
    std::env::var_os("INVOCATION_ID").is_some()
}

/// Why an update was refused, in words worth showing an operator.
pub fn refusal(cfg: &NodeConfig) -> Option<String> {
    refusal_with(cfg, supervisor_detected())
}

fn refusal_with(cfg: &NodeConfig, supervisor_detected: bool) -> Option<String> {
    if supervised_with(cfg, supervisor_detected) {
        return None;
    }
    Some(match cfg.service.as_deref() {
        // A container is replaced by pulling a new image, not by rewriting a
        // binary inside a layer that vanishes on the next `up`.
        Some("docker") => "this node runs in a container — update its image instead".into(),
        _ => "nothing would restart this agent, so updating it would take it \
              offline. Re-run `nook setup` and install it as a service."
            .into(),
    })
}

/// Fetch, verify, replace, and exit so the supervisor starts the new binary.
///
/// Never returns on success: the process ends, deliberately. Restarting in
/// place would leave the old code running with a new file on disk, which is the
/// state where "did the update work?" has no honest answer.
pub async fn run(reason: &str) -> Result<()> {
    let cfg = NodeConfig::load().context("this machine has not joined a control plane")?;
    if let Some(why) = refusal(&cfg) {
        bail!("{why}");
    }

    tracing::info!(reason, "updating this agent");
    crate::update_binary().await?;

    // tmux is the buffer of record and outlives this process, so sessions
    // survive the restart. That is why KillMode=process matters in the unit,
    // and why this is safe to do under someone's running work.
    tracing::info!("update installed — exiting so the service manager restarts us");
    std::process::exit(0);
}

/// Should this agent update itself, given what the control plane expects?
///
/// Compared exactly rather than ordered: the question is "am I what the control
/// plane expects", not "am I newer". A node downgraded deliberately should
/// follow the downgrade.
pub fn should_update(expected: Option<&str>, cfg: &NodeConfig) -> bool {
    should_update_with(expected, cfg, supervisor_detected())
}

fn should_update_with(expected: Option<&str>, cfg: &NodeConfig, supervisor_detected: bool) -> bool {
    let Some(expected) = expected else {
        return false;
    };
    expected != env!("CARGO_PKG_VERSION") && supervised_with(cfg, supervisor_detected)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(service: Option<&str>) -> NodeConfig {
        NodeConfig {
            server: "https://x".into(),
            node_id: "n".into(),
            node_name: "n".into(),
            node_token: String::new(),
            workspace_roots: vec![],
            ssh_key_path: None,
            server_fingerprint: None,
            agent_server: None,
            service: service.map(str::to_string),
        }
    }

    // The probe result is passed in rather than read from the environment, so
    // these are deterministic and cannot race each other's `set_var`. `false`
    // means "no supervisor detected", `true` means systemd's `INVOCATION_ID`
    // was present — the two real cases, named at every call.
    const NO_SUPERVISOR: bool = false;
    const DETECTED: bool = true;

    /// The check that keeps a fleet from going dark. An agent nothing restarts,
    /// with nothing detecting one either, must refuse — and say why in terms an
    /// operator can act on.
    #[test]
    fn an_unsupervised_agent_refuses_to_update() {
        for s in [None, Some("none")] {
            let c = cfg(s);
            assert!(!supervised_with(&c, NO_SUPERVISOR));
            let why = refusal_with(&c, NO_SUPERVISOR).expect("must refuse");
            assert!(why.contains("nook setup"), "{why}");
        }
    }

    #[test]
    fn a_container_is_told_to_update_its_image() {
        let why = refusal_with(&cfg(Some("docker")), NO_SUPERVISOR).expect("must refuse");
        assert!(why.contains("image"), "{why}");
    }

    #[test]
    fn supervised_agents_are_allowed() {
        // supervisord included: a config that names it must self-update without
        // waiting on `INVOCATION_ID`, which supervisord does not set.
        for s in ["systemd-user", "systemd-system", "launchd", "supervisord"] {
            assert!(supervised_with(&cfg(Some(s)), NO_SUPERVISOR), "{s}");
            assert!(refusal_with(&cfg(Some(s)), NO_SUPERVISOR).is_none(), "{s}");
        }
    }

    /// Matching the control plane is the goal, not being newest — otherwise a
    /// deliberate downgrade would be undone by every node that noticed.
    #[test]
    fn it_follows_the_control_plane_in_either_direction() {
        let c = cfg(Some("systemd-user"));
        assert!(
            should_update_with(Some("99.0.0"), &c, NO_SUPERVISOR),
            "behind"
        );
        assert!(
            should_update_with(Some("0.0.1"), &c, NO_SUPERVISOR),
            "ahead — still follow"
        );
        assert!(
            !should_update_with(Some(env!("CARGO_PKG_VERSION")), &c, NO_SUPERVISOR),
            "already matching"
        );
        // An older control plane says nothing; silence is not an instruction.
        assert!(!should_update_with(None, &c, NO_SUPERVISOR));
        // And supervision still gates it: no service, nothing detected.
        assert!(!should_update_with(
            Some("99.0.0"),
            &cfg(None),
            NO_SUPERVISOR
        ));
    }

    /// A node enrolled before `service` existed is supervised in fact and
    /// silent about it in config. Every node in the fleet was in that state,
    /// so it sat one version behind forever — and said nothing. When a
    /// supervisor IS detected, that node updates despite the empty config.
    #[test]
    fn a_detected_supervisor_overrides_empty_config() {
        let c = cfg(None);
        assert!(
            !supervised_with(&c, NO_SUPERVISOR),
            "no config and nothing detected — must refuse"
        );
        assert!(
            supervised_with(&c, DETECTED),
            "detected a supervisor, so something WILL restart us"
        );
    }

    /// Docker must never be second-guessed. A container is restarted by its
    /// runtime, so detection would say yes — and rewriting a binary inside a
    /// layer that vanishes on the next `up` is exactly the wrong move. Config
    /// beats inference in the one case inference is confidently wrong.
    #[test]
    fn docker_is_never_overridden_by_detection() {
        let c = cfg(Some("docker"));
        assert!(
            !supervised_with(&c, DETECTED),
            "a container updates by pulling an image, not by rewriting its binary"
        );
        assert!(refusal_with(&c, DETECTED).is_some_and(|r| r.contains("image")));
    }
}
