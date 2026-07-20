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
    /// Register this machine with a NookOS control plane.
    Join {
        /// Control plane URL, e.g. https://nook.example.com
        #[arg(long)]
        server: String,
        /// Join token from the NookOS UI (nook_join_…)
        #[arg(long)]
        token: String,
        /// Node name (defaults to this machine's hostname)
        #[arg(long)]
        name: Option<String>,
        /// Where to look for workspaces (repeatable)
        #[arg(long = "workspace-root")]
        workspace_roots: Vec<String>,
    },
    /// Run the agent (persistent connection to the control plane).
    Run,
    /// Show this node's configuration and connectivity.
    Status,
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
        Command::Join {
            server,
            token,
            name,
            workspace_roots,
        } => join(server, token, name, workspace_roots).await,
        Command::Run => {
            let cfg = NodeConfig::load()?;
            // Reaches sessions that already exist (mouse/scrollback/clipboard).
            tmux::apply_server_defaults();
            conn::run(cfg).await
        }
        Command::Status => status().await,
    }
}

async fn join(
    server: String,
    token: String,
    name: Option<String>,
    workspace_roots: Vec<String>,
) -> Result<()> {
    let server = server.trim_end_matches('/').to_string();
    let caps = capabilities::detect();

    ok("Validating token...");
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{server}/api/v1/nodes/join"))
        .json(&JoinRequest {
            token,
            name: name.unwrap_or_else(|| caps.hostname.clone()),
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

    let workspace_roots = if workspace_roots.is_empty() {
        vec!["~/workspace".to_string()]
    } else {
        workspace_roots
    };
    let cfg = NodeConfig {
        server,
        node_id: joined.node_id.to_string(),
        node_name: joined.node_name.clone(),
        node_token: joined.node_token.clone(),
        workspace_roots: workspace_roots.clone(),
    };
    cfg.save()?;

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
