mod attachments;
mod capabilities;
mod certs;
mod chat;
mod cli;
mod compose;
mod config;
mod conn;
mod contexts;
mod cordon;
mod device_login;
mod discovery;
mod enroll;
mod gitops;
mod job_adapter;
mod loop_job;
mod notebook;
mod pinning;
mod ports;
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
        /// Include finished cards (Done, Canceled). Excluded by default, so a
        /// card left labelled `agent-ready` after it merged is not offered as
        /// work — also enforced server-side.
        #[arg(long)]
        done: bool,
        #[arg(long)]
        json: bool,
    },
    /// Read one whole issue: body, labels, comments, blockers.
    Task {
        /// Human key (NOOK-42) or id.
        key: String,
        #[arg(long)]
        json: bool,
        /// List the description bodies past replaces overwrote (newest first)
        /// — the undo for a clobbered description.
        #[arg(long)]
        revisions: bool,
    },
    /// Comment on a task.
    Comment {
        key: String,
        /// Also RESTART the card: clear every escalation label (`blocked`,
        /// `spec-blocked`, `needs-human-review`), put `agent-ready` back on,
        /// and let the build loop pick it up again. The body is the ruling
        /// that released it, and is required.
        #[arg(long)]
        unblock: bool,
        /// Also rule CHANGES REQUESTED on this card's pull request: post the
        /// body there as well, replace its verdict label with
        /// `loop-changes-requested`, hold the review loop off that head, and
        /// send the builder back to repair it. The body is the ruling, and is
        /// required; a lone `-` reads it from stdin. Needs an open PR.
        #[arg(long)]
        request_changes: bool,
        /// The comment body (markdown).
        body: Vec<String>,
    },
    /// Safely replace a task's description. Reads the current version and writes
    /// with an optimistic-concurrency guard, retrying on a concurrent edit — so
    /// it never silently loses your change. Read the body first with `nook task`.
    SetDescription {
        key: String,
        /// The new description (markdown). A lone `-` reads it from stdin,
        /// for multi-line bodies.
        description: Vec<String>,
        /// Replace a long description with a tiny body anyway — without this,
        /// shrinking a >200-char description below 20 chars is refused as
        /// probable payload loss.
        #[arg(long)]
        force: bool,
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
    /// Deprecated: these verbs are `nook issues attach|attachments|download|
    /// detach` now (MAIN-610). Kept for ONE release and hidden from `--help`,
    /// so the new spelling is the only one anybody discovers.
    ///
    /// Not politeness: loop agents are running right now with the old spelling
    /// baked into skill text a fleet has not been re-taught, and a hard removal
    /// breaks a build mid-run. Every invocation prints the replacement.
    #[command(subcommand, hide = true)]
    Attachments(AttachmentsCommand),
    /// Your personal notebook (MAIN-66) at a terminal.
    ///
    ///     nook notebook folders                     the tree
    ///     nook notebook create --folder "A/B" …     writing it makes A and B
    ///     nook notebook append "A/B/note" …         adds a block to the end
    ///
    /// Person-owned and private: this is YOUR notebook in every org you belong
    /// to, and a machine credential is refused rather than answered as the
    /// tenant owner. Notes and folders are addressed by slash-delimited path
    /// or by id. A new noun group, per docs/cli-style.md — the top level stays
    /// frozen.
    #[command(subcommand)]
    Notebook(NotebookCommand),
    /// Loop reviews of a workspace (MAIN-408).
    ///
    /// A review job is raised automatically by the board-signal sweep when a
    /// card lands in a review column; these verbs are the manual half and the
    /// sweep's own switch. A new noun group, per docs/cli-style.md — the top
    /// level stays frozen.
    #[command(subcommand)]
    Reviews(ReviewsCommand),
    /// Build runs: the per-repo ceiling (MAIN-461) and the board's
    /// convergence on demand for one card (MAIN-458).
    /// A new noun group, per docs/cli-style.md — the top level stays frozen.
    #[command(subcommand)]
    Builds(BuildsCommand),
    /// The listeners this workspace declared, and the numbers this process
    /// holds for them (MAIN-597).
    ///
    /// The declaration is the workspace's and the leases are in the
    /// environment; only here are the two joined into something openable. A
    /// new noun group, per docs/cli-style.md — the top level stays frozen.
    #[command(subcommand)]
    Ports(PortsCommand),
    /// Board cards, by key — the verbs a skill used to reach for `curl` to
    /// perform (MAIN-138), and the files hung on them (MAIN-610).
    ///
    ///     nook issues attach MAIN-42 shot.png   put a file on the card
    ///     nook issues attachments MAIN-42       what the card carries
    ///     nook issues download MAIN-42/shot.png pull the one you want
    ///     nook issues detach MAIN-42/shot.png   take it off again
    ///
    /// The CLI is the surface skills are meant to drive the board through: one
    /// tested client, fewer tokens, and no hand-built request body to get
    /// wrong. A new noun group, per docs/cli-style.md — the top level stays
    /// frozen and none of the flat task verbs move.
    #[command(subcommand)]
    Issues(IssuesCommand),
    /// Epic-runner passes (MAIN-144): the loop's merge authority, one
    /// deliberate enqueue per pass.
    #[command(subcommand)]
    Epics(EpicsCommand),
    /// Reach a port on this machine from anywhere in the tenant (MAIN-9).
    ///
    ///     nook tunnel 3000        open one, and print its URL
    ///     nook tunnel list        what is open
    ///     nook tunnel stop        close this session's tunnels
    ///
    /// `tunnel` is the singular this reads as in a sentence; `tunnels` is the
    /// group's own plural name, per docs/cli-style.md. Both work.
    #[command(visible_alias = "tunnel")]
    Tunnels(TunnelsArgs),
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
        /// Act in one of your other tenants. Slug or id. Overrides
        /// NOOK_TENANT_ID; without either you get your home tenant.
        #[arg(short = 'T', long)]
        tenant: Option<String>,
    },

    /// Set a mutable property of a fleet object. Currently: a node's port range.
    Set {
        #[command(subcommand)]
        cmd: SetCmd,
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
    Whoami {
        /// Act in one of your other tenants. Slug or id. Overrides
        /// NOOK_TENANT_ID; without either you get your home tenant.
        #[arg(short = 'T', long)]
        tenant: Option<String>,
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

    // A desktop install's shell asks for this (MAIN-400 AC-1); nothing else
    // sets the variable, so a node under systemd or compose is untouched. It is
    // what stops a force-quit of the app leaving this node running against a
    // control plane that is gone.
    nook_desktop_env::exit_when_orphaned();

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
            done,
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
                done,
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
        Command::Task {
            key,
            json,
            revisions,
        } => cli::task(&key, json, revisions).await,
        Command::Comment {
            key,
            unblock,
            request_changes,
            body,
        } => cli::comment(&key, &body, unblock, request_changes).await,
        Command::SetDescription {
            key,
            description,
            force,
        } => cli::set_description(&key, &description, force).await,
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
        Command::Reviews(ReviewsCommand::Enqueue {
            workspace,
            seed,
            pr,
            force,
        }) => cli::reviews_enqueue(&workspace, seed.as_deref(), pr, force).await,
        Command::Epics(EpicsCommand::Run { epic, seed }) => {
            cli::epics_run(&epic, seed.as_deref()).await
        }
        Command::Issues(IssuesCommand::Move { key, state, column }) => {
            cli::issues_move(&key, state.as_deref(), column.as_deref()).await
        }
        Command::Issues(IssuesCommand::Release { key }) => cli::issues_release(&key).await,
        Command::Issues(IssuesCommand::PruneWorktree { key }) => {
            cli::issues_prune_worktree(&key).await
        }
        Command::Issues(IssuesCommand::SetParent { key, parent }) => {
            cli::issues_set_parent(&key, &parent).await
        }
        Command::Issues(IssuesCommand::Attach {
            key,
            file,
            replace,
            json,
        }) => attachments::add(&key, &file, replace, json).await,
        Command::Issues(IssuesCommand::Attachments { key, json }) => {
            attachments::list(&key, json).await
        }
        Command::Issues(IssuesCommand::Download { addr, out, force }) => {
            attachments::get(&addr, out.as_deref(), force).await
        }
        Command::Issues(IssuesCommand::Detach { addr, json }) => attachments::rm(&addr, json).await,
        Command::Tunnels(TunnelsArgs {
            command: Some(TunnelsCommand::List { json }),
            ..
        }) => cli::tunnels_list(json).await,
        Command::Tunnels(TunnelsArgs {
            command: Some(TunnelsCommand::Stop { label }),
            ..
        }) => cli::tunnels_stop(label.as_deref()).await,
        Command::Tunnels(TunnelsArgs { port, json, .. }) => cli::tunnels_open(port, json).await,
        Command::Reviews(ReviewsCommand::Verdict { verdict, body }) => {
            cli::reviews_verdict(&verdict, body.as_deref()).await
        }
        Command::Reviews(ReviewsCommand::Scale { workspace, count }) => {
            cli::reviews_scale(&workspace, count.as_deref()).await
        }
        Command::Builds(BuildsCommand::Scale { workspace, count }) => {
            cli::builds_scale(&workspace, count.as_deref()).await
        }
        Command::Builds(BuildsCommand::Loop {
            workspace,
            state,
            node,
            concurrency,
        }) => {
            cli::builds_loop(
                &workspace,
                state.as_deref(),
                node.as_deref(),
                concurrency.as_deref(),
            )
            .await
        }
        Command::Builds(BuildsCommand::Enqueue { task }) => cli::builds_enqueue(&task).await,
        Command::Builds(BuildsCommand::Outcome {
            conclusion,
            url,
            question,
        }) => cli::builds_outcome(&conclusion, url.as_deref(), question.as_deref()).await,
        Command::Ports(PortsCommand::List {
            workspace,
            browsable,
            json,
        }) => ports::list(workspace.as_deref(), browsable, json).await,
        Command::Attachments(AttachmentsCommand::List { task, json }) => {
            attachments::deprecated("list");
            attachments::list(&task, json).await
        }
        Command::Attachments(AttachmentsCommand::Get { id, out, force }) => {
            attachments::deprecated("get");
            attachments::get(&id, out.as_deref(), force).await
        }
        Command::Attachments(AttachmentsCommand::Add {
            task,
            file,
            replace,
            json,
        }) => {
            attachments::deprecated("add");
            attachments::add(&task, &file, replace, json).await
        }
        Command::Attachments(AttachmentsCommand::Rm { id, json }) => {
            attachments::deprecated("rm");
            attachments::rm(&id, json).await
        }
        Command::Notebook(NotebookCommand::List { folder, json }) => {
            notebook::list(folder.as_deref(), json).await
        }
        Command::Notebook(NotebookCommand::Read { note, json }) => {
            notebook::read(&note, json).await
        }
        Command::Notebook(NotebookCommand::Create {
            title,
            folder,
            content,
            json,
        }) => notebook::create(&title, folder.as_deref(), content.as_deref(), json).await,
        Command::Notebook(NotebookCommand::Append {
            note,
            content,
            json,
        }) => notebook::append(&note, &content, json).await,
        Command::Notebook(NotebookCommand::Delete { note, json }) => {
            notebook::delete(&note, json).await
        }
        Command::Notebook(NotebookCommand::Folders { json }) => notebook::folders(json).await,
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
            giphy,
            giphy_key,
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
                giphy,
                giphy_key,
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
            // Mark this agent live (MAIN-107 AC-2 established this for the
            // since-retired workspace migration; the pidfile remains useful as
            // the general "is an agent already running here" signal). Removed
            // on clean exit; stale copies are ignored.
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
        Command::Delete {
            resource,
            names,
            tenant,
        } => cli::delete(&resource, &names, tenant.as_deref()).await,
        Command::Set { cmd } => match cmd {
            SetCmd::Ports {
                target,
                range,
                clear,
                exclude,
                exclude_clear,
                tenant,
            } => {
                cli::set_ports(
                    &target,
                    range.as_deref(),
                    clear,
                    exclude.as_deref(),
                    exclude_clear,
                    tenant.as_deref(),
                )
                .await
            }
            SetCmd::Capacity {
                target,
                jobs,
                clear,
                tenant,
            } => cli::set_capacity(&target, jobs, clear, tenant.as_deref()).await,
        },
        Command::Rollout { cmd } => match cmd {
            RolloutCmd::Restart {
                target,
                yes,
                tenant,
            } => cli::rollout_restart(&target, yes, tenant.as_deref()).await,
        },
        Command::Login { token, server } => match token {
            Some(t) => cli::login(&t, server.as_deref()).await,
            None => cli::login_with_provider(server.as_deref()).await,
        },
        Command::Whoami { tenant } => cli::whoami(tenant.as_deref()).await,
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
enum EpicsCommand {
    /// Run one epic-runner pass over a named epic, on the fleet. Manual only —
    /// invocation is authorization; there is no schedule and no auto-feed.
    Run {
        /// The epic, by board key (e.g. MAIN-35).
        epic: String,
        /// An opening brief for the pass.
        #[arg(long)]
        seed: Option<String>,
    },
}

/// `nook issues …` — the board verbs a skill drives a card with (MAIN-138).
///
/// Every one takes the card by KEY (`MAIN-42`) as well as by uuid, because a
/// key is what an agent is handed.
#[derive(clap::Subcommand)]
enum IssuesCommand {
    /// Move a card to the column that means <state> on its own board.
    ///
    /// The type form — `nook issues move MAIN-42 started` — is the one to
    /// write: it survives a board that renamed its columns, which an exact
    /// `--column "In Review"` does not.
    Move {
        /// The card, by key or uuid.
        key: String,
        /// backlog | unstarted | started | review | completed | canceled.
        /// Omit only when giving --column.
        state: Option<String>,
        /// Target an EXACT column name instead, for a board whose columns do
        /// not map onto the six types.
        #[arg(long)]
        column: Option<String>,
    },
    /// Give a claimed card back: clears the assignee, so it is pickable again.
    Release {
        /// The card, by key or uuid.
        key: String,
    },
    /// Remove the worktree a card recorded, once its PR has landed.
    PruneWorktree {
        /// The card, by key or uuid.
        key: String,
    },
    /// Re-file a card under an epic, or detach it.
    SetParent {
        /// The card, by key or uuid.
        key: String,
        /// The epic, by key or uuid — or `none` to detach it entirely.
        parent: String,
    },
    /// Put a local file on a card: upload it and attach it, in one command.
    ///
    /// The content type is taken from the extension, so a `.webm` is stored as
    /// a video rather than as bytes nothing will play.
    Attach {
        /// The card, by key (MAIN-42) or id.
        key: String,
        /// The file to put on it.
        file: String,
        /// Take off anything on the card already carrying this filename first —
        /// "one of these per card", rather than a pile of versions.
        #[arg(long)]
        replace: bool,
        #[arg(long)]
        json: bool,
    },
    /// Every file on a card and on its comments: address, type, size and the
    /// id to fetch it with.
    Attachments {
        /// The card, by key (MAIN-42) or id.
        key: String,
        #[arg(long)]
        json: bool,
    },
    /// Download one attachment, preserving its original filename.
    ///
    /// Refuses rather than overwriting a file that is already there — a
    /// download that silently replaced a file in a worktree would be the one
    /// mistake nothing recovers from.
    Download {
        /// The attachment: `MAIN-42/shot.png` as `attachments` prints it, or
        /// its uuid.
        addr: String,
        /// Where to put it: a path, or a directory to keep the name inside.
        #[arg(long)]
        out: Option<String>,
        /// Overwrite what is already there.
        #[arg(long)]
        force: bool,
    },
    /// Take one attachment off a card. Removes the stored file with it.
    Detach {
        /// The attachment: `MAIN-42/shot.png` as `attachments` prints it, or
        /// its uuid.
        addr: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(clap::Subcommand)]
enum ReviewsCommand {
    /// Review this workspace now: the reconciler's own convergence on demand.
    /// One directed run per pull request owed one — same rule, same dedupe,
    /// same ceiling — reporting what was raised and why anything was not.
    Enqueue {
        /// The workspace, by id, slug or name.
        workspace: String,
        /// The opening brief for the run.
        #[arg(long)]
        seed: Option<String>,
        /// Converge only this pull request. Required for --force.
        #[arg(long)]
        pr: Option<i64>,
        /// Re-review even when the head equals the last verdicted head
        /// (MAIN-473) — the lever for evidence that changed under a verdict,
        /// like a CI rerun turning green. A live run still refuses.
        #[arg(long)]
        force: bool,
    },
    /// The ceiling on a workspace's review loops, or read the current value.
    ///
    /// A ceiling, not a count: it caps how many review runs are in flight for
    /// the repo at once, and the forge decides how many are wanted (MAIN-448).
    ///
    /// `0` turns reviewing off for that repo; `unset` returns it to the build's
    /// default of one. A repo with more open PRs than the ceiling reviews them
    /// as runs finish, rather than all at once.
    Scale {
        /// The workspace, by id, slug or name.
        workspace: String,
        /// The ceiling, `0` to turn it off, or `unset`. Omit to read.
        count: Option<String>,
    },
    /// Report this review run's conclusion (runs inside a review job; the
    /// control plane posts the comment and labels).
    Verdict {
        /// approved | changes_requested | needs_human | skipped
        verdict: String,
        /// The verdict body; `-` reads stdin. Omit only for `skipped`.
        #[arg(long)]
        body: Option<String>,
    },
}

#[derive(clap::Subcommand)]
enum BuildsCommand {
    /// The ceiling on a workspace's build runs, or read the current value.
    ///
    /// `0` turns builds off for that repo — the workspace-level kill-switch —
    /// and `unset` returns it to the default of one. A ceiling, not a count:
    /// it caps how many build runs are in flight at once.
    Scale {
        /// The workspace, by id, slug or name.
        workspace: String,
        /// The ceiling, `0` to turn builds off, or `unset`. Omit to read.
        count: Option<String>,
    },
    /// The per-workspace build loop: does the control plane fire build runs
    /// for this repo by itself (MAIN-385)?
    ///
    /// Off for every workspace until somebody turns it on, and the person who
    /// does is who the auto-fired runs are requested by — so they are placed on
    /// THEIR nodes. Omit the state to read the current settings.
    Loop {
        /// The workspace, by id, slug or name.
        workspace: String,
        /// `on` or `off`. Omit to read.
        state: Option<String>,
        /// Pin auto-fired runs to this node (by name or id), or `none` to
        /// unpin. A pin never fails over: a run waits queued while its node is
        /// dark rather than starting somewhere else.
        #[arg(long)]
        node: Option<String>,
        /// How many of this repo's cards may build at once — the same ceiling
        /// `nook builds scale` sets. `unset` returns it to the default of one.
        #[arg(long)]
        concurrency: Option<String>,
    },
    /// Build this card now: the reconciler's own convergence on demand,
    /// filtered to one card — same claim, same dedupe, same ceiling.
    Enqueue {
        /// The card, by key (MAIN-42) or id.
        task: String,
    },
    /// A build run reports how it ended — its LAST act (MAIN-459). Job-scoped:
    /// reads `NOOK_JOB_ID`, so an agent cannot conclude a job it is not. The
    /// control plane records the outcome, mirrors it to the board, and
    /// validates the PR's `Closes` join.
    Outcome {
        /// `pr` (opened one) | `blocked` (a human must answer) | `nothing`
        /// (nothing to do at this content).
        conclusion: String,
        /// The opened PR's URL. Required for `pr`.
        #[arg(long)]
        url: Option<String>,
        /// The specific question a human can answer asynchronously. Required
        /// for `blocked`; `-` reads stdin.
        #[arg(long)]
        question: Option<String>,
    },
}

#[derive(Subcommand)]
enum NotebookCommand {
    /// Every note, or just the ones in one folder.
    List {
        /// Narrow to this folder, by path ("Nook/Ideas") or id. Resolved, not
        /// created — a listing never writes.
        #[arg(long)]
        folder: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Print a note's body.
    Read {
        /// The note, by path ("Nook/Ideas/2026-08-13") or id.
        note: String,
        #[arg(long)]
        json: bool,
    },
    /// Write a new note. Missing `--folder` levels are created, `mkdir -p`
    /// style, and repeating the identical command succeeds rather than
    /// conflicting.
    Create {
        /// The note's title — the last segment of the path it will answer to.
        #[arg(long)]
        title: String,
        /// Where it goes, by path or id. Omit for the notebook root.
        #[arg(long)]
        folder: Option<String>,
        /// The body. `-` reads stdin. Omit for an empty note.
        #[arg(long)]
        content: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Add a block to the end of a note, a blank line after what is there.
    ///
    /// Read-modify-write, and BEST-EFFORT by design. The note is re-read
    /// immediately before the write and the append aborts, having written
    /// nothing, if the body moved underneath — it never merges over somebody
    /// else's edit. It cannot close the window entirely: an edit landing in
    /// the last moment before the write is still overwritten, because the
    /// notebook API has no compare-and-set for a client to use. MAIN-590 adds
    /// one. Until then this is not a lock — for a note two people edit at
    /// once, the web UI is the safer surface.
    Append {
        /// The note, by path or id.
        note: String,
        /// The block to add. `-` reads stdin.
        #[arg(long)]
        content: String,
        #[arg(long)]
        json: bool,
    },
    /// Delete a note. Notes only — folders stay where they are.
    Delete {
        /// The note, by path or id.
        note: String,
        #[arg(long)]
        json: bool,
    },
    /// The folder tree, indented.
    Folders {
        #[arg(long)]
        json: bool,
    },
}

/// The retired `nook attachments …` spelling, kept for one release (MAIN-610
/// AC-4). Every variant delegates to the `issues` verb that replaced it, so
/// there is one implementation and the alias cannot drift from it.
#[derive(Subcommand)]
enum AttachmentsCommand {
    /// Every file on a ticket and on its comments: filename, type, size and
    /// the id to fetch it with.
    List {
        /// The card, by key (MAIN-42) or id.
        task: String,
        #[arg(long)]
        json: bool,
    },
    /// Download one attachment, preserving its original filename.
    ///
    /// Refuses rather than overwriting a file that is already there — a
    /// download that silently replaced a file in a worktree would be the one
    /// mistake nothing recovers from.
    Get {
        /// The attachment, by id or `MAIN-42/shot.png` address.
        id: String,
        /// Where to put it: a path, or a directory to keep the name inside.
        #[arg(long)]
        out: Option<String>,
        /// Overwrite what is already there.
        #[arg(long)]
        force: bool,
    },
    /// Put a local file on a card: upload it and attach it, in one command.
    ///
    /// The content type is taken from the extension, so a `.webm` is stored as
    /// a video rather than as bytes nothing will play.
    Add {
        /// The card, by key (MAIN-42) or id.
        task: String,
        /// The file to put on it.
        file: String,
        /// Take off anything on the card already carrying this filename first —
        /// "one of these per card", rather than a pile of versions.
        #[arg(long)]
        replace: bool,
        #[arg(long)]
        json: bool,
    },
    /// Take one attachment off a card. Removes the stored file with it.
    Rm {
        /// The attachment, by id or `MAIN-42/shot.png` address.
        id: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum PortsCommand {
    /// What this workspace declared, with the port each variable holds here.
    ///
    /// `--browsable` narrows it to the listeners that serve a UI — MAIN-596's
    /// resolver answers which, so nothing re-derives the rule locally — and is
    /// how a recorder finds the one URL it should open.
    List {
        /// The workspace, by name or id. Defaults to the one this session or
        /// loop job is already in (`NOOK_WORKSPACE_ID`).
        #[arg(long)]
        workspace: Option<String>,
        /// Only the listeners something can be opened at.
        #[arg(long)]
        browsable: bool,
        #[arg(long)]
        json: bool,
    },
}

/// `nook tunnels <port>` with `list` and `stop` beside it.
///
/// The bare port is a positional on the GROUP rather than an `open` verb,
/// because opening one is what people do ninety-nine times out of a hundred and
/// `nook tunnel 3000` is the sentence they already say. `args_conflicts_with_
/// subcommands` is what keeps clap from reading `list` as a port.
#[derive(clap::Args)]
#[command(args_conflicts_with_subcommands = true)]
struct TunnelsArgs {
    /// The port on this machine to expose.
    port: Option<u16>,
    /// Print the tunnel as JSON instead of a sentence.
    #[arg(long)]
    json: bool,
    #[command(subcommand)]
    command: Option<TunnelsCommand>,
}

#[derive(Subcommand)]
enum TunnelsCommand {
    /// Every tunnel open in this tenant.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Close a tunnel. Without a label, closes the ones this session opened —
    /// or, outside a session, the ones on this machine.
    Stop { label: Option<String> },
}

#[derive(Subcommand)]
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
enum SetCmd {
    /// The range of ports a node may lease to sessions on it.
    ///
    /// `nook set ports node/azul 4200-4299`, or `nook set ports azul
    /// 4200-4299`. A node with NO range leases nothing, which is the shipped
    /// default and is why a workspace declaring a REQUIRED listener cannot
    /// start a session there — deliberately, because a guessed range would hand
    /// out ports something else is already listening on.
    ///
    /// Sizing it is sizing concurrency: a workspace leases one port per declared
    /// listener, so a range of 100 against a repo declaring 11 is nine
    /// concurrent sessions, and the tenth is refused by name.
    Ports {
        /// `node/<name-or-id>`, or just the name or id. An id may be any
        /// unambiguous prefix.
        target: String,
        /// `<start>-<end>`, e.g. `4200-4299`. Omit with --clear or --exclude.
        range: Option<String>,
        /// Take the range away, so the node leases nothing.
        #[arg(long, conflicts_with = "range")]
        clear: bool,
        /// Ports on this machine to never lease, comma-separated. Ranges are
        /// allowed: `--exclude 4510,4700-4705`. Replaces the whole list.
        ///
        /// For a port something else owns HERE — a stray container, a vendor
        /// agent — including one that is not listening right now but will be
        /// after a reboot, which is the case nothing else catches.
        #[arg(long, value_name = "PORTS")]
        exclude: Option<String>,
        /// Drop every exclusion on this node.
        #[arg(long, conflicts_with = "exclude")]
        exclude_clear: bool,
        /// Act in one of your other tenants. Slug or id. Overrides
        /// NOOK_TENANT_ID; without either you get your home tenant.
        #[arg(short = 'T', long)]
        tenant: Option<String>,
    },

    /// How many loop jobs a node runs at once.
    ///
    /// `nook set capacity node/azul 4`, or `nook set capacity azul 4`. It takes
    /// effect at the next dispatch poll: nothing restarts, so every build
    /// already running on that machine is untouched.
    ///
    /// `0` is a cordon — the node stops being chosen and finishes what it
    /// holds. `--clear` hands the decision back to the machine's own
    /// `NOOK_MAX_LOOP_JOBS`.
    ///
    /// This number WINS over that variable, which is the point: the machine
    /// needing a retune is the one whose unit file already names a number. A
    /// host that must decide locally sets `NOOK_MAX_LOOP_JOBS_PINNED=1`, and
    /// this command then refuses rather than being quietly ignored.
    Capacity {
        /// `node/<name-or-id>`, or just the name or id. An id may be any
        /// unambiguous prefix.
        target: String,
        /// Concurrent loop jobs, `0` to cordon. Omit with --clear.
        jobs: Option<i64>,
        /// Fall back to whatever the node itself advertises.
        #[arg(long, conflicts_with = "jobs")]
        clear: bool,
        /// Act in one of your other tenants. Slug or id. Overrides
        /// NOOK_TENANT_ID; without either you get your home tenant.
        #[arg(short = 'T', long)]
        tenant: Option<String>,
    },
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
        /// Act in one of your other tenants. Slug or id. Overrides
        /// NOOK_TENANT_ID; without either you get your home tenant.
        #[arg(short = 'T', long)]
        tenant: Option<String>,
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
        /// Wire chat's Giphy GIF picker (secretKeys.giphyKey). Implied by
        /// --giphy-key. Off leaves chat GIF-less, which is the default.
        #[arg(long)]
        giphy: bool,
        /// Giphy API key → the printed secret command only, never written.
        #[arg(long)]
        giphy_key: Option<String>,
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
        capabilities: Box::new(capabilities::detect()),
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

/// The top-level namespace freeze (MAIN-157).
///
/// The CLI's top level accreted a flat verb per feature until it held thirty of
/// them. `nook get sessions` and `nook interactions ask` show the shape it
/// should have had — a plural noun, then a verb — and nothing stopped the next
/// ticket adding a thirty-first. This is what stops it.
///
/// **Groups are not frozen; leaves are.** A new `nook <noun> <verb>` group IS
/// the convention and needs no permission, so adding one passes. A new FLAT
/// verb is what the freeze refuses. Snapshotting everything would have made the
/// guard refuse the very thing it exists to encourage.
///
/// The surface is read from clap rather than from the source text, so it is the
/// CLI a user actually gets — `#[command(name = …)]` renames included.
#[cfg(test)]
mod cli_surface {
    use super::*;
    use clap::CommandFactory;

    /// Every flat top-level verb as of MAIN-157, sorted.
    ///
    /// **The ratchet: additions are refused, removals are welcome.** Each
    /// removal is MAIN-139 burying one more verb under a noun, so this list
    /// only ever shrinks. It is not a target to keep at thirty — it is a
    /// high-water mark to erode.
    const FROZEN_LEAVES: &[&str] = &[
        "agent-state",
        "claim",
        "comment",
        "delete",
        "enroll",
        "exec",
        "get",
        "import",
        "join",
        "label",
        "login",
        "logout",
        "notify",
        "read",
        "relate",
        "renew",
        "run",
        "send",
        "set-description",
        "setup",
        "start",
        "status",
        "task",
        "tasks",
        "taught",
        "teach",
        "unteach",
        "update",
        "whoami",
    ];

    /// Top-level entries split into `(flat verbs, noun groups)`. A group is a
    /// command that has subcommands of its own — `context`, `operator`, `k8s` —
    /// which is exactly what "put it under a noun" produces.
    ///
    /// Hidden entries are left out, because this reads the CLI a user actually
    /// gets: a deprecated alias kept for one release (`attachments`, MAIN-610)
    /// is not part of the surface, and counting it would make the guard say a
    /// group is still there after the doc, the help and the skills all stopped
    /// mentioning it.
    fn surface() -> (Vec<String>, Vec<String>) {
        let cmd = Cli::command();
        let (groups, leaves): (Vec<_>, Vec<_>) = cmd
            .get_subcommands()
            .filter(|s| !s.is_hide_set())
            .partition(|s| s.get_subcommands().next().is_some());
        let name = |v: Vec<&clap::Command>| {
            let mut v: Vec<String> = v.into_iter().map(|s| s.get_name().to_string()).collect();
            v.sort();
            v
        };
        (name(leaves), name(groups))
    }

    #[test]
    fn the_top_level_gains_no_new_flat_verbs() {
        let (leaves, _) = surface();
        let frozen: Vec<String> = FROZEN_LEAVES.iter().map(|s| s.to_string()).collect();
        if leaves == frozen {
            return;
        }

        let added: Vec<&String> = leaves.iter().filter(|l| !frozen.contains(l)).collect();
        let removed: Vec<&String> = frozen.iter().filter(|f| !leaves.contains(f)).collect();

        let mut msg = String::new();
        if !added.is_empty() {
            msg.push_str(&format!(
                "\nNew top-level verb(s): {added:?}\n\n\
                 The top level is FROZEN. New commands land as `nook <plural-noun> <verb>` \n\
                 (e.g. `issues move`, `interactions ask`) — see docs/cli-style.md.\n\n\
                 Add it under a noun, or amend this frozen list deliberately — the reviewer\n\
                 will see it.\n\n\
                 A new NOUN GROUP needs no permission and does not trip this test; only a\n\
                 flat verb does.\n"
            ));
        }
        if !removed.is_empty() {
            msg.push_str(&format!(
                "\nVerb(s) gone from the top level: {removed:?}\n\n\
                 Good — that is MAIN-139 progress. Delete them from FROZEN_LEAVES; the list\n\
                 only ever shrinks.\n"
            ));
        }
        msg.push_str(
            "\nEITHER WAY, SWEEP THE SKILLS. Adding, renaming or removing a CLI verb means\n\
             grepping skills/ for every affected command, updating each hit, bumping each\n\
             touched skill's `version:`, and noting the required `nook teach` re-run in the\n\
             PR — in the SAME ticket, never as follow-up. A skill teaching a command that no\n\
             longer exists fails on a fleet nobody has re-taught.\n",
        );
        panic!("{msg}");
    }

    /// The other half of the ruling on MAIN-157: a noun group is the shape we
    /// WANT, so it must be able to appear without touching the frozen list.
    /// This pins that freedom, so a future tightening of the guard cannot
    /// quietly turn the convention into something that also needs permission.
    #[test]
    fn noun_groups_are_not_frozen() {
        let (leaves, groups) = surface();
        assert!(
            groups.len() >= 12,
            "expected the existing noun groups; found {groups:?}"
        );
        for g in &groups {
            assert!(
                !FROZEN_LEAVES.contains(&g.as_str()),
                "{g} is a noun group and must not be in the flat-verb freeze"
            );
            assert!(!leaves.contains(g), "{g} cannot be both a group and a leaf");
        }
    }

    /// Every command a skill tells an agent to run must exist (MAIN-534 AC-7).
    ///
    /// `skills/` ships prompts that are TAUGHT to a fleet, so a verb that has
    /// been renamed does not fail at review — it fails months later, on a
    /// machine nobody re-taught, inside a loop with no human watching. The
    /// sweep is a rule in docs/cli-style.md; this is the half a machine can
    /// check.
    ///
    /// Scoped to command POSITION — an inline `` `nook x` `` or a line in a
    /// fenced block starting with `nook` — because prose says the word "nook"
    /// too, and a test that read "nook is the fleet" as a verb would be a
    /// tripwire nobody could satisfy.
    #[test]
    fn every_verb_a_skill_teaches_exists() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../skills");
        let cmd = Cli::command();
        let known: Vec<String> = cmd
            .get_subcommands()
            .flat_map(|s| {
                std::iter::once(s.get_name().to_string())
                    .chain(s.get_all_aliases().map(str::to_string))
            })
            .collect();

        let mut unknown: Vec<String> = Vec::new();
        for entry in walkdir::WalkDir::new(&root)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_name() == "SKILL.md")
        {
            let text = std::fs::read_to_string(entry.path()).expect("a readable skill");
            for verb in verbs_taught(&text) {
                if !known.contains(&verb) {
                    unknown.push(format!("{}: nook {verb}", entry.path().display()));
                }
            }
        }
        assert!(
            unknown.is_empty(),
            "\nSkills teach commands that do not exist:\n  {}\n\n\
             Sweep skills/ in THIS ticket — update every hit, bump each touched skill's\n\
             `version:`, and say in the PR that `nook teach` must be re-run.\n",
            unknown.join("\n  ")
        );
    }

    /// MAIN-610 AC-7: the four attachment verbs are `issues` verbs now, and
    /// the group they used to live in is gone from the surface.
    ///
    /// The flat set is untouched by the move — `FROZEN_LEAVES` above is the
    /// proof — because a group folding into another group never reaches the
    /// top level's leaves.
    #[test]
    fn attachment_verbs_live_under_issues() {
        let (_, groups) = surface();
        assert!(
            !groups.contains(&"attachments".to_string()),
            "`attachments` is folded into `issues`; found {groups:?}"
        );
        assert!(groups.contains(&"issues".to_string()));

        let cmd = Cli::command();
        let issues = cmd
            .get_subcommands()
            .find(|s| s.get_name() == "issues")
            .expect("the issues group");
        let verbs: Vec<&str> = issues.get_subcommands().map(|s| s.get_name()).collect();
        for v in ["attach", "attachments", "download", "detach"] {
            assert!(verbs.contains(&v), "nook issues {v} is missing: {verbs:?}");
        }
    }

    /// AC-4: the retired spelling still PARSES, so a fleet running the old
    /// skill text keeps working for one release — hidden, never removed.
    #[test]
    fn the_deprecated_attachments_group_still_routes() {
        let cmd = Cli::command();
        let old = cmd
            .get_subcommands()
            .find(|s| s.get_name() == "attachments")
            .expect("the alias is kept for one release");
        assert!(old.is_hide_set(), "the alias must not be discoverable");

        for argv in [
            vec!["nook", "attachments", "list", "MAIN-42"],
            vec!["nook", "attachments", "get", "MAIN-42/shot.png"],
            vec!["nook", "attachments", "add", "MAIN-42", "./shot.png"],
            vec!["nook", "attachments", "rm", "MAIN-42/shot.png"],
        ] {
            Cli::try_parse_from(&argv).unwrap_or_else(|e| panic!("{argv:?} must route: {e}"));
        }
    }

    /// AC-5: the sweep cannot silently rot.
    ///
    /// `every_verb_a_skill_teaches_exists` cannot catch this one — the alias
    /// deliberately still exists, so the old spelling would pass it forever.
    /// What must be true is narrower: no skill TEACHES the deprecated group,
    /// because a fleet re-taught from these files should learn only the
    /// spelling that survives the release.
    #[test]
    fn no_skill_teaches_the_deprecated_attachments_group() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../skills");
        let mut stale: Vec<String> = Vec::new();
        for entry in walkdir::WalkDir::new(&root)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_name() == "SKILL.md")
        {
            let text = std::fs::read_to_string(entry.path()).expect("a readable skill");
            for (n, line) in text.lines().enumerate() {
                if line.contains("nook attachments") {
                    stale.push(format!("{}:{}", entry.path().display(), n + 1));
                }
            }
        }
        assert!(
            stale.is_empty(),
            "\nSkills still teach `nook attachments`, which is deprecated:\n  {}\n\n\
             It is `nook issues attach|attachments|download|detach` now (MAIN-610).\n",
            stale.join("\n  ")
        );
    }

    /// The verbs a skill's text tells somebody to run.
    fn verbs_taught(text: &str) -> Vec<String> {
        let mut out = Vec::new();
        for line in text.lines() {
            let trimmed = line.trim_start().trim_start_matches("$ ");
            if let Some(rest) = trimmed.strip_prefix("nook ") {
                push_verb(rest, &mut out);
            }
            // Inline code: `nook task <KEY>` in the middle of a sentence.
            for chunk in line.split('`').skip(1).step_by(2) {
                if let Some(rest) = chunk.trim_start().strip_prefix("nook ") {
                    push_verb(rest, &mut out);
                }
            }
        }
        out
    }

    fn push_verb(rest: &str, out: &mut Vec<String>) {
        let word = rest.split_whitespace().next().unwrap_or("");
        // A flag or a variable is not a verb, and neither is `nook --help`.
        if !word.is_empty()
            && word
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && !word.starts_with('-')
        {
            out.push(word.to_string());
        }
    }
}
