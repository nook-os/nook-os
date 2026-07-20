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

/// The signed-in caller with their tenant.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MeResponse {
    pub user: User,
    pub tenant: Tenant,
}

/// Unauthenticated sign-in capabilities, so the login screen only offers what
/// this instance actually supports.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AuthProviders {
    /// An OIDC identity provider is configured.
    pub oidc: bool,
    /// The dev/CI escape hatch is enabled (never in production).
    pub dev_login: bool,
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
    pub workspace_id: WorkspaceId,
    pub node_id: NodeId,
    pub name: String,
    pub runtime: String,
    pub tmux_session: Option<String>,
    pub status: String,
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
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct PutSecretRequest {
    pub content: String,
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
