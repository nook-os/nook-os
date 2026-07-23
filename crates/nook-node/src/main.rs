mod capabilities;
mod cli;
mod config;
mod conn;
mod device_login;
mod discovery;
mod enroll;
mod gitops;
mod pinning;
mod resources;
mod sessions;
mod ssh;
mod style;
mod tmux;
mod wizard;

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
    /// Interactive first-time setup: server, token, workspace root, SSH key
    /// and how the agent should stay running. Re-runnable.
    ///
    /// Flags pre-answer questions; anything left out is prompted for.
    Setup {
        #[arg(long)]
        server: Option<String>,
        /// Where the agent connects, when that differs from the API.
        #[arg(long)]
        agent_url: Option<String>,
        #[arg(long)]
        token: Option<String>,
        #[arg(long)]
        name: Option<String>,
        /// SHA-256 of the control plane's certificate, from the join token.
        #[arg(long)]
        fingerprint: Option<String>,
    },
    /// Install the NookOS skill so your agents can drive the fleet themselves.
    #[command(subcommand)]
    Skills(SkillsCommand),
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
    /// Trade a join token for this machine's own certificate (mutual TLS).
    ///
    /// The private key is generated here and never leaves — the control plane
    /// only ever sees a signing request.
    Enroll {
        /// Join token from the NookOS UI (nook_join_…)
        #[arg(long)]
        token: String,
        /// Control plane URL. Defaults to the one this machine already joined.
        #[arg(long)]
        server: Option<String>,
        /// Node name for a machine enrolling for the first time.
        #[arg(long)]
        name: Option<String>,
        /// SHA-256 of the control plane's certificate, from the join token.
        /// Without it, enrolment trusts whatever the web PKI vouches for.
        #[arg(long)]
        server_fingerprint: Option<String>,
    },
    /// Renew this machine's certificate using the key it already holds.
    /// No join token: a machine that has been offline renews itself.
    Renew,
    /// Control-plane administration.
    #[command(subcommand)]
    Server(ServerCommand),
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
        ///
        /// Omit it to sign in through your identity provider instead: the
        /// browser opens, you approve, and no token is ever copied by hand.
        #[arg(long)]
        token: Option<String>,
        /// Control plane URL (defaults to the one this machine joined).
        #[arg(long)]
        server: Option<String>,
    },
    /// Which credential is this CLI using, and for whom?
    Whoami,
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
    /// SHA-256 of the control plane's certificate, pinned from here on.
    #[serde(default)]
    server_fingerprint: Option<String>,
    token: Option<String>,
    name: Option<String>,
    #[serde(default)]
    workspace_roots: Vec<String>,
    ssh_key_path: Option<String>,
}

