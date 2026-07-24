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
    /// Every tenant this person belongs to (from `tenant_members`), with the
    /// active one marked `current`. Carried on `me` so the UI can render a
    /// tenant switcher without a second request. A person in exactly one tenant
    /// gets a one-element list, and the UI shows a plain label for that case.
    #[serde(default)]
    pub tenants: Vec<TenantMembership>,
    /// What this caller may do, so a UI can hide what it cannot offer rather
    /// than rendering a button that 403s.
    #[serde(default)]
    pub capability: Capability,
}

/// Switch the browser session's active tenant. The caller must be a member of
/// the target tenant, or the endpoint returns 403.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SwitchTenantRequest {
    pub tenant_id: TenantId,
}

/// Unauthenticated sign-in capabilities, so the login screen only offers what
/// this instance actually supports.
/// Hand an identity provider's ID token to the control plane, get one of ours.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct OidcExchangeRequest {
    pub id_token: String,
    /// Shown in the tokens list, so a person can tell which client to revoke.
    #[serde(default)]
    pub client_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OidcExchangeResponse {
    /// Shown once. Behaves like any other user token.
    pub token: String,
    pub user: User,
    pub tenant: Tenant,
}

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
    /// The agent's own version. Reported like everything else here, so
    /// "which machines are behind?" needs no column of its own — this whole
    /// struct is already stored as jsonb on the node.
    #[serde(default)]
    pub agent_version: Option<String>,
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

// ── Skills ───────────────────────────────────────────────────────────────────

/// A skill taught to the whole fleet.
///
/// Stored by the control plane rather than pushed and forgotten, so that a node
/// which was offline when it was taught — or which joins next week — converges
/// on register instead of quietly being the one machine that never learned it.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct Skill {
    pub id: uuid::Uuid,
    pub tenant_id: TenantId,
    /// Becomes a path component on every machine: `<skills>/<name>/SKILL.md`.
    pub name: String,
    pub content: String,
    /// Of `content`. Lets a node skip a write it already has, and lets an
    /// operator see whether two machines really do hold the same thing.
    pub sha256: String,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub updated_by: Option<uuid::Uuid>,
}

