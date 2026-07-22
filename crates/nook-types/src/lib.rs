//! Domain types for NookOS. Rust owns the types: everything here derives
//! `ToSchema` and flows through OpenAPI into generated TypeScript.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

/// Strongly-typed UUID newtypes. `value_type = String, format = Uuid` keeps the
/// generated OpenAPI/TS surface a plain string.
macro_rules! id_type {
    ($($name:ident),+ $(,)?) => {
        $(
            #[derive(
                Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord,
                Serialize, Deserialize, sqlx::Type, ToSchema,
            )]
            #[sqlx(transparent)]
            #[schema(value_type = String, format = Uuid)]
            pub struct $name(pub Uuid);

            impl $name {
                pub fn new() -> Self { Self(Uuid::now_v7()) }
            }

            impl Default for $name {
                fn default() -> Self { Self::new() }
            }

            impl std::fmt::Display for $name {
                fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    self.0.fmt(f)
                }
            }

            impl std::str::FromStr for $name {
                type Err = uuid::Error;
                fn from_str(s: &str) -> Result<Self, Self::Err> {
                    Ok(Self(Uuid::parse_str(s)?))
                }
            }
        )+
    };
}

id_type!(
    TenantId,
    UserId,
    IdentityId,
    AuthSessionId,
    JoinTokenId,
    NodeId,
    WorkspaceId,
    NodeWorkspaceId,
    SessionId,
    BoardId,
    ColumnId,
    TaskId,
    EventId,
    NoteId,
    ThemeId,
    SettingId,
    GitCredentialId,
);

// ── Tenancy ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct Tenant {
    pub id: TenantId,
    pub name: String,
    pub slug: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Role values: `owner` | `admin` | `member` (TEXT CHECK in the schema).
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct User {
    pub id: UserId,
    pub tenant_id: TenantId,
    pub display_name: String,
    pub email: String,
    pub avatar_url: Option<String>,
    pub role: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A tenant the caller belongs to, and the role they hold in it.
///
/// Membership is deliberately its own concept: a user has one *current*
/// tenant (`users.tenant_id`) and may reach several, which is what teams will
/// be — not a new mechanism, just more rows.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TenantMembership {
    pub id: TenantId,
    pub name: String,
    pub slug: String,
    /// `owner` | `admin` | `member`.
    pub role: String,
    /// The tenant this session is scoped to right now.
    pub current: bool,
    pub created_at: DateTime<Utc>,
}

/// The signed-in caller with their tenant.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MeResponse {
    pub user: User,
    pub tenant: Tenant,
}

/// Unauthenticated sign-in capabilities, so the login screen only offers what
/// this instance actually supports.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct LocalAuthStatus {
    /// Local sign-in is possible: the tenant is undecided, or already local.
    pub available: bool,
    /// No account exists yet, so the first visitor can claim this instance.
    pub needs_bootstrap: bool,
    /// "oidc" | "local" | null when nobody has signed in yet.
    #[serde(default)]
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct LocalLoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct LocalRegisterRequest {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ChangePasswordRequest {
    pub current: String,
    pub next: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct AuthProviders {
    /// An OIDC identity provider is configured.
    pub oidc: bool,
    /// The dev/CI escape hatch is enabled (never in production).
    pub dev_login: bool,
    /// Username and password held in this database.
    #[serde(default)]
    pub local: bool,
    /// The identity provider itself, for clients that must talk to it
    /// directly.
    ///
    /// A desktop app cannot receive the browser redirect this control plane
    /// registered, so it uses the device authorization grant against the IdP.
    /// It learns where to go from here rather than from its own configuration:
    /// the operator sets the IdP up once, on the server, and every client
    /// follows.
    #[serde(default)]
    pub oidc_issuer: Option<String>,
    /// Where a native client starts a device authorization.
    ///
    /// Read from the IdP's discovery document. `None` means the provider does
    /// not advertise one — in which case no compliant client can start the
    /// flow, whatever else the provider supports.
    #[serde(default)]
    pub device_authorization_endpoint: Option<String>,
    /// Public client id for native clients. Distinct from the control plane's
    /// own client, which is confidential and must not ship inside an app.
    #[serde(default)]
    pub device_client_id: Option<String>,
}

// ── Nodes ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct GpuInfo {
    pub vendor: String,
    pub model: String,
}

/// What a node reports about itself on registration. The control plane never
/// inspects a machine — the node describes its own capabilities.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct Capabilities {
    pub hostname: String,
    pub platform: String,
    pub architecture: String,
    pub cpus: u32,
    pub memory: u64,
    #[serde(default)]
    pub gpus: Vec<GpuInfo>,
    pub docker: bool,
    pub tmux: bool,
    pub git: Option<String>,
    /// Detected runtime executables: "claude", "hermes", "codex", "bash", ...
    #[serde(default)]
    pub runtimes: Vec<String>,
    /// This node's SSH public key (generated locally; the private half never
    /// leaves the machine). Add it as a deploy key to clone private repos.
    #[serde(default)]
    pub ssh_public_key: Option<String>,
}

