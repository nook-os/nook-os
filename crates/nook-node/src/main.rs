mod capabilities;
mod certs;
mod cli;
mod config;
mod conn;
mod contexts;
mod device_login;
mod discovery;
mod enroll;
mod gitops;
mod job_adapter;
mod loop_job;
mod pinning;
mod resources;
mod runtime_auth;
mod selfupdate;
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

// The `K8s` variant carries the wide `k8s init` flag set (host/OIDC/mail/…), so
// it dwarfs the others. The lint guards against a big variant bloating a
// frequently-moved enum; this one is parsed once at startup and never copied in
// a hot path, so boxing it would only add an allocation and fight clap's derive.
#[allow(clippy::large_enum_variant)]
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
    /// Teach a skill to every agent on every machine in the fleet.
    ///
    /// The control plane stores it and fans it out. Nodes that are offline —
    /// and nodes that join later — learn it when they connect, so this is not
    /// "copy a file to whoever happens to be awake".
    Teach {
        /// Path to a SKILL.md (or any markdown skill document).
        path: String,
        /// Override the name. Otherwise taken from the document's frontmatter
        /// `name:`, then from the filename.
        #[arg(long)]
        name: Option<String>,
    },
    /// What the fleet has been taught.
    Taught {
        #[arg(long)]
        json: bool,
    },
    /// Remove a taught skill from the control plane and from every machine.
    Unteach { name: String },
    /// List board tasks with the same filter an agent's pick step uses.
    ///
    /// `nook tasks --label agent-ready --assignee none --unblocked` is exactly
    /// what the loop asks for, so you can see what it will take next.
    Tasks {
        #[arg(long)]
        board: Option<String>,
        /// Require this label (repeatable).
        #[arg(long = "label")]
        labels: Vec<String>,
        /// Exclude this label (repeatable).
        #[arg(long = "not-label")]
        not_labels: Vec<String>,
        /// A user id, or `none` for unclaimed work.
        #[arg(long)]
        assignee: Option<String>,
        /// backlog | unstarted | started | completed | canceled
        #[arg(long = "column-type")]
        column_type: Option<String>,
        /// Issue type to include (repeatable): task|bug|epic|story|chore.
        /// `--type epic` lists epics, which are excluded by default.
        #[arg(long = "type")]
        types: Vec<String>,
        /// An epic's children (key or uuid): the tasks filed under it, including
        /// backlog ones.
        #[arg(long)]
        parent: Option<String>,
        /// Hide anything with an unresolved blocker.
        #[arg(long)]
        unblocked: bool,
        /// Narrow to work THIS machine should take: cards dispatched to it, plus
        /// everything undispatched. What a builder looping on a node wants —
        /// dispatch then means "this one is yours" instead of setting a field
        /// nothing read. Off by default, so a human's `nook tasks` still shows
        /// the whole board even on a machine that is also a node.
        #[arg(long = "this-node")]
        this_node: bool,
        /// Only this workspace (uuid or name). Defaults to the workspace of the
        /// session you are in, so an agent's pick stays inside its own repo.
        #[arg(long)]
        workspace: Option<String>,
        /// Ignore the session's workspace and list across the whole tenant.
        #[arg(long = "all-workspaces")]
        all_workspaces: bool,
        /// Include tasks in the backlog (Triage). Excluded by default: the
        /// backlog is a human refinement space the loop never draws from, and
        /// epics are excluded too — both are enforced server-side.
        #[arg(long)]
        backlog: bool,
        #[arg(long)]
        json: bool,
    },
    /// Read one whole issue: body, labels, comments, blockers.
    Task {
        /// Human key (NOOK-42) or id.
        key: String,
        #[arg(long)]
        json: bool,
    },
    /// Comment on a task.
    Comment {
        key: String,
        /// The comment body (markdown).
        body: Vec<String>,
    },
    /// Safely replace a task's description. Reads the current version and writes
    /// with an optimistic-concurrency guard, retrying on a concurrent edit — so
    /// it never silently loses your change. Read the body first with `nook task`.
    SetDescription {
        key: String,
        /// The new description (markdown).
        description: Vec<String>,
    },
    /// Add or remove a label.
    Label {
        key: String,
        name: String,
        #[arg(long)]
        remove: bool,
    },
    /// Create board objects (currently: a task).
    #[command(subcommand)]
    Create(CreateCommand),
    /// Relate two tasks: `nook relate <BLOCKER> blocks <DEPENDENT>`.
    ///
    /// Posts the relation on the BLOCKER. Kinds: blocks | relates | duplicates.
    /// Keys or uuids both work. After a `blocks`, it reports whether the
    /// dependent is now blocked.
    Relate {
        /// The blocking task (key or uuid).
        blocker: String,
        /// blocks | relates | duplicates
        kind: String,
        /// The dependent task (key or uuid).
        dependent: String,
    },
    /// Wire an agent's finish hook so it notifies the fleet when it is done.
    #[command(subcommand)]
    Hooks(HooksCommand),
    /// Switch between control planes — dev and production, say.
    ///
    /// A named credential, one of them current, and a command that says which.
    /// Without this, moving between two deployments meant re-running `nook
    /// login` and retyping a token, so in practice people hand-edit
    /// `auth.toml` — which is how production tooling ends up pointed at
    /// localhost without anybody noticing.
    #[command(subcommand)]
    Context(ContextCommand),
    /// Who may run this deployment.
    #[command(subcommand)]
    Operator(OperatorCommand),
    /// Durable human interactions: ask a human a question, answer one.
    ///
    /// An ask is persisted, announced over channels, and answerable from any
    /// surface — so a paused loop job or an in-session agent can wait on a
    /// human decision without losing it to a dropped connection (MAIN-159).
    #[command(subcommand)]
    Interactions(InteractionsCommand),
    /// Tell the fleet something happened.
    ///
    /// Fans out to every connected UI and every configured channel (Slack,
    /// Telegram, push, webhooks…). Ideal as an agent's finish hook:
    ///
    ///     nook notify "Claude finished" --level success
    Notify {
        /// The headline.
        title: Vec<String>,
        /// Longer detail.
        #[arg(long)]
        body: Option<String>,
        /// info | success | warning | error
        #[arg(long, default_value = "info")]
        level: String,
        /// Dotted kind that channels filter on, e.g. `agent.finished`.
        #[arg(long)]
        kind: Option<String>,
        /// Somewhere to go when clicked.
        #[arg(long)]
        link: Option<String>,
        /// The session this is about (usually `$NOOK_SESSION_ID`). The control
        /// plane turns it into a deep link to that terminal.
        #[arg(long)]
        session: Option<String>,
    },
    /// Which workspace is the session you are in? (`nook workspace current`)
    #[command(subcommand)]
    Workspace(WorkspaceCommand),
    /// Report the agent's state for this session (running|waiting|idle). A
    /// no-op outside a nook session; called by the Claude Code hooks.
    AgentState { state: String },
    /// Claim a task so nobody else takes it.
    Claim {
        key: String,
        /// Move it here at the same time, e.g. `started`.
        #[arg(long = "column-type")]
        column_type: Option<String>,
        /// Claim even if the task belongs to a different workspace than this
        /// session's. Off by default: the guard is what keeps an agent from
        /// building another repo's ticket.
        #[arg(long = "any-workspace")]
        any_workspace: bool,
    },
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
    /// Kubernetes: generate a Helm values file and print the install commands.
    #[command(subcommand)]
    K8s(K8sCommand),
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
    ///
    /// `nook get nodes` lists the machines you can see: your own, or — if you
    /// are a tenant owner/admin — the whole fleet (MAIN-132). The control plane
    /// scopes it by your role; there is no client flag to widen it.
    ///
    /// `nook get sessions` is scoped by role too: you see the sessions you
    /// started, or — as a tenant owner/admin — every session's metadata for
    /// capacity and audit (MAIN-133). Session content stays private regardless.
    Get {
        /// nodes | sessions | workspaces | secrets | tasks | events | themes
        resource: String,
        /// Narrow to one by name (or, for secrets, one workspace).
        name: Option<String>,
        /// Print raw JSON instead of a table.
        #[arg(long)]
        json: bool,
        /// Act in one of your other tenants. Slug or id. Overrides
        /// NOOK_TENANT_ID; without either you get your home tenant.
        #[arg(short = 'T', long)]
        tenant: Option<String>,
        /// Every tenant you belong to, with a TENANT column.
        #[arg(short = 'A', long = "all-tenants")]
        all_tenants: bool,
        /// Anything after that. `nook get workspace git-ssh` is invoked BY git
        /// as its `GIT_SSH_COMMAND`, which appends ssh's own arguments —
        /// `user@host`, `-o`, a remote command — so they have to be accepted
        /// here rather than rejected as unexpected (MAIN-367).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
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
    /// Restart the sessions of a workspace, the way `kubectl rollout restart`
    /// restarts a deployment: kill them and let the reconciler bring them back.
    Rollout {
        #[command(subcommand)]
        cmd: RolloutCmd,
    },

    /// Delete sessions, workspaces or tasks by name or id — several at once.
    Delete {
        /// sessions | workspaces | tasks
        resource: String,
        /// One or more names or ids. An id may be any unambiguous prefix.
        #[arg(required = true)]
        names: Vec<String>,
    },

    /// Migrate this machine's checkouts from the flat legacy workspace root
    /// into this control plane's per-control-plane slugged root.
    ///
    /// Dry-run by default: prints the plan (old → new for every checkout, its
    /// worktrees included) and changes nothing. `--apply` performs the moves,
    /// tells the control plane to rewrite its path records, and points
    /// `node.toml` at the slugged root. `--apply` refuses while `nook run` is
    /// live — stop the agent first.
    MigrateWorkspaces {
        /// Perform the migration instead of only printing the plan.
        #[arg(long)]
        apply: bool,
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

    // Refuse a misspelled loop kind here rather than reporting a shorter list
    // than the operator configured (MAIN-142 AC-6). Silently dropping it would
    // leave them believing a stage was enabled — the failure would surface as
    // jobs queueing forever with no obvious cause.
    if let Err(e) =
        capabilities::parse_loop_kinds(&std::env::var("NOOK_LOOP_KINDS").unwrap_or_default())
    {
        anyhow::bail!("{e}");
    }

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
        Command::Teach { path, name } => cli::teach(&path, name.as_deref()).await,
        Command::Taught { json } => cli::taught(json).await,
        Command::Unteach { name } => cli::unteach(&name).await,
        Command::Tasks {
            board,
            labels,
            not_labels,
            assignee,
            column_type,
            types,
            parent,
            unblocked,
            this_node,
            workspace,
            all_workspaces,
            backlog,
            json,
        } => {
            cli::tasks(
                board.as_deref(),
                &labels,
                &not_labels,
                assignee.as_deref(),
                column_type.as_deref(),
                &types,
                parent.as_deref(),
                unblocked,
                this_node,
                workspace.as_deref(),
                all_workspaces,
                backlog,
                json,
            )
            .await
        }
        Command::Create(CreateCommand::Task {
            title,
            board,
            description,
            column_type,
            priority,
            labels,
            type_,
            parent,
            workspace,
        }) => {
            cli::create_task(cli::CreateTask {
                title,
                board,
                description,
                column_type,
                priority,
                labels,
                type_,
                parent,
                workspace,
            })
            .await
        }
        Command::Relate {
            blocker,
            kind,
            dependent,
        } => cli::relate(&blocker, &kind, &dependent).await,
        Command::Task { key, json } => cli::task(&key, json).await,
        Command::Comment { key, body } => cli::comment(&key, &body.join(" ")).await,
        Command::SetDescription { key, description } => {
            cli::set_description(&key, &description.join(" ")).await
        }
        Command::Label { key, name, remove } => cli::label(&key, &name, remove).await,
        Command::Workspace(WorkspaceCommand::Current { json }) => {
            cli::workspace_current(json).await
        }
        Command::AgentState { state } => cli::agent_state(&state).await,
        Command::Claim {
            key,
            column_type,
            any_workspace,
        } => cli::claim(&key, column_type.as_deref(), any_workspace).await,
        Command::Context(ContextCommand::List) => contexts::list(),
        Command::Context(ContextCommand::Current) => contexts::current(),
        Command::Context(ContextCommand::Save { name, server }) => contexts::save(&name, server),
        Command::Context(ContextCommand::Use { name }) => contexts::use_context(&name),
        Command::Context(ContextCommand::Remove { name }) => contexts::remove(&name),
        Command::Hooks(HooksCommand::Install { dry_run }) => wizard::hooks::install(dry_run),
        Command::Hooks(HooksCommand::Uninstall) => wizard::hooks::uninstall(),
        Command::Operator(OperatorCommand::Grant { email, role }) => {
            cli::operator_role(&email, &role, false).await
        }
        Command::Operator(OperatorCommand::Revoke { email, role }) => {
            cli::operator_role(&email, &role, true).await
        }
        Command::Operator(OperatorCommand::Loops { state }) => cli::operator_loops(&state).await,
        Command::Operator(OperatorCommand::Reconcile { state }) => {
            cli::operator_reconcile(&state).await
        }
        Command::Operator(OperatorCommand::Who) => cli::operator_who().await,
        Command::Operator(OperatorCommand::Bindings { json }) => cli::operator_bindings(json).await,
        Command::Operator(OperatorCommand::Org(OrgCommand::List { json })) => {
            cli::operator_orgs(json).await
        }
        Command::Operator(OperatorCommand::Org(OrgCommand::Create { name, slug })) => {
            cli::operator_org_create(&name, slug.as_deref()).await
        }
        Command::Operator(OperatorCommand::Org(OrgCommand::Move { tenant, org })) => {
            cli::operator_move_tenant(&tenant, &org).await
        }
        Command::Operator(OperatorCommand::Ca(CaCommand::Stage { tenant })) => {
            cli::operator_ca_stage(&tenant).await
        }
        Command::Operator(OperatorCommand::Ca(CaCommand::Promote { tenant, ca })) => {
            cli::operator_ca_promote(&tenant, &ca).await
        }
        Command::Operator(OperatorCommand::Node { node, remove }) => {
            cli::operator_node(&node, remove).await
        }
        Command::Interactions(InteractionsCommand::Ask {
            prompt,
            choices,
            wait,
            job,
            task,
        }) => cli::interactions_ask(&prompt, &choices, wait, job.as_deref(), task.as_deref()).await,
        Command::Interactions(InteractionsCommand::Answer { id, response }) => {
            cli::interactions_answer(&id, &response).await
        }
        Command::Notify {
            title,
            body,
            level,
            kind,
            link,
            session,
        } => {
            cli::notify_fleet(
                &title.join(" "),
                body.as_deref(),
                &level,
                kind.as_deref(),
                link.as_deref(),
                session.as_deref(),
            )
            .await
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
        Command::Server(ServerCommand::PurgeTestTenants) => server_purge_test_tenants().await,
        Command::K8s(K8sCommand::Init {
            release,
            namespace,
            host,
            public_base_url,
            web_origin,
            ingress_class,
            secret_name,
            agent,
            agent_url,
            agent_tls_secret,
            chart_version,
            advanced,
            app_env,
            log_level,
            oidc,
            oidc_issuer,
            oidc_client_id,
            oidc_client_secret,
            oidc_scopes,
            oidc_device_client_id,
            oidc_device_authorization_endpoint,
            mail_provider,
            mail_from,
            mail_token,
            mail_send_enabled,
            mail_notifications_enabled,
            mail_max_per_month,
            mail_max_per_day,
            mail_smtp_host,
            mail_smtp_port,
            mail_smtp_tls,
            mail_smtp_username,
            mail_postmark_api_url,
            queue_provider,
            redis_url,
            redis_list_name,
            sqs_queue_url,
            sqs_region,
            sqs_credentials_mode,
            aws_access_key_id,
            aws_secret_access_key,
            worker,
            worker_replicas,
            worker_work_types,
            keda,
            keda_min_replicas,
            keda_max_replicas,
        }) => {
            let home = std::env::var("HOME").context("HOME is not set")?;
            // Any OIDC/mail field implies its branch is on, mirroring how an agent
            // address implies --agent: nobody passes --oidc-issuer without meaning it.
            let oidc = oidc
                || oidc_issuer.is_some()
                || oidc_client_id.is_some()
                || oidc_client_secret.is_some()
                || oidc_device_client_id.is_some();
            // A queue-specific flag picks the provider when it was left implicit,
            // the same way an --oidc-* flag turns OIDC on: --redis-url means redis,
            // any SQS field means sqs.
            let queue_provider = queue_provider.or_else(|| {
                if redis_url.is_some() || redis_list_name.is_some() {
                    Some("redis".to_string())
                } else if sqs_queue_url.is_some()
                    || sqs_region.is_some()
                    || sqs_credentials_mode.is_some()
                    || aws_access_key_id.is_some()
                    || aws_secret_access_key.is_some()
                {
                    Some("sqs".to_string())
                } else {
                    None
                }
            });
            wizard::k8s::init(wizard::k8s::InitOptions {
                release,
                namespace,
                host,
                public_base_url,
                web_origin,
                ingress_class,
                secret_name,
                // Supplying an agent address or its TLS Secret is meaningless
                // unless the listener is on, so either implies --agent.
                agent: agent || agent_url.is_some() || agent_tls_secret.is_some(),
                agent_url,
                agent_tls_secret,
                chart_version,
                advanced,
                app_env,
                log_level,
                oidc,
                oidc_issuer,
                oidc_client_id,
                oidc_client_secret,
                oidc_scopes,
                oidc_device_client_id,
                oidc_device_authorization_endpoint,
                mail_provider,
                mail_from,
                mail_token,
                mail_send_enabled,
                mail_notifications_enabled,
                mail_max_per_month,
                mail_max_per_day,
                mail_smtp_host,
                mail_smtp_port,
                mail_smtp_tls,
                mail_smtp_username,
                mail_postmark_api_url,
                queue_provider,
                redis_url,
                redis_list_name,
                sqs_queue_url,
                sqs_region,
                sqs_credentials_mode,
                aws_access_key_id,
                aws_secret_access_key,
                // Supplying a worker/KEDA field is meaningless unless the worker
                // is on, so any of them implies --worker.
                worker: worker
                    || worker_replicas.is_some()
                    || worker_work_types.is_some()
                    || keda
                    || keda_min_replicas.is_some()
                    || keda_max_replicas.is_some(),
                worker_replicas,
                worker_work_types,
                keda,
                keda_min_replicas,
                keda_max_replicas,
                // The chart's version equals the release tag WITHOUT the `v`
                // (the release workflow stamps it that way), so the bare crate
                // version is the right default pin — not the v-prefixed image tag.
                default_chart_version: env!("CARGO_PKG_VERSION").to_string(),
                nook_dir: std::path::PathBuf::from(home).join(".nook"),
            })
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
            // Plumb this node's tmux server socket BEFORE any tmux use, so every
            // call lands on the node's own server (MAIN-108 AC-2). Absent →
            // the default server, unchanged.
            tmux::set_socket(cfg.tmux_socket.clone());
            // Reaches sessions that already exist (mouse/scrollback/clipboard).
            tmux::apply_server_defaults();
            // Mark this agent live so `nook migrate-workspaces --apply` refuses
            // to move checkouts while discovery could report a half-moved tree
            // (MAIN-107 AC-2). Removed on clean exit; stale copies are ignored.
            let _pidfile = config::PidFile::write()?;
            // The loop skills have to BE here before a job types `/nook-spec`
            // at an agent (MAIN-344). Done on every boot rather than once at
            // join: a node joined by an older binary keeps that binary's set,
            // so an image upgrade would otherwise deliver nothing.
            let installed = wizard::skills::install_embedded_quietly();
            tracing::info!(count = installed.len(), "embedded skills installed");
            conn::run(cfg).await
        }
        Command::Status => status().await,
        Command::Get {
            resource,
            name,
            json,
            tenant,
            all_tenants,
            args,
        } => {
            // `nook get workspace git-ssh` is not a listing at all — it is the
            // ssh shim git execs. Routed here so the verb reads the way the
            // rest of the CLI does.
            if resource == "workspace" && name.as_deref() == Some("git-ssh") {
                cli::git_ssh(&args).await
            } else {
                cli::get(
                    &resource,
                    name.as_deref(),
                    json,
                    tenant.as_deref(),
                    all_tenants,
                )
                .await
            }
        }
        Command::Import { path, link } => cli::import(path.as_deref(), link).await,
        Command::Delete { resource, names } => cli::delete(&resource, &names).await,
        Command::Rollout { cmd } => match cmd {
            RolloutCmd::Restart { target, yes } => cli::rollout_restart(&target, yes).await,
        },
        Command::MigrateWorkspaces { apply } => cli::migrate_workspaces(apply).await,
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
enum OperatorCommand {
    /// Grant a deployment-scoped role to a user, by email.
    Grant {
        email: String,
        /// operator | org_admin
        #[arg(long, default_value = "operator")]
        role: String,
    },
    /// Take it away again.
    Revoke {
        email: String,
        #[arg(long, default_value = "operator")]
        role: String,
    },
    /// Turn the loop machinery on or off for this tenant, or ask its state.
    /// Default is OFF: a fresh deployment runs no loops until asked.
    Loops {
        /// `on` | `off` | `status` (omit for status).
        #[arg(default_value = "status")]
        state: String,
    },
    /// Turn session RECONCILING on or off for this tenant, or ask its state.
    /// Default is OFF, like loops — a fresh deployment converges nothing until
    /// asked. Until this verb existed the switch had no CLI and no UI write at
    /// all, so declarative sessions could only be enabled by raw HTTP.
    Reconcile {
        /// `on` | `off` | `status` (omit for status).
        #[arg(default_value = "status")]
        state: String,
    },
    /// What does the CLI's current credential hold?
    Who,
    /// Who holds what, across the deployment.
    Bindings {
        #[arg(long)]
        json: bool,
    },
    /// Orgs: the layer between a deployment and its tenants.
    #[command(subcommand)]
    Org(OrgCommand),
    /// Certificate authorities. Staging and promoting are two acts on purpose.
    #[command(subcommand)]
    Ca(CaCommand),
    /// Stop a machine, or remove it.
    Node {
        /// Node id.
        node: String,
        /// Remove the record entirely rather than revoking its certificate.
        #[arg(long)]
        remove: bool,
    },
}

#[derive(clap::Subcommand)]
enum InteractionsCommand {
    /// Ask a human a question and persist it. Prints the interaction id.
    ///
    /// Auto-scopes to the calling session/job from `NOOK_SESSION_ID` and
    /// `NOOK_JOB_ID` when set, so an in-session executor's ask is anchored to
    /// its own work without passing ids by hand. With `--wait`, blocks until a
    /// human answers (or cancels), then prints the answer to stdout.
    Ask {
        /// The question to put to a human.
        prompt: String,
        /// A structured choice the answer is expected to be one of (repeatable).
        #[arg(long = "choice")]
        choices: Vec<String>,
        /// Block until answered (or canceled), then print the response.
        #[arg(long)]
        wait: bool,
        /// The loop job this pauses on. Defaults to `NOOK_JOB_ID`.
        #[arg(long)]
        job: Option<String>,
        /// The ticket to anchor to when there is no job. Ignored if `job` is set.
        #[arg(long)]
        task: Option<String>,
    },
    /// Answer a pending interaction.
    Answer {
        /// The interaction id.
        id: String,
        /// The response.
        response: String,
    },
}

#[derive(clap::Subcommand)]
enum OrgCommand {
    /// List orgs and how many tenants each holds.
    List {
        #[arg(long)]
        json: bool,
    },
    Create {
        name: String,
        #[arg(long)]
        slug: Option<String>,
    },
    /// Move a tenant into another org.
    Move {
        /// Tenant id.
        tenant: String,
        /// Org id.
        org: String,
    },
}

#[derive(clap::Subcommand)]
enum CaCommand {
    /// Stage a new CA for a tenant. Nodes pick it up on their next renewal.
    ///
    /// Deliberately does NOT promote: switching signer before machines have
    /// renewed strands every one that has not.
    Stage {
        /// Tenant id.
        tenant: String,
    },
    /// Make a staged CA the signer.
    Promote {
        tenant: String,
        /// CA id from `stage`.
        ca: String,
    },
}

#[derive(clap::Subcommand)]
enum ContextCommand {
    /// Every saved control plane, with the current one marked.
    #[command(alias = "ls")]
    List,
    /// Just the current one, tab-separated, for prompts and scripts.
    Current,
    /// Save the login you are using now under a name.
    Save {
        name: String,
        /// Override the URL recorded, when the login did not carry one.
        #[arg(long)]
        server: Option<String>,
    },
    /// Point this machine at a saved control plane.
    Use { name: String },
    /// Forget a saved control plane. Does not log you out of it.
    #[command(alias = "rm")]
    Remove { name: String },
}

#[derive(Subcommand)]
enum RolloutCmd {
    /// Kill a workspace's sessions so the reconciler starts fresh ones.
    ///
    /// `nook rollout restart workspace/nook-os` — the `kubectl` spelling, and
    /// `nook rollout restart nook-os` works too. The reconciler is what brings
    /// them back, so this only does anything for a workspace it manages.
    Restart {
        /// `workspace/<slug-or-name>`, or just the slug or name.
        target: String,
        /// Skip the confirmation. Every session in the workspace is killed.
        #[arg(short = 'y', long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum WorkspaceCommand {
    /// Print the workspace of the session this command runs in (name + id).
    /// Empty when not in a workspace session. `--json` for `{id, name}` or null.
    Current {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum CreateCommand {
    /// File a new task on the board. Prints the created key and url; exits
    /// non-zero with the server's message on a rejected value.
    Task {
        /// The title (required).
        #[arg(long)]
        title: String,
        /// Board key or uuid. Defaults to the first board.
        #[arg(long)]
        board: Option<String>,
        /// Markdown body. `-` reads stdin, for multi-line bodies.
        #[arg(long)]
        description: Option<String>,
        /// backlog | unstarted | started | completed | canceled. Default: backlog.
        #[arg(long = "column-type")]
        column_type: Option<String>,
        /// 0 none, 1 urgent, 2 high, 3 medium, 4 low.
        #[arg(long)]
        priority: Option<i32>,
        /// Attach a label by name (repeatable), created for the tenant if new.
        #[arg(long = "label")]
        labels: Vec<String>,
        /// task | bug | epic | story | chore. Default: task.
        #[arg(long = "type")]
        type_: Option<String>,
        /// File under an epic (key or uuid) on the same board.
        #[arg(long)]
        parent: Option<String>,
        /// Workspace (uuid or name). Defaults to the session's workspace.
        #[arg(long)]
        workspace: Option<String>,
    },
}

#[derive(Subcommand)]
enum HooksCommand {
    /// Add a Stop hook to Claude Code so finishing a turn notifies the fleet.
    Install {
        /// Print what would change instead of writing it.
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove it again.
    Uninstall,
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
    /// Delete the legacy `test-<uuid>` tenants left behind by the old shared-DB
    /// test path (MAIN-221). Dev-only: the control plane refuses it in
    /// production or when dev mode is off. Cascades, and is idempotent — a
    /// second run deletes nothing.
    PurgeTestTenants,
}

#[derive(clap::Subcommand)]
enum K8sCommand {
    /// Write ~/.nook/k8s/<release>/values.yaml for a NookOS control plane and
    /// print the exact `kubectl create secret` + `helm install` commands.
    ///
    /// A hand-off, not an install: it never runs helm or kubectl and writes no
    /// secret material. With a terminal it prompts (flags pre-seed the answers);
    /// with none it runs from the flags and requires --host.
    Init {
        /// Helm release name; also the per-release values folder. Default: nook.
        #[arg(long)]
        release: Option<String>,
        /// Kubernetes namespace. Default: nook.
        #[arg(long)]
        namespace: Option<String>,
        /// The external host that routes to NookOS. Required without a terminal.
        #[arg(long)]
        host: Option<String>,
        /// PUBLIC_BASE_URL. Default: https://<host>.
        #[arg(long)]
        public_base_url: Option<String>,
        /// WEB_ORIGIN. Default: the public base URL.
        #[arg(long)]
        web_origin: Option<String>,
        /// ingressClassName. Empty leaves it unset (cluster default).
        #[arg(long)]
        ingress_class: Option<String>,
        /// Name of the Kubernetes Secret the chart reads. Default: nook-control-secrets.
        #[arg(long)]
        secret_name: Option<String>,
        /// Enable the agent mTLS listener so external nodes can join.
        #[arg(long)]
        agent: bool,
        /// Agent listener public address (host:8081). Implies --agent.
        #[arg(long)]
        agent_url: Option<String>,
        /// Name of the TLS Secret holding the agent cert. Implies --agent.
        #[arg(long)]
        agent_tls_secret: Option<String>,
        /// Chart version to pin (helm --version). Empty pulls the latest chart.
        /// Defaults to this binary's build version.
        #[arg(long)]
        chart_version: Option<String>,
        /// Take the Advanced path on every branch (OIDC, mail, logging): prompt
        /// for every field. Without a terminal, this is what reproduces the
        /// full interactive result from flags alone.
        #[arg(long)]
        advanced: bool,
        /// APP_ENV: "dev" (default, dev-login hatch ON) or "production" (OFF).
        #[arg(long)]
        app_env: Option<String>,
        /// RUST_LOG: a bare level (error/warn/info/debug/trace) or a full
        /// EnvFilter directive. Default: info.
        #[arg(long)]
        log_level: Option<String>,
        /// Configure OIDC single sign-on. Implied by any --oidc-* flag below.
        #[arg(long)]
        oidc: bool,
        /// OIDC_ISSUER_URL. Its /.well-known/openid-configuration is probed.
        #[arg(long)]
        oidc_issuer: Option<String>,
        /// OIDC_CLIENT_ID (the confidential client).
        #[arg(long)]
        oidc_client_id: Option<String>,
        /// OIDC client secret → the printed secret command only, never written.
        #[arg(long)]
        oidc_client_secret: Option<String>,
        /// OIDC_SCOPES. Default: "openid profile email".
        #[arg(long)]
        oidc_scopes: Option<String>,
        /// OIDC_DEVICE_CLIENT_ID (the public client for device sign-in).
        #[arg(long)]
        oidc_device_client_id: Option<String>,
        /// OIDC_DEVICE_AUTHORIZATION_ENDPOINT — only needed when discovery does
        /// not advertise one. Emitted via controlPlane.extraEnv.
        #[arg(long)]
        oidc_device_authorization_endpoint: Option<String>,
        /// MAIL_PROVIDER: "capture" (default, delivers nothing), "smtp", or
        /// "postmark". smtp/postmark take the Advanced mail path.
        #[arg(long)]
        mail_provider: Option<String>,
        /// MAIL_FROM address for outbound mail.
        #[arg(long)]
        mail_from: Option<String>,
        /// SMTP password or Postmark token → the printed secret command only.
        #[arg(long)]
        mail_token: Option<String>,
        /// MAIL_SEND_ENABLED: the master send switch.
        #[arg(long)]
        mail_send_enabled: bool,
        /// MAIL_NOTIFICATIONS_ENABLED: also send notification emails.
        #[arg(long)]
        mail_notifications_enabled: bool,
        /// MAIL_MAX_PER_MONTH cap on real sends (blank = uncapped).
        #[arg(long)]
        mail_max_per_month: Option<String>,
        /// MAIL_MAX_PER_DAY cap on real sends (blank = uncapped).
        #[arg(long)]
        mail_max_per_day: Option<String>,
        /// SMTP_HOST (provider=smtp).
        #[arg(long)]
        mail_smtp_host: Option<String>,
        /// SMTP_PORT (provider=smtp).
        #[arg(long)]
        mail_smtp_port: Option<String>,
        /// SMTP_TLS: none/starttls/implicit (provider=smtp).
        #[arg(long)]
        mail_smtp_tls: Option<String>,
        /// SMTP_USERNAME (provider=smtp).
        #[arg(long)]
        mail_smtp_username: Option<String>,
        /// POSTMARK_API_URL override (provider=postmark).
        #[arg(long)]
        mail_postmark_api_url: Option<String>,
        /// NOOK_QUEUE_PROVIDER: "database" (default) | "redis" | "sqs". Implied by
        /// any queue flag below.
        #[arg(long)]
        queue_provider: Option<String>,
        /// Redis URL (provider=redis) → the printed secret command only. Implies
        /// --queue-provider redis.
        #[arg(long)]
        redis_url: Option<String>,
        /// Redis list KEDA watches (provider=redis). Blank uses the chart default.
        #[arg(long)]
        redis_list_name: Option<String>,
        /// NOOK_SQS_QUEUE_URL (provider=sqs). Implies --queue-provider sqs.
        #[arg(long)]
        sqs_queue_url: Option<String>,
        /// NOOK_SQS_REGION (provider=sqs). Implies --queue-provider sqs.
        #[arg(long)]
        sqs_region: Option<String>,
        /// SQS credentials: "irsa" (default, pod IAM role) | "secret" (AWS keys).
        #[arg(long)]
        sqs_credentials_mode: Option<String>,
        /// AWS access key id (sqs + secret mode) → the printed secret command only.
        #[arg(long)]
        aws_access_key_id: Option<String>,
        /// AWS secret access key (sqs + secret mode) → the printed secret command only.
        #[arg(long)]
        aws_secret_access_key: Option<String>,
        /// Deploy the queue worker. Implied by any --worker-* or --keda* flag.
        #[arg(long)]
        worker: bool,
        /// Worker replica count (worker.replicas). Default: 1.
        #[arg(long)]
        worker_replicas: Option<String>,
        /// NOOK_WORK_TYPES allow-list (comma-separated). Empty = every type.
        #[arg(long)]
        worker_work_types: Option<String>,
        /// Autoscale the worker with KEDA (must be installed in-cluster). Implies
        /// --worker.
        #[arg(long)]
        keda: bool,
        /// KEDA minimum replicas. Default: 1.
        #[arg(long)]
        keda_min_replicas: Option<String>,
        /// KEDA maximum replicas. Default: 10.
        #[arg(long)]
        keda_max_replicas: Option<String>,
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
/// `nook server purge-test-tenants` (MAIN-221 AC-3): ask the control plane to
/// drop the legacy `test-%` tenants. The endpoint is dev-gated server-side, so
/// this carries no auth and simply surfaces the count — or the server's refusal
/// message verbatim when it declines (production / dev mode off).
async fn server_purge_test_tenants() -> Result<()> {
    let cfg = NodeConfig::load()?;
    let server = cfg.server.trim_end_matches('/');
    let resp = reqwest::Client::new()
        .post(format!("{server}/api/v1/auth/purge-test-tenants"))
        .send()
        .await
        .context("reaching the control plane")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        // The server hands back {"error": "<why>"}; show the reason, not a bare code.
        let reason = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
            .unwrap_or_else(|| body.trim().to_string());
        anyhow::bail!("purge refused ({status}): {reason}");
    }
    let out: serde_json::Value = resp.json().await.context("decoding the response")?;
    let deleted = out.get("deleted").and_then(|d| d.as_i64()).unwrap_or(0);
    println!("Deleted {deleted} test tenant(s).");
    Ok(())
}

pub(crate) async fn update_binary() -> Result<()> {
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

    // Empty only from a control plane predating MAIN-347; normalize to `None` so
    // the default root falls back to the host slug rather than scoping under "".
    let tenant_slug = Some(joined.tenant_slug.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    // An explicit root (--workspace-root, a config file's workspace_roots, or the
    // docker entrypoint's NOOK_WORKSPACE_ROOT → --workspace-root) always wins;
    // only the DEFAULT becomes tenant-scoped (MAIN-347).
    let workspace_roots = if !spec.workspace_roots.is_empty() {
        // An explicit root (flag / config file / NOOK_WORKSPACE_ROOT) always wins.
        spec.workspace_roots
    } else if let Some(existing) = NodeConfig::load()
        .ok()
        .filter(|c| !c.workspace_roots.is_empty())
    {
        // Re-joining an already-configured node with no explicit root: carry the
        // roots the previous config established. A bare `nook join` must NOT
        // rebuild node.toml onto the default and silently relocate the root out
        // from under every existing checkout (data-orphaning bug).
        existing.workspace_roots
    } else {
        // A genuine first join with no explicit root: the tenant-scoped default
        // (MAIN-347), falling back to the host slug when the CP sent no tenant.
        vec![crate::config::default_workspace_root(
            tenant_slug.as_deref(),
            &server,
        )]
    };
    // Forward-only, like workspace_roots (MAIN-108 AC-1): a re-join KEEPS the
    // node's existing socket — a pre-108 node's `None` stays `None` (byte-
    // identical), and a private socket is never relocated out from under live
    // sessions (AC-6). Only a genuine first join (no prior config) derives the
    // private server.
    let tmux_socket = match NodeConfig::load() {
        Ok(existing) => existing.tmux_socket,
        Err(_) => Some(crate::config::derived_tmux_socket(&server)),
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
        service: NodeConfig::load().ok().and_then(|c| c.service),
        tmux_socket,
        // The tenant this node joined, scoping the root above. Keep a
        // previously-recorded slug if this CP sent none (forward-only).
        tenant_slug: tenant_slug
            .clone()
            .or_else(|| NodeConfig::load().ok().and_then(|c| c.tenant_slug)),
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
