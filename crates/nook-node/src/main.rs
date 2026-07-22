mod capabilities;
mod cli;
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
    Run {
        /// LOCAL DEV ONLY: allow an unencrypted/unverified control plane.
        /// Refused when APP_ENV=production. Prefer an https:// server.
        #[arg(long)]
        insecure_skip_verify: bool,
    },
    /// Replace this binary with the build the control plane is serving, so
    /// every machine in the fleet runs the same version as the server.
    Update,
    /// Show this node's configuration and connectivity.
    Status,

    /// List resources from the control plane, kubectl-style:
    /// `nook get nodes`, `nook get sessions`, `nook get secrets`.
    Get {
        /// nodes | sessions | workspaces | secrets | tasks | events | themes
        resource: String,
        /// Narrow to one by name (or, for secrets, one workspace).
        name: Option<String>,
        /// Print raw JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Adopt a git repository as a workspace on this node. Works from
    /// anywhere: a repo outside the workspace roots is placed at
    /// <root>/<org>/<repo>, derived from its remote.
    Import {
        /// Repository directory (defaults to the current one).
        path: Option<String>,
        /// Symlink it into place instead of moving it, so the working copy
        /// stays exactly where it is.
        #[arg(long)]
        link: bool,
    },
    /// Delete a session, workspace or task by name.
    Delete {
        /// sessions | workspaces | tasks
        resource: String,
        name: String,
    },

    /// Act as yourself rather than as this machine, so the CLI can drive the
    /// whole fleet: `nook login --token nook_user_…`.
    Login {
        /// A user token from Settings → Access tokens.
        #[arg(long)]
        token: String,
        /// Control plane URL (defaults to the one this machine joined).
        #[arg(long)]
        server: Option<String>,
    },
    /// Which credential is this CLI using, and for whom?
    Whoami,
    /// Upload a built binary so the fleet can install it — how a macOS build
    /// made on a Mac reaches a Linux-built control plane.
    Publish {
        /// The binary to upload (e.g. target/release/nook).
        file: String,
        /// Version to publish under (defaults to the server's own).
        #[arg(long)]
        version: Option<String>,
        /// Artifact name (defaults to this machine's platform, e.g. nook-darwin-aarch64).
        #[arg(long)]
        artifact: Option<String>,
    },
    /// Forget the user token; fall back to this machine's node token.
    Logout,

    /// Open a session on any node in the fleet: `nook start my-repo --runtime claude`.
    Start {
        /// Workspace name, slug or id.
        workspace: String,
        /// Which machine to run it on (defaults to any online node with a checkout).
        #[arg(long)]
        node: Option<String>,
        /// claude | hermes | codex | bash | zsh | …
        #[arg(long, default_value = "bash")]
        runtime: String,
        /// Name the session (defaults to a generated one).
        #[arg(long)]
        name: Option<String>,
    },
    /// Type into a session, wherever it lives: `nook send api-work 'run the tests'`.
    Send {
        /// Session name or id.
        session: String,
        /// What to type.
        text: Vec<String>,
        /// Don't press Enter afterwards.
        #[arg(long)]
        no_enter: bool,
    },
    /// Show what a session is displaying right now.
    Read {
        /// Session name or id.
        session: String,
        /// Scrollback lines to include above the visible screen.
        #[arg(long, default_value_t = 0)]
        lines: u32,
        /// Screen only — no runtime/status header.
        #[arg(long)]
        quiet: bool,
    },
    /// Send a prompt and wait for the reply: `nook exec review 'summarize the diff'`.
    Exec {
        /// Session name or id.
        session: String,
        /// The prompt.
        text: Vec<String>,
        /// Give up waiting after this many seconds.
        #[arg(long, default_value_t = 120)]
        timeout: u64,
        /// Scrollback lines to include in the reply.
        #[arg(long, default_value_t = 200)]
        lines: u32,
    },
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
        Command::Update => update_binary().await,
        Command::Run {
            insecure_skip_verify,
        } => {
            if insecure_skip_verify {
                // Flag and env var are equivalent; funnel to one place so the
                // checks downstream only have to read the environment.
                std::env::set_var("NOOK_INSECURE", "1");
            }
            let cfg = NodeConfig::load()?;
            // Reaches sessions that already exist (mouse/scrollback/clipboard).
            tmux::apply_server_defaults();
            conn::run(cfg).await
        }
        Command::Status => status().await,
        Command::Get {
            resource,
            name,
            json,
        } => cli::get(&resource, name.as_deref(), json).await,
        Command::Import { path, link } => cli::import(path.as_deref(), link).await,
        Command::Delete { resource, name } => cli::delete(&resource, &name).await,
        Command::Login { token, server } => cli::login(&token, server.as_deref()).await,
        Command::Whoami => cli::whoami().await,
        Command::Publish {
            file,
            version,
            artifact,
        } => cli::publish(&file, version.as_deref(), artifact.as_deref()).await,
        Command::Logout => cli::logout(),
        Command::Start {
            workspace,
            node,
            runtime,
            name,
        } => cli::start(&workspace, node.as_deref(), &runtime, name.as_deref()).await,
        Command::Send {
            session,
            text,
            no_enter,
        } => cli::send(&session, &text.join(" "), !no_enter).await,
        Command::Read {
            session,
            lines,
            quiet,
        } => cli::read(&session, lines, quiet).await,
        Command::Exec {
            session,
            text,
            timeout,
            lines,
        } => cli::exec(&session, &text.join(" "), timeout, lines).await,
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

/// `nook update` — pull the binary this node's control plane is serving.
///
/// Self-hosted fleets drift because updating is a chore done per machine, so
/// the agent updates itself from the server it is already talking to: the
/// version that answers is by definition the version that matches.
///
/// Written to a temp file and renamed into place, never overwritten: the
/// running binary is this file, and writing over it fails with ETXTBSY.
async fn update_binary() -> Result<()> {
    let cfg = NodeConfig::load()?;
    let server = cfg.server.trim_end_matches('/');
    let (os, arch) = target_platform()?;
    let artifact = format!("nook-{os}-{arch}");
    let url = format!("{server}/dist/{artifact}");

    println!("▸ fetching {artifact} from {server}");
    let resp = reqwest::Client::new()
        .get(&url)
        .send()
        .await
        .with_context(|| format!("cannot reach {url}"))?;
    if !resp.status().is_success() {
        anyhow::bail!(
            "{server} has no build for {os}/{arch} ({}). Build from source or \
             add the artifact to the control plane's dist directory.",
            resp.status()
        );
    }
    let bytes = resp.bytes().await?;

    let current = std::env::current_exe().context("cannot locate the running binary")?;
    let dir = current
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let staged = dir.join(format!(".nook-update-{}", std::process::id()));
    std::fs::write(&staged, &bytes).with_context(|| {
        format!(
            "cannot write {} — is {} writable?",
            staged.display(),
            dir.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&staged, &current)
        .with_context(|| format!("cannot replace {}", current.display()))?;

    ok(&format!("updated {}", current.display()));
    println!("  Restart the agent to run it: systemctl restart nook-node");
    Ok(())
}

/// This machine, named the way the control plane names artifacts.
fn target_platform() -> Result<(&'static str, &'static str)> {
    let os = match std::env::consts::OS {
        "linux" => "linux",
        "macos" => "darwin",
        other => anyhow::bail!("no published build for {other} — build from source"),
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => anyhow::bail!("no published build for {other}"),
    };
    Ok((os, arch))
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

    // First contact is the worst moment to be unencrypted: this exchange hands
    // the machine its credential.
    let insecure = crate::config::check_server_security(&server, false)?;
    crate::config::warn_if_insecure(insecure, &server);

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