/// Live resource sample a node reports on each heartbeat, so both humans and
/// the triage scheduler can see which machine can take the workload.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct NodeResources {
    /// Overall CPU utilization, 0–100.
    pub cpu_percent: f32,
    pub mem_used: u64,
    pub mem_total: u64,
    /// 1-minute load average (0 on platforms without it).
    pub load_avg1: f64,
    /// NookOS-managed sessions currently alive on the node.
    pub active_sessions: u32,
}

/// Status values: `online` | `offline`.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct Node {
    pub id: NodeId,
    pub tenant_id: TenantId,
    pub name: String,
    pub hostname: String,
    pub platform: String,
    pub capabilities: serde_json::Value,
    /// Latest heartbeat resource sample (see `NodeResources`); `{}` until first.
    pub resources: serde_json::Value,
    pub status: String,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// ── Workspaces ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub tenant_id: TenantId,
    pub name: String,
    pub slug: String,
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A workspace checked out at a path on a particular node — the join table
/// that lets one workspace exist on many machines.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct NodeWorkspace {
    pub id: NodeWorkspaceId,
    pub tenant_id: TenantId,
    pub node_id: NodeId,
    pub workspace_id: WorkspaceId,
    pub path: String,
    pub git_remote_url: Option<String>,
    pub git_branch: Option<String>,
    pub git_status: serde_json::Value,
    pub discovered_at: DateTime<Utc>,
    pub last_scanned_at: DateTime<Utc>,
}

/// A workspace location as presented to the UI (node join included).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct WorkspaceLocation {
    pub node_id: NodeId,
    pub node_name: String,
    pub node_status: String,
    pub path: String,
    pub git_branch: Option<String>,
    pub dirty: bool,
    /// This checkout is a linked git worktree of the workspace's primary repo.
    #[serde(default)]
    pub worktree: bool,
}

// ── Sessions ─────────────────────────────────────────────────────────────────

/// Status values: `starting` | `running` | `detached` | `exited` | `error`.
/// Runtime is an open string: "claude", "hermes", "codex", "bash", "zsh", ...
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct Session {
    pub id: SessionId,
    pub tenant_id: TenantId,
    /// The workspace this session runs in, or `None` for an ad-hoc terminal —
    /// a plain shell opened on a machine with no project behind it, running in
    /// the node's home directory.
    pub workspace_id: Option<WorkspaceId>,
    pub node_id: NodeId,
    pub name: String,
    pub runtime: String,
    pub tmux_session: Option<String>,
    pub status: String,
    /// Why the session failed to start, when it did.
    pub error: Option<String>,
    pub created_by: Option<UserId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
}

// ── Kanban ───────────────────────────────────────────────────────────────────