/// The same thing without its body — a list of twenty skills should not ship
/// twenty documents to draw a table.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct SkillSummary {
    pub id: uuid::Uuid,
    pub name: String,
    pub sha256: String,
    /// Bytes, so the UI can show a size without holding the content.
    pub size: i64,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TeachRequest {
    /// Omitted means "derive it": from the document's own frontmatter `name:`,
    /// falling back to the filename. Explicit wins, because a file called
    /// SKILL.md says nothing about what it teaches.
    #[serde(default)]
    pub name: Option<String>,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TeachResponse {
    pub skill: SkillSummary,
    /// Nodes the fan-out actually reached. The rest converge on reconnect.
    pub delivered_to: Vec<String>,
    /// Nodes known to this tenant that were offline, named rather than
    /// counted — "3 nodes were offline" is not something an operator can act
    /// on, and silence about them would be worse.
    pub offline: Vec<String>,
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
    /// The prefix in `NOOK-42`. Unique per tenant, derived from the name when
    /// not given, and immutable once assigned — it is written into PR bodies
    /// and branch names, which no rename can reach back and fix.
    #[serde(default)]
    pub key: Option<String>,
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
    /// What this column MEANS, independent of what it is called:
    /// `backlog` | `unstarted` | `started` | `completed` | `canceled`.
    ///
    /// Automation targets the type so that renaming "In Progress" to "Doing"
    /// is a cosmetic change rather than a broken loop. The name is for people.
    #[serde(default = "default_column_type")]
    pub r#type: String,
}

fn default_column_type() -> String {
    "unstarted".into()
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
    /// `0` none, `1` urgent, `2` high, `3` medium, `4` low — Linear's
    /// convention, so values port cleanly. Note `0` sorts LAST: "nobody set a
    /// priority" is not a claim that the work is least important.
    #[serde(default)]
    pub priority: i32,
    /// Per-board sequence behind the human key. `None` only for a task created
    /// before keys existed and not yet backfilled.
    #[serde(default)]
    pub number: Option<i32>,
    /// `NOOK-42` — the board's key and this task's number. Computed, not
    /// stored: storing it would let it disagree with the two columns it is
    /// made of.
    #[serde(default)]
    #[sqlx(skip)]
    pub key: Option<String>,
    /// Absolute deep link into the web UI, so an agent reporting "filed
    /// NOOK-42" can give a human something to click.
    #[serde(default)]
    #[sqlx(skip)]
    pub url: Option<String>,
    /// Every label on this task. Populated by one query for a whole board
    /// rather than one per task.
    #[serde(default)]
    #[sqlx(skip)]
    pub labels: Vec<Label>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// ── Labels, comments, relations ─────────────────────────────────────────────

/// A tenant-wide label. `agent-ready` is the human approval gate: the one
/// signal that says an agent may pick this up, and deliberately not something
/// an agent can apply to itself.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct Label {
    pub id: Uuid,
    pub tenant_id: TenantId,
    pub name: String,
    pub color: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateLabelRequest {
    pub name: String,
    #[serde(default)]
    pub color: Option<String>,
}

/// Durable discussion on a task: the builder's blocking question, the
/// reviewer's verdict, the human's answer.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct TaskComment {
    pub id: Uuid,
    pub tenant_id: TenantId,
    pub task_id: TaskId,
    /// `user` | `agent` | `system`.
    pub author_type: String,
    #[serde(default)]
    pub author_id: Option<Uuid>,
    /// Denormalised, so an agent with no users row — and a deleted user —
    /// still render with attribution.
    pub author_name: String,
    pub body_md: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateCommentRequest {
    pub body_md: String,
    /// How an agent signs its work, e.g. `"loop-build on azul"`.
    ///
    /// NookOS has no separate agent identity — an agent acts under a person's
    /// token, so the honest record is "this user's credential, used by this
    /// tool". Supplying a name says which tool; it does not grant anything,
    /// and the underlying `author_id` remains the real user.
    #[serde(default)]
    pub author_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct UpdateCommentRequest {
    pub body_md: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct TaskRelation {
    pub id: Uuid,
    pub tenant_id: TenantId,
    pub from_task: TaskId,
    pub to_task: TaskId,
    /// `blocks` | `relates` | `duplicates`.
    pub kind: String,
    pub created_at: DateTime<Utc>,
}

/// The other end of a relation, with enough to render it without a second
/// fetch.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct RelatedTask {
    pub relation_id: Uuid,
    pub id: TaskId,
    #[serde(default)]
    pub key: Option<String>,
    pub title: String,
    pub kind: String,
    /// The column type of the other task — what makes a blocker resolved.
    pub column_type: String,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateRelationRequest {
    pub to_task: TaskId,
    pub kind: String,
}

/// One whole issue: what the loop reads before it starts work.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TaskDetail {
    pub task: TaskItem,
    pub comments: Vec<TaskComment>,
    /// Tasks that must finish before this one can start.
    pub blocked_by: Vec<RelatedTask>,
    /// Tasks waiting on this one.
    pub blocking: Vec<RelatedTask>,
    /// Non-blocking links (`relates`, `duplicates`), both directions.
    pub related: Vec<RelatedTask>,
    /// Derived from the blockers' column types, never stored — a stored flag
    /// would drift the moment a blocker moved.
    pub is_blocked: bool,
}

/// `POST /tasks/{id}/claim` — take the work without racing another agent.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct ClaimTaskRequest {
    /// Move the task here at the same time, by column TYPE. Omit to claim
    /// without moving.
    #[serde(default)]
    pub column_type: Option<String>,
    /// Claim on behalf of this user rather than the caller. For a human
    /// assigning work; agents omit it.
    #[serde(default)]
    pub assignee_user_id: Option<UserId>,
}

/// One account you can sign in as, in dev mode only.
///
/// Exists so a person can switch between users without inventing credentials —
/// testing "what does an operator see that a member does not" is impossible if
/// becoming the other person is hard.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct DevAccount {
    pub email: String,
    pub display_name: String,
    pub tenant_slug: String,
    /// Role keys held at the deployment scope — so the picker can show which
    /// of these accounts is the operator without you having to remember.
    #[serde(default)]
    pub deployment_roles: Vec<String>,
}

// ── Orgs, roles, and the operator surface ────────────────────────────────────

/// An org: the layer between a deployment and its tenants.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct Org {
    pub id: Uuid,
    pub name: String,
    pub slug: String,
    pub created_at: DateTime<Utc>,
}

/// What the signed-in caller may do, so a UI can hide what it cannot offer.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct Capability {
    /// Holds an operator binding somewhere — drives whether the operator
    /// section appears at all.
    pub operator: bool,
    /// Permission keys held at the deployment scope.
    #[serde(default)]
    pub deployment: Vec<String>,
    /// The org this caller's tenant belongs to, for reading its policy.
    #[serde(default)]
    pub org_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct OperatorOrg {
    pub id: Uuid,
    pub name: String,
    pub slug: String,
    pub created_at: DateTime<Utc>,
    pub tenants: i64,
}

