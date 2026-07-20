mod capabilities;
mod config;
mod conn;
mod discovery;
mod gitops;
mod resources;
mod sessions;
mod ssh;
mod tmux;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use nook_types::{Capabilities, JoinRequest, JoinResponse};
use tracing_subscriber::EnvFilter;

use config::NodeConfig;

#[derive(Parser)]
#[command(name = "nook", about = "NookOS node agent", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Interactive first-time setup: walks through server, token, workspace
    /// root, and SSH key choice, then joins and prints service instructions.
    Setup,
    /// Register this machine non-interactively (flags and/or a config file —
    /// the automation path; humans usually want `nook setup`).
    Join {
        /// Control plane URL, e.g. https://nook.example.com
        #[arg(long)]
        server: Option<String>,
        /// Join token from the NookOS UI (nook_join_…)
        #[arg(long)]
        token: Option<String>,
        /// Node name (defaults to this machine's hostname)
        #[arg(long)]
        name: Option<String>,
        /// Where to look for workspaces (repeatable)
        #[arg(long = "workspace-root")]
        workspace_roots: Vec<String>,
        /// SSH private key for git operations (defaults to a generated key)
        #[arg(long)]
        ssh_key: Option<String>,
        /// TOML file with the same fields (server, token, name,
        /// workspace_roots, ssh_key_path); "-" reads stdin. Flags win.
        #[arg(long)]
        config: Option<String>,
    },
    /// Run the agent (persistent connection to the control plane).
    Run,
    /// Show this node's configuration and connectivity.
    Status,
}

/// Everything `join` needs, assembled from flags, a config file, or prompts.
#[derive(Debug, Default, serde::Deserialize)]
struct JoinSpec {
    server: Option<String>,
    token: Option<String>,
    name: Option<String>,
    #[serde(default)]
    workspace_roots: Vec<String>,
    ssh_key_path: Option<String>,
}

fn ok(line: &str) {
    println!("\u{2713} {line}");
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    match Cli::parse().command {
        Command::Setup => match setup_wizard()? {
            SetupPlan::Join(spec) => join(spec).await,
            SetupPlan::LocalUpdate {
                workspace_roots,
                ssh_key_path,
            } => apply_local_update(workspace_roots, ssh_key_path),
        },
        Command::Join {
            server,
            token,
            name,
            workspace_roots,
            ssh_key,
            config,
        } => {
            // Config file (or stdin) supplies defaults; flags win.
            let mut spec = match config.as_deref() {
                Some("-") => {
                    let mut raw = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut raw)?;
                    toml::from_str::<JoinSpec>(&raw).context("bad join config on stdin")?
                }
                Some(path) => {
                    let raw = std::fs::read_to_string(path)
                        .with_context(|| format!("cannot read {path}"))?;
                    toml::from_str::<JoinSpec>(&raw).context("bad join config file")?
                }
                None => JoinSpec::default(),
            };
            if server.is_some() {
                spec.server = server;
            }
            if token.is_some() {
                spec.token = token;
            }
            if name.is_some() {
                spec.name = name;
            }
            if !workspace_roots.is_empty() {
                spec.workspace_roots = workspace_roots;
            }
            if ssh_key.is_some() {
                spec.ssh_key_path = ssh_key;
            }
            join(spec).await
        }
        Command::Run => {
            let cfg = NodeConfig::load()?;
            // Reaches sessions that already exist (mouse/scrollback/clipboard).
            tmux::apply_server_defaults();
            conn::run(cfg).await
        }
        Command::Status => status().await,
    }
}

fn prompt(question: &str, default: Option<&str>) -> Result<String> {
    use std::io::Write;
    match default {
        Some(d) => print!("{question} [{d}]: "),
        None => print!("{question}: "),
    }
    std::io::stdout().flush()?;
    let mut line = String::new();
    // EOF must abort, not loop forever on empty answers (piped/closed stdin).
    if std::io::stdin().read_line(&mut line)? == 0 {
        anyhow::bail!("input closed — setup aborted");
    }
    let line = line.trim();
    if line.is_empty() {
        return Ok(default.unwrap_or_default().to_string());
    }
    Ok(line.to_string())
}

/// What the wizard decided: a (re-)join, or a local settings update that
/// keeps the existing registration.
enum SetupPlan {
    Join(JoinSpec),
    LocalUpdate {
        workspace_roots: Vec<String>,
        ssh_key_path: Option<String>,
    },
}