/// Provider values: `local` | `jira` | `github` | `linear` | `trello`.
/// External boards remain authoritative; NookOS federates.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct Board {
    pub id: BoardId,
    pub tenant_id: TenantId,
    pub workspace_id: Option<WorkspaceId>,
    pub name: String,
    pub provider: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct BoardColumn {
    pub id: ColumnId,
    pub board_id: BoardId,
    pub name: String,
    pub position: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct TaskItem {
    pub id: TaskId,
    pub tenant_id: TenantId,
    pub board_id: BoardId,
    pub column_id: ColumnId,
    pub title: String,
    pub description: Option<String>,
    pub position: i32,
    pub external_id: Option<String>,
    pub external_url: Option<String>,
    pub assignee_user_id: Option<UserId>,
    pub workspace_id: Option<WorkspaceId>,
    /// Node the triage scheduler chose (or you forced) to run this work.
    pub assigned_node_id: Option<NodeId>,
    pub branch: Option<String>,
    pub worktree_path: Option<String>,
    pub worktree_node_id: Option<NodeId>,
    pub session_id: Option<SessionId>,
    pub pr_url: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// ── Activity ─────────────────────────────────────────────────────────────────

/// Everything produces events. Kind is an open dotted string:
/// "node.connected", "session.started", "task.moved", "user.login", ...
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct Event {
    pub id: EventId,
    pub tenant_id: TenantId,
    pub occurred_at: DateTime<Utc>,
    pub kind: String,
    pub actor_type: Option<String>,
    pub actor_id: Option<Uuid>,
    pub workspace_id: Option<WorkspaceId>,
    pub node_id: Option<NodeId>,
    pub session_id: Option<SessionId>,
    pub payload: serde_json::Value,
}

// ── Notes ────────────────────────────────────────────────────────────────────

/// Kind values: `rolling` | `briefing` | `decision` | free-form.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct Note {
    pub id: NoteId,
    pub tenant_id: TenantId,
    pub workspace_id: WorkspaceId,
    pub title: String,
    pub content_md: String,
    pub kind: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// ── Themes ───────────────────────────────────────────────────────────────────

/// Design tokens applied as CSS custom properties. Every visual aspect is
/// configurable; unknown keys pass through untouched so theme packs can extend.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ThemeTokens {
    pub colors: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub fonts: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub spacing: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub effects: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct Theme {
    pub id: ThemeId,
    /// NULL = built-in theme shipped with NookOS.
    pub tenant_id: Option<TenantId>,
    pub name: String,
    pub slug: String,
    pub tokens: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

// ── Settings ─────────────────────────────────────────────────────────────────

/// Scope values: `tenant` | `user`.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct Setting {
    pub id: SettingId,
    pub tenant_id: TenantId,
    pub scope: String,
    pub user_id: Option<UserId>,
    pub key: String,
    pub value: serde_json::Value,
}

// ── API request/response DTOs ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct WorkspaceDetail {
    #[serde(flatten)]
    pub workspace: Workspace,
    pub locations: Vec<WorkspaceLocation>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateWorkspaceRequest {
    pub name: String,
    pub description: Option<String>,
}

/// A new label for a workspace. The name is what people read; the slug, the
/// checkouts on disk and the git remote are its identity, and none of them
/// move — which is what makes this safe to do on a whim.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct RenameWorkspaceRequest {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct BoardDetail {
    pub board: Board,
    pub columns: Vec<BoardColumn>,
    pub tasks: Vec<TaskItem>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateBoardRequest {
    pub name: String,
    pub workspace_id: Option<WorkspaceId>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct UpdateBoardRequest {
    pub name: String,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateColumnRequest {
    pub name: String,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct UpdateColumnRequest {
    pub name: Option<String>,
    pub position: Option<i32>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateTaskRequest {
    pub title: String,
    pub description: Option<String>,
    pub column_id: Option<ColumnId>,
    pub workspace_id: Option<WorkspaceId>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct UpdateTaskRequest {
    pub title: Option<String>,
    pub description: Option<String>,
    pub column_id: Option<ColumnId>,
    pub position: Option<i32>,
    pub assignee_user_id: Option<UserId>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct EventsPage {
    pub events: Vec<Event>,
    /// Pass as `before` to fetch the next (older) page.
    pub next_cursor: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateNoteRequest {
    pub title: Option<String>,
    pub content_md: String,
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct UpdateNoteRequest {
    pub title: Option<String>,
    pub content_md: Option<String>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateSessionRequest {
    pub workspace_id: WorkspaceId,
    pub node_id: NodeId,
    pub runtime: String,
    pub name: Option<String>,
    /// Pin the session to a specific checkout path (e.g. a worktree). When
    /// omitted, the workspace's first checkout on the node is used.
    pub path: Option<String>,
}

/// Open an ad-hoc terminal on a machine — a shell with no workspace, running in
/// the node's home directory. What you reach for when you just want a prompt on
/// a box, not to start work on a project.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateTerminalRequest {
    /// The runtime to run — `bash` by default, but any the node has installed.
    #[serde(default)]
    pub runtime: Option<String>,
    /// Name the session; defaults to something like "bash · <node>".
    #[serde(default)]
    pub name: Option<String>,
}

/// The node the resource-aware scheduler chose for "Auto" placement.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ScheduledNode {
    pub node_id: NodeId,
    pub node_name: String,
}

/// Sent by `nook join` (unauthenticated; the join token IS the credential).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct JoinRequest {
    pub token: String,
    pub name: String,
    pub hostname: String,
    pub platform: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct JoinResponse {
    pub node_id: NodeId,
    pub node_name: String,
    /// Long-lived node credential; shown once, stored hashed.
    pub node_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreateJoinTokenResponse {
    /// Shown exactly once; only a hash is stored.
    pub token: String,
    pub expires_at: DateTime<Utc>,
    /// SHA-256 of the certificate the joining machine should expect to see,
    /// so it can pin the server before handing over anything. `None` when the
    /// control plane does not terminate TLS itself (dev, or TLS at the edge),
    /// in which case there is nothing honest to pin to.
    #[serde(default)]
    pub ca_fingerprint: Option<String>,
    /// Where the joining machine should point its **agent** connection.
    ///
    /// Not always the API's address. The agent listener terminates TLS in the
    /// control-plane process — only it can judge a client certificate against
    /// the right tenant's CA — so it cannot sit behind the proxy that fronts
    /// the API, and deployments routinely give it its own name. A node told
    /// only the API address would enrol against a URL it must not use.
    #[serde(default)]
    pub agent_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct UpdateSettingRequest {
    pub value: serde_json::Value,
    /// `tenant` (default) or `user`.
    pub scope: Option<String>,
}

// ── Git status/diff (relayed from the node over its WebSocket) ───────────────

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct GitFileStatus {
    /// Porcelain status code, e.g. " M", "??", "A ".
    pub status: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct GitStatusResponse {
    pub branch: Option<String>,
    pub dirty: bool,
    pub files: Vec<GitFileStatus>,
    /// Unified diff of the working tree (truncated by the node if huge).
    pub diff: String,
}

// ── Git operations & vault DTOs ──────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CloneRequest {
    pub url: String,
    /// Directory name; derived from the URL when omitted.
    pub name: Option<String>,
    /// Tenant git credential to clone with (private repos over SSH).
    pub credential_id: Option<GitCredentialId>,
    /// Return as soon as the node has been asked, instead of waiting for the
    /// clone to finish. Progress arrives as activity events carrying `job_id`.
    #[serde(default)]
    pub background: bool,
}

/// A long-running operation the caller can watch instead of blocking on.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct JobAccepted {
    pub job_id: String,
    pub message: String,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct WorktreeRequest {
    pub node_id: NodeId,
    pub branch: String,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct RemoveWorktreeRequest {
    pub node_id: NodeId,
    pub path: String,
}

/// Renaming a session (tabs are named things people recognize).
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct UpdateSessionRequest {
    pub name: String,
}

/// Commit everything in a checkout, from the git panel.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct GitCommitRequest {
    /// Which machine's checkout — a workspace can exist on several.
    pub node_id: NodeId,
    pub message: String,
}

/// Push a checkout's current branch.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct GitPushRequest {
    pub node_id: NodeId,
    /// Tenant git credential to push with. Omit to use the node's own key.
    #[serde(default)]
    pub credential_id: Option<GitCredentialId>,
}

/// A tenant CA as an admin sees it. Never the private key — it is not
/// exportable, by design.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TenantCaSummary {
    pub id: String,
    /// `staged` | `active` | `retiring`.
    pub state: String,
    pub fingerprint: String,
    pub not_after: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    /// Machines still holding an unexpired leaf from this CA — the number that
    /// says whether it can be retired yet.
    pub nodes_holding_leaves: i64,
}

// ── Node enrolment (mTLS) ────────────────────────────────────────────────────

/// First contact: trade a join token for a certificate.
///
/// The node generates its keypair locally and sends only a CSR — the private
/// key never leaves the machine, so the control plane cannot leak what it was
/// never given.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct EnrollRequest {
    /// `nook_join_…`, which is what decides whose CA signs this.
    pub token: String,
    pub csr_pem: String,
    /// Name for a machine enrolling for the first time.
    #[serde(default)]
    pub name: Option<String>,
}

/// Renewal: a node asks for a fresh certificate using the key it already has.
///
/// Deliberately no join token. Tokens are for a machine with no key yet;
/// requiring one at renewal would mean expiry costs a manual re-join, which is
/// exactly what must never happen to a laptop that was closed for a month.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct RenewRequest {
    pub node_id: NodeId,
    pub csr_pem: String,
}

/// A certificate plus the trust the node needs to verify its peer.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct EnrollResponse {
    pub node_id: NodeId,
    pub cert_pem: String,
    /// EVERY CA this tenant trusts, not just the signer. A node that refreshed
    /// only its own certificate would stay pinned to a CA being retired, which
    /// is what turns a rotation into an outage.
    pub ca_bundle: Vec<String>,
    pub not_after: DateTime<Utc>,
}

/// Asking for a personal access token.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct CreateUserTokenRequest {
    /// What it's for ("laptop cli", "ci"). Shown in the list you revoke from.
    #[serde(default)]
    pub name: Option<String>,
    /// Expire it after this many days. Omit for a token that doesn't expire.
    #[serde(default)]
    pub expires_in_days: Option<i64>,
}

/// The one and only time the token itself is readable.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreateUserTokenResponse {
    /// `nook_user_…` — store it now; the server keeps only its hash.
    pub token: String,
    pub id: String,
    pub name: String,
    pub expires_at: Option<DateTime<Utc>>,
}

/// A personal access token as listed back — everything except the secret.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct UserToken {
    pub id: String,
    pub name: String,
    pub last_used_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// Keystrokes for a session. What a script sends instead of attaching a
/// terminal.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SessionInputRequest {
    pub text: String,
    /// Press Enter afterwards. Defaults to true: an unsubmitted prompt is
    /// almost never what a caller wanted.
    #[serde(default)]
    pub enter: Option<bool>,
}

/// How much of a session's screen to read back.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct SessionOutputRequest {
    /// Scrollback lines above the visible screen (0–2000). Default 0.
    #[serde(default)]
    pub history_lines: Option<u32>,
}

/// A session's current screen, with enough context to know what you're
/// looking at — `runtime` is how a caller tells a claude shell from a bash one.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SessionOutputResponse {
    /// "claude" | "hermes" | "codex" | "bash" | …
    pub runtime: String,
    /// `starting` | `running` | `detached` | `exited` | `error`.
    pub status: String,
    pub text: String,
}

/// One terminal inside a session (a tmux window).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, Default)]
pub struct SessionWindow {
    pub index: u32,
    pub name: String,
    pub active: bool,
    /// Panes in this window — >1 means it's split.
    #[serde(default)]
    pub panes: u32,
}

/// Deleting a workspace. Records always go; the checkouts on disk only go
/// when explicitly asked for (and if they stay, discovery re-adds them).
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct DeleteWorkspaceRequest {
    /// Also delete the checkout directories on every online node.
    #[serde(default)]
    pub delete_files: bool,
}

/// What a workspace delete actually did.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DeleteWorkspaceResponse {
    pub deleted: bool,
    /// Checkouts removed from disk.
    pub checkouts_removed: usize,
    /// Checkouts left behind (node offline, or removal failed) — these will
    /// be rediscovered.
    pub checkouts_remaining: usize,
    pub message: String,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct InitProjectRequest {
    pub name: String,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct StartWorkRequest {
    pub node_id: Option<NodeId>,
    pub runtime: String,
    pub branch: Option<String>,
    pub workspace_id: Option<WorkspaceId>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct StartWorkResponse {
    pub task: TaskItem,
    pub session: Session,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SubmitPrRequest {
    pub pr_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct MoveTaskRequest {
    pub column: String,
}

/// Outcome of a long-running git operation on a node.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OpResponse {
    pub ok: bool,
    pub path: Option<String>,
    pub message: String,
}

/// A tenant git credential — only the public half is ever returned.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct GitCredential {
    pub id: GitCredentialId,
    pub tenant_id: TenantId,
    pub name: String,
    pub kind: String,
    pub public_key: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateGitCredentialRequest {
    pub name: String,
    /// Paste an existing private key (OpenSSH PEM)…
    pub private_key: Option<String>,
    /// …or let the server generate an ed25519 keypair.
    #[serde(default)]
    pub generate: bool,
}

/// A workspace secret file (e.g. .env). Content only present on single-get.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct WorkspaceSecret {
    pub name: String,
    pub updated_at: DateTime<Utc>,
    pub content: Option<String>,
    /// Sealed with a passphrase — reading it needs that passphrase.
    #[serde(default)]
    pub protected: bool,
    /// Removed from checkouts when the session ends.
    #[serde(default)]
    pub ephemeral: bool,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct PutSecretRequest {
    pub content: String,
    /// The app password. Required, not optional: a `.env` moves between
    /// machines, so it is sealed with something the server never stores
    /// before the app key wraps it. A database dump plus `SECRETS_KEY` must
    /// never be enough to read one.
    pub passphrase: String,
    /// Wipe the synced file from checkouts when the session ends.
    #[serde(default)]
    pub ephemeral: bool,
}

/// Adopt a file that already exists in a checkout into the vault.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ImportSecretRequest {
    /// The app password. Same rule as saving: nothing enters the vault
    /// unsealed.
    pub passphrase: String,
    #[serde(default)]
    pub ephemeral: bool,
}

/// Whether an import left a `.env` on disk that the vault hasn't adopted yet.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SecretOnDisk {
    pub found: bool,
    /// Which checkout it was found in.
    pub checkout_path: Option<String>,
    /// Already stored in the vault, so there's nothing to adopt.
    pub in_vault: bool,
}

/// One improvement someone asked for, and what became of it.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct FeedbackItem {
    pub id: Uuid,
    pub tenant_id: TenantId,
    pub workspace_id: Option<WorkspaceId>,
    pub session_id: Option<SessionId>,
    pub body: String,
    /// queued | delivered | submitted | dropped
    pub status: String,
    pub pr_url: Option<String>,
    pub created_by: Option<UserId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SubmitFeedbackRequest {
    pub body: String,
    /// Where this feedback should be worked on. Remembered for next time;
    /// required only until one has been chosen.
    pub workspace_id: Option<WorkspaceId>,
    /// Runtime for the feedback session (defaults to claude).
    pub runtime: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct UpdateFeedbackRequest {
    pub status: Option<String>,
    pub pr_url: Option<String>,
}

/// Where feedback goes — the first-run question this answers is "which repo?"
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct FeedbackTarget {
    pub configured: bool,
    pub workspace_id: Option<WorkspaceId>,
    pub workspace_name: Option<String>,
    pub git_remote: Option<String>,
    pub session_name: String,
    /// Branch the agent is told to work on, so improvements land somewhere
    /// isolated and deployable rather than on whatever was checked out.
    pub branch: Option<String>,
    /// What the agent should do with the change once it works — reviewed,
    /// pushed, PR'd, left uncommitted. Overrides the built-in wording.
    pub instructions: Option<String>,
    /// True when the instructions came from `.nook-feedback.md` in the repo
    /// rather than from this setting, so the UI can say where they live.
    pub instructions_from_repo: bool,
}

/// Point feedback at a repo and a branch. Separate from submitting, so the
/// target can be changed at any time rather than only on the first send.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SetFeedbackTargetRequest {
    pub workspace_id: WorkspaceId,
    /// Empty means "leave the agent to pick"; a name pins every change to it.
    #[serde(default)]
    pub branch: Option<String>,
    /// Empty falls back to `.nook-feedback.md` in the repo, then to the
    /// built-in wording.
    #[serde(default)]
    pub instructions: Option<String>,
}

/// A node binary this control plane can hand out. One per platform it was
/// built with — the fleet stays on one version because the server only offers
/// the build it shipped with.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct NodeArtifact {
    /// `linux` | `darwin`, as `uname -s` lowercased.
    pub os: String,
    /// `x86_64` | `aarch64`, normalized from `uname -m`.
    pub arch: String,
    /// Human label for the picker ("macOS · Apple silicon").
    pub label: String,
    pub filename: String,
    /// Where to download it — a GitHub release asset. The control plane no
    /// longer hosts binaries, so it deliberately reports neither size nor
    /// checksum: it cannot attest to bytes it does not serve, and a stale
    /// digest is worse than none.
    pub url: String,
}

/// Everything the "add node" flow needs: what to download, and where the
/// one-shot installer lives.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct NodeReleases {
    /// Version of the control plane, which is the version of these binaries.
    pub version: String,
    /// URL of the generated install script.
    pub install_url: String,
    /// This instance as the caller reached it — what a new machine should use.
    pub base_url: String,
    pub artifacts: Vec<NodeArtifact>,
}