/// A tenant as an operator sees it.
///
/// The first block is always visible: existence, counts, load. Everything
/// after is `Option` and stays `None` unless the org opted in — policy ADDS
/// these fields rather than filtering them out, so forgetting to add one
/// leaves it absent instead of leaking it.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct OperatorTenant {
    pub id: TenantId,
    pub slug: String,
    #[serde(default)]
    pub org_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub members: i64,
    pub nodes: i64,
    pub active_sessions: i64,
    pub workspaces: i64,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[sqlx(skip)]
    pub repositories: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[sqlx(skip)]
    pub task_titles: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct OperatorNode {
    pub id: NodeId,
    pub name: String,
    pub platform: String,
    pub status: String,
    #[serde(default)]
    pub last_seen_at: Option<DateTime<Utc>>,
    pub resources: serde_json::Value,
    pub tenant_id: TenantId,
    pub tenant_slug: String,
    pub active_sessions: i64,
}

/// An audit row. Kinds, actors and times — never payloads, which can carry the
/// very metadata policy exists to gate.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct OperatorAuditEntry {
    pub id: EventId,
    pub kind: String,
    #[serde(default)]
    pub actor_type: Option<String>,
    #[serde(default)]
    pub actor_id: Option<Uuid>,
    pub tenant_id: TenantId,
    pub tenant_slug: String,
    pub occurred_at: DateTime<Utc>,
}