fn ok(line: &str) {
    println!("{}", style::success(line));
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    // rustls refuses to guess when more than one provider is compiled in, and
    // several dependencies pull it with different feature sets. Left unset it
    // panics at the moment a TLS config is built — which is the moment the
    // agent connects, so the failure only ever shows up at runtime on a real
    // machine. Choose explicitly, at the top, before anything can need it.
    let _ = rustls::crypto::ring::default_provider().install_default();

    match Cli::parse().command {
        Command::Setup {
            server,
            agent_url,
            token,
            name,
            fingerprint,
        } => {
            wizard::node::setup(wizard::node::SetupArgs {
                server,
                agent_url,
                token,
                name,
                fingerprint,
            })
            .await
        }
        Command::Skills(SkillsCommand::Install { dir, quiet }) => {
            wizard::skills::install(dir, quiet)
        }
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
        Command::Enroll {
            ref token,
            ref server,
            ref name,
            ref server_fingerprint,
        } => {
            enroll::enroll(
                token,
                server.as_deref(),
                name.as_deref(),
                server_fingerprint.as_deref(),
            )
            .await
        }
        Command::Renew => enroll::renew().await,
        Command::Server(ServerCommand::Init {
            dir,
            version,
            dry_run,
        }) => wizard::server::init(wizard::server::InitOptions {
            dir,
            // Pin to the version that generated it: the images and this binary
            // come out of the same release, so they are known to agree.
            version: version.unwrap_or_else(|| format!("v{}", env!("CARGO_PKG_VERSION"))),
            dry_run,
        }),
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
        Command::Login { token, server } => match token {
            Some(t) => cli::login(&t, server.as_deref()).await,
            None => cli::login_with_provider(server.as_deref()).await,
        },
        Command::Whoami => cli::whoami().await,
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

#[derive(clap::Subcommand)]
enum SkillsCommand {
    /// Write the skill into every agent installation found on this machine.
    Install {
        /// Install into this directory instead of auto-detecting.
        #[arg(long)]
        dir: Option<std::path::PathBuf>,
        #[arg(long)]
        quiet: bool,
    },
}

#[derive(clap::Subcommand)]
enum ServerCommand {
    /// Stand up a control plane here: generates secrets, writes the deployment
    /// files, and brings it up.
    Init {
        /// Where to write the deployment. Prompted for when omitted.
        #[arg(long)]
        dir: Option<std::path::PathBuf>,
        /// Image tag to pin. Defaults to this binary's version.
        #[arg(long)]
        version: Option<String>,
        /// Print what would be written and exit.
        #[arg(long)]
        dry_run: bool,
    },
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

    // Ask the control plane which build to take, then fetch the bytes from
    // where they actually live. It knows the version; GitHub serves the file.
    let client = cli::Client::from_config()?;
    let releases = client.get("/api/v1/node/releases").await?;
    let url = releases
        .get("artifacts")
        .and_then(|a| a.as_array())
        .and_then(|list| {
            list.iter()
                .find(|a| a.get("filename").and_then(|f| f.as_str()) == Some(artifact.as_str()))
        })
        .and_then(|a| a.get("url").and_then(|u| u.as_str()))
        .map(str::to_string)
        .with_context(|| format!("{server} lists no build for {os}/{arch}"))?;

    println!("▸ fetching {artifact} from {url}");
    let resp = reqwest::Client::new()
        .get(&url)
        .send()
        .await
        .with_context(|| format!("cannot reach {url}"))?;
    if !resp.status().is_success() {
        anyhow::bail!(
            "no published build for {os}/{arch} ({}). Releases live at {url} — \
             either that platform has not been published yet, or build from \
             source: cargo build --release -p nook-node",
            resp.status()
        );
    }
    let bytes = resp.bytes().await?;

    // Verify against the checksum published beside the binary, exactly as
    // install.sh does. Without this the update path — the one that would run
    // unattended across a whole fleet — is the least checked way to get a
    // binary onto a machine, which is precisely backwards. A missing checksum
    // is fatal rather than skipped: an unverifiable update is one to refuse,
    // not to shrug at.
    let sum_url = format!("{url}.sha256");
    let published = reqwest::Client::new()
        .get(&sum_url)
        .send()
        .await
        .with_context(|| format!("cannot reach {sum_url}"))?;
    if !published.status().is_success() {
        anyhow::bail!(
            "no checksum published at {sum_url} ({}) — refusing to install a \
             binary that cannot be verified",
            published.status()
        );
    }
    let expected = published
        .text()
        .await?
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let actual = {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(&bytes))
    };
    if actual != expected {
        anyhow::bail!(
            "checksum mismatch for {artifact}: expected {expected}, got {actual}. \
             Refusing to install."
        );
    }
    println!("✓ checksum verified");

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

/// The pre-mTLS path, kept only as a fallback for a control plane that has no
/// `/nodes/enroll`. Not reachable from a current install otherwise.
pub(crate) async fn join_legacy(server: &str, token: &str, name: &str) -> Result<()> {
    join(JoinSpec {
        server: Some(server.to_string()),
        server_fingerprint: None,
        token: Some(token.to_string()),
        name: Some(name.to_string()),
        workspace_roots: Vec::new(),
        ssh_key_path: None,
    })
    .await
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
        // Set once the join flow carries a fingerprint; until then the node
        // relies on ordinary web-PKI validation for https.
        server_fingerprint: spec.server_fingerprint.clone(),
        // Joining does not know about the agent port; `nook enroll` sets it.
        agent_server: NodeConfig::load().ok().and_then(|c| c.agent_server),
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
    // `ok()` prefixes a ✓ unconditionally, so using it for a detection RESULT
    // printed "✓ Detecting tmux... ✗" — which reads as found. The marker has
    // to carry the answer, not decorate the question.
    fn found(label: &str, present: bool, detail: &str) {
        let mark = if present {
            style::ok_c("\u{2713}")
        } else {
            style::err("\u{2717}")
        };
        println!("{mark} {label} {}", style::dim(detail));
    }
    found(
        "Docker",
        caps.docker,
        if caps.docker { "" } else { "not found" },
    );
    found(
        "tmux",
        caps.tmux,
        &capabilities::detect_tmux().unwrap_or_else(|| "not found".into()),
    );
    found(
        "git",
        caps.git.is_some(),
        caps.git.as_deref().unwrap_or("not found"),
    );
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