/// Whether this user has an app password (the key that seals their secrets).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct VaultStatus {
    pub configured: bool,
    pub created_at: Option<DateTime<Utc>>,
    /// How many passkeys can unlock this vault. Non-zero means the UI should
    /// reach for a passkey before asking anyone to type a password.
    #[serde(default)]
    pub passkeys: i64,
}

/// A passkey enrolled to unlock the vault.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct VaultPasskey {
    pub id: uuid::Uuid,
    /// Base64url WebAuthn credential id, so the browser can ask for this
    /// specific passkey.
    pub credential_id: String,
    pub label: String,
    /// The app password sealed under the passkey-derived key, base64. Only
    /// the browser that holds the passkey can open it.
    pub wrapped_secret: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

/// Enrolling a passkey. The wrapping happens in the browser; the server only
/// ever sees the sealed blob.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct AddPasskeyRequest {
    pub credential_id: String,
    #[serde(default)]
    pub label: String,
    pub wrapped_secret: String,
}

/// Setting or checking the app password.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SetVaultPassphraseRequest {
    pub passphrase: String,
}

/// Unlocking a protected secret.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct OpenSecretRequest {
    pub passphrase: String,
}

// ── Dispatcher ───────────────────────────────────────────────────────────────

/// The dispatcher recommends; humans approve. It never codes, edits, or
/// deploys — suggestions are the entire output surface.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct DispatchSuggestion {
    pub headline: String,
    pub items: Vec<DispatchItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct DispatchItem {
    pub task_id: Option<TaskId>,
    pub title: String,
    pub rationale: String,
    pub suggested_runtime: Option<String>,
    pub workspace_id: Option<WorkspaceId>,
}