/// Interactive setup. Re-runnable: existing values become the defaults, and
/// when the registration (server + name) is unchanged you can skip the token
/// — settings update in place without re-joining.
fn setup_wizard() -> Result<SetupPlan> {
    println!("◆ NookOS node setup");
    println!("  This machine becomes a node: workspaces live here, sessions run here.");
    println!();

    let existing = NodeConfig::load().ok();
    if let Some(cfg) = &existing {
        println!(
            "  Currently joined as '{}' → {} — press Enter to keep any value.",
            cfg.node_name, cfg.server
        );
        println!();
    }

    let server_default = existing
        .as_ref()
        .map(|c| c.server.clone())
        .unwrap_or_else(|| "https://nook.example.com".into());
    let server = loop {
        let s = prompt("Control plane URL", Some(&server_default))?;
        if s.starts_with("http://") || s.starts_with("https://") {
            break s;
        }
        println!("  Please enter a full URL (https://…).");
    };

    let hostname = sysinfo::System::host_name().unwrap_or_else(|| "node".into());
    let name_default = existing
        .as_ref()
        .map(|c| c.node_name.clone())
        .unwrap_or(hostname);
    let name = prompt("Node name", Some(&name_default))?;

    let root_default = existing
        .as_ref()
        .and_then(|c| c.workspace_roots.first().cloned())
        .unwrap_or_else(|| "~/.nook/workspace".into());
    let root = prompt(
        "Workspace root (repos live under this directory)",
        Some(&root_default),
    )?;

    // SSH key: the node's own generated key (private key never leaves this
    // machine — recommended) or an existing key the user already uses.
    println!();
    println!("SSH key for cloning private repositories:");
    let current_key = existing.as_ref().and_then(|c| c.ssh_key_path.clone());
    println!(
        "  [1] Dedicated key for this node{}",
        if current_key.is_none() && existing.is_some() {
            " (current)"
        } else if current_key.is_none() {
            " (recommended)"
        } else {
            ""
        }
    );
    let mut choices: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        if let Ok(entries) = std::fs::read_dir(format!("{home}/.ssh")) {
            for e in entries.flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "pub") {
                    let private = p.with_extension("");
                    if private.exists() {
                        choices.push(private);
                    }
                }
            }
        }
    }
    choices.sort();
    let mut default_choice = "1".to_string();
    for (i, key) in choices.iter().enumerate() {
        let display = key.display().to_string();
        let marker = if Some(&display) == current_key.as_ref() {
            default_choice = (i + 2).to_string();
            " (current)"
        } else {
            ""
        };
        println!("  [{}] Use existing {display}{marker}", i + 2);
    }
    let ssh_key_path = loop {
        let pick = prompt("Choice", Some(&default_choice))?;
        match pick.parse::<usize>() {
            Ok(1) => break None,
            Ok(n) if n >= 2 && n - 2 < choices.len() => {
                break Some(choices[n - 2].display().to_string())
            }
            _ => println!("  Enter a number from the list."),
        }
    };
    println!();

    // Same registration → the token is optional; blank means "keep it" and
    // only the local settings change. New/changed registration needs a token.
    let same_registration = existing
        .as_ref()
        .is_some_and(|c| c.server == server && c.node_name == name);
    let token = loop {
        let hint = if same_registration {
            "Join token (Enter = keep current registration)"
        } else {
            "Join token (UI → Nodes → new join token)"
        };
        let t = prompt(hint, None)?;
        if !t.is_empty() || same_registration {
            break t;
        }
        println!("  A token is required to register with {server} as '{name}'.");
    };
    println!();

    if token.is_empty() {
        return Ok(SetupPlan::LocalUpdate {
            workspace_roots: vec![root],
            ssh_key_path,
        });
    }
    Ok(SetupPlan::Join(JoinSpec {
        server: Some(server),
        token: Some(token),
        name: Some(name),
        workspace_roots: vec![root],
        ssh_key_path,
    }))
}

/// Apply a token-less reconfigure: keep the registration, update settings.
fn apply_local_update(workspace_roots: Vec<String>, ssh_key_path: Option<String>) -> Result<()> {
    let mut cfg = NodeConfig::load()?;
    cfg.workspace_roots = workspace_roots;
    cfg.ssh_key_path = ssh_key_path;
    cfg.save()?;
    ok("Settings updated (registration unchanged).");
    if let Some(pubkey) = ssh::public_key_for(cfg.ssh_key_path.as_deref()) {
        println!();
        println!("SSH public key (add as a deploy key on your git host):");
        println!("{pubkey}");
    }
    println!();
    println!("Restart the agent to apply: sudo systemctl restart nook-node");
    Ok(())
}