/// One policy-gated field with its current state and plain-language meaning.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PolicyField {
    pub field: String,
    /// Written for a person, not a developer — every user is shown this.
    pub description: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateOrgRequest {
    pub name: String,
    #[serde(default)]
    pub slug: Option<String>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct RenameOrgRequest {
    pub name: String,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct MoveTenantRequest {
    pub org_id: Uuid,
}

/// Who holds what, for the roles table.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct BindingRow {
    pub id: Uuid,
    pub email: String,
    pub display_name: String,
    pub role_key: String,
    pub scope_type: String,
    #[serde(default)]
    pub scope_id: Option<Uuid>,
    /// The org or tenant slug the binding is scoped to, when it has one.
    #[serde(default)]
    pub scope_label: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Grant (or revoke) a deployment-scoped role.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct GrantRequest {
    pub email: String,
    /// `operator` | `org_admin` | …
    pub role: String,
    #[serde(default)]
    pub revoke: bool,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SetPolicyRequest {
    pub field: String,
    pub enabled: bool,
}

// ── Notifications ────────────────────────────────────────────────────────────

/// Something a person should see. Distinct from an `Event`, which is the
/// complete record of what happened and is never marked read.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct Notification {
    pub id: Uuid,
    pub tenant_id: TenantId,
    /// `None` means everyone in the tenant.
    #[serde(default)]
    pub user_id: Option<Uuid>,
    /// `info` | `success` | `warning` | `error`.
    pub level: String,
    pub title: String,
    pub body: String,
    /// The dotted event kind that produced it, or `custom`.
    pub kind: String,
    /// Where clicking it should go.
    #[serde(default)]
    pub link: Option<String>,
    pub payload: serde_json::Value,
    #[serde(default)]
    pub read_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct NotificationPage {
    pub notifications: Vec<Notification>,
    pub unread: i64,
}

/// Raise a notification by hand — what `nook notify` and an agent's finish
/// hook both call.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct NotifyRequest {
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    /// `info` | `success` | `warning` | `error`. Defaults to `info`.
    #[serde(default)]
    pub level: Option<String>,
    /// Defaults to `custom`. Channels filter on this.
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub link: Option<String>,
    /// The session this notification is about, when it comes from an agent hook.
    /// The control plane turns it into a deep link to the terminal (using its
    /// own public URL, which the node does not know), so clicking "an agent is
    /// waiting on you" opens the session — and external channels get a real URL,
    /// not a path. An explicit `link` still wins.
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub payload: Option<serde_json::Value>,
}

/// A configured delivery channel.
///
/// `config` is deliberately absent: it holds bot tokens and webhook URLs, and
/// a channel list is the sort of thing a UI fetches often and logs freely.
/// What a person needs to see is that it exists, whether it works, and what it
/// is filtered to.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct NotificationChannel {
    pub id: Uuid,
    pub tenant_id: TenantId,
    /// `webhook` | `slack` | `discord` | `telegram` | `twilio` | `ntfy`.
    pub kind: String,
    pub name: String,
    pub enabled: bool,
    pub levels: Vec<String>,
    pub kinds: Vec<String>,
    #[serde(default)]
    pub last_ok_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateChannelRequest {
    pub kind: String,
    pub name: String,
    /// Provider-specific. Write-only: it is never read back.
    pub config: serde_json::Value,
    #[serde(default)]
    pub levels: Vec<String>,
    #[serde(default)]
    pub kinds: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct UpdateChannelRequest {
    #[serde(default)]
    pub name: Option<String>,
    /// Omit to keep the stored secrets untouched.
    #[serde(default)]
    pub config: Option<serde_json::Value>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub levels: Option<Vec<String>>,
    #[serde(default)]
    pub kinds: Option<Vec<String>>,
}

/// What a channel kind needs, so the UI can build a form without hardcoding it.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ChannelKind {
    pub id: String,
    pub label: String,
    pub description: String,
    pub fields: Vec<ChannelField>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ChannelField {
    pub name: String,
    pub label: String,
    pub placeholder: String,
    /// Masked in the UI and never read back.
    pub secret: bool,
    pub required: bool,
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
    /// Omit to derive one from the name.
    #[serde(default)]
    pub key: Option<String>,
    pub workspace_id: Option<WorkspaceId>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct UpdateBoardRequest {
    pub name: String,
    /// Change the prefix in `NOOK-42`.
    ///
    /// Normally immutable — it is written into PR bodies and branch names that
    /// no rename can reach back and fix — but settable, because a key derived
    /// from a board name is sometimes just wrong ("NookOS Bootstrap" derives
    /// "NOOKO") and living with it forever is worse than an explicit change a
    /// person chose.
    #[serde(default)]
    pub key: Option<String>,
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
    /// Place by semantic state instead of by id — what automation wants, since
    /// it knows "the backlog" but not which uuid that is today.
    #[serde(default)]
    pub column_type: Option<String>,
    pub workspace_id: Option<WorkspaceId>,
    #[serde(default)]
    pub priority: Option<i32>,
    /// Label NAMES, created for the tenant if new. Names rather than ids
    /// because a filer knows `agent-ready`, not its uuid.
    #[serde(default)]
    pub labels: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct UpdateTaskRequest {
    pub title: Option<String>,
    pub description: Option<String>,
    pub column_id: Option<ColumnId>,
    #[serde(default)]
    pub column_type: Option<String>,
    pub position: Option<i32>,
    pub assignee_user_id: Option<UserId>,
    #[serde(default)]
    pub priority: Option<i32>,
    /// Which workspace this task belongs to. Absent leaves it alone, `null`
    /// clears it, an id sets it.
    ///
    /// Nested because those are three cases, not two, and every other field
    /// here only has two. A confined `/loop-build` agent claims only tasks in
    /// its own workspace, so an unscoped task is one no loop will ever pick
    /// up — and until this field existed there was no way to scope one after
    /// filing it. Clearing has to be expressible too, or a wrong answer is
    /// permanent.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = Option<String>, nullable)]
    pub workspace_id: Option<Option<WorkspaceId>>,
}

/// Deserialize a field that can be absent, null, or a value.
///
/// `Option<Option<T>>` on its own does not do this: serde applies a JSON
/// `null` to the OUTER option, so "clear it" and "do not touch it" both
/// arrive as `None` and the caller cannot tell them apart. Going through
/// `Deserialize` for the inner option and wrapping the result in `Some`
/// reserves the outer `None` for a field that was never sent.
fn double_option<'de, T, D>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Deserialize::deserialize(de).map(Some)
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
    /// `false` when the checkout is not a git repository — a "+ New empty
    /// project" directory, say. Everything below is then empty for a reason
    /// that is not "nothing has changed", and the UI hides the panel instead
    /// of reporting a clean tree.
    pub is_repo: bool,
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

/// A hook reporting what the agent in a session is doing.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ReportAgentStateRequest {
    /// `running` | `waiting` | `idle`.
    pub state: String,
    /// The tmux window the agent runs in, so the right terminal chip lights up.
    #[serde(default)]
    pub window: Option<u32>,
}

/// One session's current agent state, for seeding the UI on load.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AgentStateItem {
    pub session_id: SessionId,
    #[serde(default)]
    pub window: Option<u32>,
    pub state: String,
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
