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
pub fn supervised(cfg: &NodeConfig) -> bool {
    matches!(
        cfg.service.as_deref(),
        Some("systemd-user") | Some("systemd-system") | Some("launchd")
    )
}

/// Why an update was refused, in words worth showing an operator.
pub fn refusal(cfg: &NodeConfig) -> Option<String> {
    if supervised(cfg) {
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
    let Some(expected) = expected else {
        return false;
    };
    expected != env!("CARGO_PKG_VERSION") && supervised(cfg)
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

    /// The check that keeps a fleet from going dark. An agent nothing restarts
    /// must refuse, and say why in terms an operator can act on.
    #[test]
    fn an_unsupervised_agent_refuses_to_update() {
        for s in [None, Some("none")] {
            let c = cfg(s);
            assert!(!supervised(&c));
            let why = refusal(&c).expect("must refuse");
            assert!(why.contains("nook setup"), "{why}");
        }
    }

    #[test]
    fn a_container_is_told_to_update_its_image() {
        let why = refusal(&cfg(Some("docker"))).expect("must refuse");
        assert!(why.contains("image"), "{why}");
    }

    #[test]
    fn supervised_agents_are_allowed() {
        for s in ["systemd-user", "systemd-system", "launchd"] {
            assert!(supervised(&cfg(Some(s))), "{s}");
            assert!(refusal(&cfg(Some(s))).is_none(), "{s}");
        }
    }

    /// Matching the control plane is the goal, not being newest — otherwise a
    /// deliberate downgrade would be undone by every node that noticed.
    #[test]
    fn it_follows_the_control_plane_in_either_direction() {
        let c = cfg(Some("systemd-user"));
        assert!(should_update(Some("99.0.0"), &c), "behind");
        assert!(should_update(Some("0.0.1"), &c), "ahead — still follow");
        assert!(
            !should_update(Some(env!("CARGO_PKG_VERSION")), &c),
            "already matching"
        );
        // An older control plane says nothing; silence is not an instruction.
        assert!(!should_update(None, &c));
        // And supervision still gates it.
        assert!(!should_update(Some("99.0.0"), &cfg(None)));
    }
}