async fn join(spec: JoinSpec) -> Result<()> {
    let server = spec
        .server
        .context("server is required (--server, config file, or `nook setup`)")?
        .trim_end_matches('/')
        .to_string();
    let token = spec
        .token
        .context("token is required (--token, config file, or `nook setup`)")?;
    let caps = capabilities::detect();

    ok("Validating token...");
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{server}/api/v1/nodes/join"))
        .json(&JoinRequest {
            token,
            name: spec.name.unwrap_or_else(|| caps.hostname.clone()),
            hostname: caps.hostname.clone(),
            platform: caps.platform.clone(),
        })
        .send()
        .await
        .context("could not reach the control plane")?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        anyhow::bail!("join token was rejected (expired or invalid)");
    }
    let joined: JoinResponse = resp
        .error_for_status()
        .context("join failed")?
        .json()
        .await?;
    ok("Registering node...");

    print_detections(&caps);

    if !caps.tmux {
        anyhow::bail!("tmux is required — install tmux and re-run `nook join`");
    }

    let workspace_roots = if spec.workspace_roots.is_empty() {
        vec!["~/workspace".to_string()]
    } else {
        spec.workspace_roots
    };
    let cfg = NodeConfig {
        server,
        node_id: joined.node_id.to_string(),
        node_name: joined.node_name.clone(),
        node_token: joined.node_token.clone(),
        workspace_roots: workspace_roots.clone(),
        ssh_key_path: spec.ssh_key_path.clone(),
    };
    cfg.save()?;

    // Surface the deploy key so private clones can be authorized right away.
    if let Some(pubkey) = ssh::public_key_for(cfg.ssh_key_path.as_deref()) {
        println!();
        println!("SSH public key (add as a deploy key on your git host):");
        println!("{pubkey}");
    }

    ok("Creating persistent connection...");
    // Prove the WebSocket path works, then hand off to `nook run`.
    let connected =
        tokio::time::timeout(std::time::Duration::from_secs(10), probe_connection(&cfg))
            .await
            .unwrap_or(false);

    println!();
    println!("Node Name:\n{}", joined.node_name);
    println!();
    println!("Workspace Root:\n{}", workspace_roots.join(", "));
    println!();
    println!(
        "Status:\n{}",
        if connected {
            "Connected"
        } else {
            "Registered (start with `nook run`)"
        }
    );
    println!();
    println!("Start the agent with: nook run");
    Ok(())
}

/// Open the WS, send Register, wait for the ack, close.
async fn probe_connection(cfg: &NodeConfig) -> bool {
    use futures_util::{SinkExt, StreamExt};
    use nook_proto::{ControlToNode, NodeToControl};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::Message;

    let Ok(mut request) = conn::ws_url(&cfg.server).into_client_request() else {
        return false;
    };
    let Ok(auth) = format!("Bearer {}", cfg.node_token).parse() else {
        return false;
    };
    request.headers_mut().insert(
        tokio_tungstenite::tungstenite::http::header::AUTHORIZATION,
        auth,
    );
    let Ok((mut socket, _)) = tokio_tungstenite::connect_async(request).await else {
        return false;
    };
    let register = NodeToControl::Register {
        capabilities: capabilities::detect(),
        live_tmux_sessions: tmux::list_nook_sessions(),
    };
    let Ok(json) = serde_json::to_string(&register) else {
        return false;
    };
    if socket.send(Message::Text(json.into())).await.is_err() {
        return false;
    }
    while let Some(Ok(msg)) = socket.next().await {
        if let Message::Text(t) = msg {
            if let Ok(ControlToNode::RegisterAck { .. }) = serde_json::from_str(&t) {
                let _ = socket.close(None).await;
                return true;
            }
        }
    }
    false
}

fn print_detections(caps: &Capabilities) {
    ok(&format!(
        "Detecting operating system... {} ({})",
        caps.platform, caps.architecture
    ));
    ok(&format!("Detecting CPU... {} cores", caps.cpus));
    if caps.gpus.is_empty() {
        ok("Detecting GPU... none");
    } else {
        for gpu in &caps.gpus {
            ok(&format!("Detecting GPU... {} {}", gpu.vendor, gpu.model));
        }
    }
    ok(&format!(
        "Detecting Docker... {}",
        if caps.docker { "\u{2713}" } else { "\u{2717}" }
    ));
    ok(&format!(
        "Detecting tmux... {}",
        if caps.tmux { "\u{2713}" } else { "\u{2717}" }
    ));
    ok(&format!(
        "Detecting git... {}",
        caps.git.as_deref().unwrap_or("\u{2717}")
    ));
    ok("Detecting installed runtimes...");
    println!();
    for (label, bin) in [
        ("Claude Code", "claude"),
        ("Hermes", "hermes"),
        ("Codex", "codex"),
    ] {
        let mark = if caps.runtimes.iter().any(|r| r == bin) {
            "\u{2713}"
        } else {
            "\u{2717}"
        };
        println!("  {label:<13} {mark}");
    }
    println!();
}

async fn status() -> Result<()> {
    let cfg = NodeConfig::load()?;
    println!("Node:            {}", cfg.node_name);
    println!("Server:          {}", cfg.server);
    println!("Workspace roots: {}", cfg.workspace_roots.join(", "));
    let healthy = reqwest::Client::new()
        .get(format!("{}/healthz", cfg.server.trim_end_matches('/')))
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false);
    println!(
        "Control plane:   {}",
        if healthy { "reachable" } else { "unreachable" }
    );
    Ok(())
}
