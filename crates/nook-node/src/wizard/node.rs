//! `nook setup` — turn this machine into a node.
//!
//! Enrols rather than joins wherever it can. A join token buys a bearer token
//! that lives in a file on a machine running other people's code; enrolment
//! buys a certificate whose private half never leaves. Falls back to joining
//! when the control plane advertises no fingerprint, so an older instance still
//! works rather than failing at the last step.

use anyhow::{Context, Result};

use super::service;
use super::tty;
use crate::config::NodeConfig;

#[derive(Debug, Default)]
pub struct SetupArgs {
    pub server: Option<String>,
    pub agent_url: Option<String>,
    pub token: Option<String>,
    pub name: Option<String>,
    pub fingerprint: Option<String>,
}

pub async fn setup(args: SetupArgs) -> Result<()> {
    let mut t = tty::require("nook setup --server https://nook.example.com --token nook_join_…")?;

    let existing = NodeConfig::load().ok();
    t.say("");
    t.say("  NookOS node");
    t.say("  This machine will run workspaces and agent sessions.");
    if let Some(c) = &existing {
        t.say(&format!(
            "  Already registered as '{}' → {}. Press Enter to keep any value.",
            c.node_name, c.server
        ));
    }
    t.say("");

    // ---- where
    let server = match args.server {
        Some(s) => s,
        None => {
            let d = existing
                .as_ref()
                .map(|c| c.server.clone())
                .unwrap_or_else(|| "https://nook.example.com".into());
            loop {
                let v = t.text("Control plane URL", Some(&d))?;
                if v.starts_with("http://") || v.starts_with("https://") {
                    break v;
                }
                t.say("  Include the scheme, e.g. https://nook.example.com");
            }
        }
    };
    let server = server.trim_end_matches('/').to_string();

    let name = match args.name {
        Some(n) => n,
        None => {
            let d = existing
                .as_ref()
                .map(|c| c.node_name.clone())
                .unwrap_or_else(|| sysinfo::System::host_name().unwrap_or_else(|| "node".into()));
            t.text("Node name", Some(&d))?
        }
    };

    let workspace_root = {
        let d = existing
            .as_ref()
            .and_then(|c| c.workspace_roots.first().cloned())
            .unwrap_or_else(|| "~/.nook/workspace".into());
        t.text("Workspace root (repos live under this directory)", Some(&d))?
    };

    // ---- credential
    let token = match args.token {
        Some(t) => Some(t),
        None if existing.is_some() => {
            t.say("");
            t.say("A join token re-registers this machine. Leave blank to keep the");
            t.say("current registration and only change settings.");
            t.optional("Join token")?
        }
        None => Some(t.text("Join token (from the UI: Nodes → add node)", None)?),
    };

    if let Some(token) = token {
        // The agent endpoint is not always the API's: TLS for node connections
        // terminates in the control plane itself, so deployments routinely give
        // it its own name.
        let agent_url = args
            .agent_url
            .unwrap_or_else(|| server.clone())
            .trim_end_matches('/')
            .to_string();

        t.say("");
        t.say(&format!("▸ Enrolling with {agent_url}"));
        match crate::enroll::enroll(
            &token,
            Some(&agent_url),
            Some(&name),
            args.fingerprint.as_deref(),
        )
        .await
        {
            Ok(()) => {}
            Err(e) => {
                // Enrolment is the modern path; a control plane predating mTLS
                // has no /nodes/enroll at all. Say which happened rather than
                // silently downgrading the machine's credential.
                t.say(&format!("  Enrolment failed: {e}"));
                t.say("  Falling back to token authentication.");
                crate::join_legacy(&server, &token, &name).await?;
            }
        }
    }

    // Keep whatever else was configured; only the workspace root changed here.
    let mut cfg = NodeConfig::load().context("setup did not produce a config")?;
    cfg.workspace_roots = vec![workspace_root];
    cfg.save()?;

    // ---- tmux, before anything that depends on it
    //
    // Checked here rather than at first use: a node that joins and then cannot
    // open a terminal has failed in the least diagnosable way possible.
    let has_tmux = super::tmux_setup::ensure(&mut t)?;

    // ---- keep it running
    let exec = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "nook".into());
    let svc = service::choose(&mut t)?;
    service::install(&mut t, svc, &exec)?;

    // Remembered so the agent knows whether anything would restart it.
    let mut cfg = NodeConfig::load()?;
    cfg.service = svc.config_value().map(str::to_string);
    cfg.save()?;

    // ---- offer the skill
    t.say("");
    if t.confirm(
        "Install the NookOS skill for your agents on this machine?",
        true,
    )? {
        super::skills::install(None, false)?;
    }

    t.say("");
    t.say("────────────────────────────────────────────────────────────");
    t.say(&format!("  '{name}' is set up."));
    if !has_tmux {
        t.say("");
        t.say("  ⚠ tmux is still missing — this node will appear online but");
        t.say("    every session will fail to open until it is installed.");
    }
    t.say("");
    t.say("  Open the control plane and it should be listed as online.");
    t.say("────────────────────────────────────────────────────────────");
    Ok(())
}
