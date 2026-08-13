//! NookOS MCP server: control NookOS from any MCP client.
//!
//! Tools delegate to a [`NookBackend`] implemented by the control plane's
//! service layer, so REST and MCP can never drift apart. The dependency
//! direction is control-plane → this crate; the trait keeps it acyclic.

use std::sync::Arc;

use async_trait::async_trait;
use axum::http::request::Parts;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo};
use rmcp::schemars::{self, JsonSchema};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::StreamableHttpService;
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler};
use serde::Deserialize;
use uuid::Uuid;

use nook_errors::ApiError;
use nook_types::{
    AttachmentContent, CreateUserNote, CreateUserNoteFolder, Event, LoopRunLookup, LoopRunSummary,
    Node, Note, Session, TaskAttachment, TaskItem, TenantId, TunnelView, UpdateUserNote,
    UpdateUserNoteFolder, UserId, UserNote, UserNoteFolder, UserNoteFolderId, UserNoteId,
    UserNoteSummary, WorkspaceDetail,
};

/// The per-request MCP caller identity resolved from an OIDC bearer (MAIN-102).
///
/// The control plane's `mcp_auth` middleware resolves the token's userinfo to a
/// NookOS user and inserts this into the HTTP request's extensions; rmcp
/// forwards `http::request::Parts` into each tool's request context, so a
/// notebook tool reads it back with `parts.extensions.get::<McpCaller>()`. The
/// static `MCP_TOKEN` path inserts nothing — those calls have no person, and
/// every notebook tool refuses them.
#[derive(Debug, Clone)]
pub struct McpCaller {
    pub person_id: Uuid,
    pub user_id: UserId,
    pub tenant_id: TenantId,
}

/// Everything the MCP surface is allowed to do, pre-scoped to a tenant.
#[async_trait]
pub trait NookBackend: Send + Sync + 'static {
    async fn list_workspaces(&self) -> anyhow::Result<Vec<WorkspaceDetail>>;
    async fn list_nodes(&self) -> anyhow::Result<Vec<Node>>;
    async fn list_sessions(&self, active_only: bool) -> anyhow::Result<Vec<Session>>;
    /// Start a session in `workspace` (name or slug). Picks an online node
    /// with a checkout when `node` is not given.
    async fn start_session(
        &self,
        workspace: String,
        node: Option<String>,
        runtime: String,
    ) -> anyhow::Result<Session>;
    /// Inject text into a running session's terminal (the "task injection"
    /// primitive). AI recommends; a human watching the session can always
    /// interrupt or take over.
    async fn send_to_session(&self, session_id: String, text: String) -> anyhow::Result<()>;
    /// Read a session's terminal screen (plus history tail) as plain text —
    /// the observe half of send_to_session, enabling send → read → act loops.
    async fn read_session(&self, session_id: String, history_lines: u32) -> anyhow::Result<String>;
    /// End a session for real (kills the tmux session on the node).
    async fn kill_session(&self, session_id: String) -> anyhow::Result<()>;
    async fn get_activity(
        &self,
        workspace: Option<String>,
        limit: i64,
    ) -> anyhow::Result<Vec<Event>>;
    async fn get_notes(&self, workspace: String) -> anyhow::Result<Vec<Note>>;
    async fn append_note(&self, workspace: String, content: String) -> anyhow::Result<Note>;
    // `parent` (MAIN-81): file under an epic — a uuid or key of a `type='epic'`
    // task on the same board; omit for a top-level task.
    async fn create_task(
        &self,
        title: String,
        description: Option<String>,
        parent: Option<String>,
    ) -> anyhow::Result<TaskItem>;

    // ── Git-powerhouse management (drive workspace/project creation) ─────────
    /// Clone a repo onto a node (name or slug). Returns a status message.
    async fn clone_repo(&self, url: String, node: Option<String>) -> anyhow::Result<String>;
    /// Create a new empty git project on a node.
    async fn create_project(&self, name: String, node: Option<String>) -> anyhow::Result<String>;
    /// Add a worktree (branch) of a workspace on a node.
    async fn add_worktree(
        &self,
        workspace: String,
        branch: String,
        node: Option<String>,
    ) -> anyhow::Result<String>;

    // ── Kanban-driven work ──────────────────────────────────────────────────
    /// Triage-dispatch a task: the scheduler picks the best online node the
    /// **caller** may use. `caller` is the authenticated MCP person (MAIN-223
    /// AC-4) — the same person-based authorization the HTTP route applies.
    async fn dispatch_task(&self, caller: McpCaller, task_id: String) -> anyhow::Result<TaskItem>;
    /// Start work on a task: worktree + session. `runtime` defaults to bash.
    /// `caller` is the authenticated MCP person, threaded so spawn authorization
    /// (own/shared node rules) applies exactly as on the HTTP route (AC-4).
    async fn start_work(
        &self,
        caller: McpCaller,
        task_id: String,
        runtime: Option<String>,
        node: Option<String>,
    ) -> anyhow::Result<Session>;
    /// Move a task to a named column (Triage/Todo/In Progress/Done).
    async fn move_task(&self, task_id: String, column: String) -> anyhow::Result<TaskItem>;
    /// Record a PR for a task and move it to Done.
    async fn submit_pr(&self, task_id: String, pr_url: Option<String>) -> anyhow::Result<TaskItem>;

    // ── The agent loop's primitives ─────────────────────────────────────────
    //
    // Every one of these takes a task by human key (`NOOK-42`) as well as a
    // uuid, because a key is what an agent is handed — in a PR body, in a
    // branch name, in the reply to filing an issue.
    /// Find work: the compound pick filter, as one query. By default it excludes
    /// tasks in a `backlog` column, tasks in a `completed`/`canceled` column and
    /// `type='epic'` tasks — the backlog is a human refinement space, a finished
    /// card is over, and an epic is a container; none is buildable.
    async fn list_tasks(&self, f: TaskQuery) -> anyhow::Result<Vec<TaskItem>>;
    /// One whole issue: body, labels, comments, relations, blocked state.
    async fn get_task(&self, task: String) -> anyhow::Result<serde_json::Value>;
    /// Take a task, atomically. Fails if somebody else got there first.
    async fn claim_task(
        &self,
        task: String,
        column_type: Option<String>,
    ) -> anyhow::Result<TaskItem>;
    /// Give a task back so somebody else can pick it up.
    async fn release_task(&self, task: String) -> anyhow::Result<TaskItem>;
    async fn comment_task(
        &self,
        task: String,
        body_md: String,
        author_name: Option<String>,
    ) -> anyhow::Result<serde_json::Value>;
    /// Safely replace a task's description. Reads the current version and writes
    /// with an optimistic-concurrency guard, retrying on a concurrent edit — so
    /// a body-edit from an agent never silently clobbers a human's change.
    async fn set_task_description(
        &self,
        task: String,
        description: String,
    ) -> anyhow::Result<TaskItem>;
    async fn add_label(&self, task: String, label: String) -> anyhow::Result<serde_json::Value>;
    async fn remove_label(&self, task: String, label: String) -> anyhow::Result<serde_json::Value>;
    async fn set_priority(&self, task: String, priority: i32) -> anyhow::Result<TaskItem>;
    /// File a task under an epic, or detach it (MAIN-81). `parent` is a uuid or
    /// key of a `type='epic'` task on the same board; omit `parent` to detach.
    async fn set_task_parent(
        &self,
        task: String,
        parent: Option<String>,
    ) -> anyhow::Result<TaskItem>;
    async fn link_tasks(
        &self,
        from: String,
        to: String,
        kind: String,
    ) -> anyhow::Result<serde_json::Value>;

    // ── Attachments (MAIN-534) ──────────────────────────────────────────────
    //
    // Read-only on purpose: a ticket's files are part of the brief an agent is
    // handed, and an agent that cannot see them is working from an incomplete
    // one. Uploading stays a person's act (NG-1).
    /// Every file on a ticket AND on its comments — metadata only, never bytes,
    /// so listing twenty screenshots costs one small answer.
    async fn list_task_attachments(&self, task: String) -> anyhow::Result<Vec<TaskAttachment>>;
    /// One attachment's content, when it is text small enough to belong in a
    /// reply. Anything else comes back as a pointer to the CLI.
    async fn read_task_attachment(&self, attachment: String) -> anyhow::Result<AttachmentContent>;

    // ── Build runs (MAIN-525) ───────────────────────────────────────────────
    //
    // The read half of the loop, and deliberately a POLLING shape: a question
    // asked gets an answer returned, so it needs nothing stateful and works
    // from any client. `read_session` is the precedent — this is the same
    // observe move, pointed at a run instead of a terminal.
    /// A workspace's loop runs, newest first.
    async fn list_build_runs(
        &self,
        caller: McpCaller,
        q: BuildRunQuery,
    ) -> anyhow::Result<Vec<LoopRunSummary>>;
    /// One run — by card key (its newest run) or by run id — with a transcript
    /// tail bounded to `tail_lines`. A card nothing has run answers with an
    /// empty lookup naming the card, never an error.
    async fn get_build_run(
        &self,
        caller: McpCaller,
        run: String,
        tail_lines: u32,
    ) -> anyhow::Result<LoopRunLookup>;

    // ── Tunnels (MAIN-11) ───────────────────────────────────────────────────
    //
    // A second door onto the endpoints MAIN-404 built, never a second
    // implementation: the naming, the collision handling, the zone requirement
    // and the node gate all stay where the CLI already finds them.
    /// Expose `port` on the node the session runs on. The session is named
    /// rather than read from the environment — an MCP caller is not inside one.
    async fn open_tunnel(
        &self,
        caller: McpCaller,
        session_id: String,
        port: u16,
    ) -> anyhow::Result<TunnelView>;
    /// Every tunnel open in the caller's tenant.
    async fn list_tunnels(&self, caller: McpCaller) -> anyhow::Result<Vec<TunnelView>>;
    /// Close one by its label. A label with no tunnel behind it is an error.
    async fn stop_tunnel(&self, caller: McpCaller, label: String) -> anyhow::Result<()>;

    // ── Notebook (person-scoped; MAIN-102) ──────────────────────────────────
    // Unlike every method above (pre-scoped to the instance's first tenant),
    // these take a resolved `person` (a `users.person_id`) — the notebook is
    // private to a person, so there is no instance fallback. They route through
    // the notebook module's own service paths (validation, encryption, decrypt).
    async fn notebook_list_notes(
        &self,
        person: Uuid,
        q: Option<String>,
    ) -> anyhow::Result<Vec<UserNoteSummary>>;
    async fn notebook_get_note(&self, person: Uuid, id: UserNoteId) -> anyhow::Result<UserNote>;
    async fn notebook_create_note(
        &self,
        person: Uuid,
        req: CreateUserNote,
    ) -> anyhow::Result<UserNote>;
    async fn notebook_update_note(
        &self,
        person: Uuid,
        id: UserNoteId,
        req: UpdateUserNote,
    ) -> anyhow::Result<UserNote>;
    async fn notebook_delete_note(&self, person: Uuid, id: UserNoteId) -> anyhow::Result<()>;
    async fn notebook_list_folders(&self, person: Uuid) -> anyhow::Result<Vec<UserNoteFolder>>;
    async fn notebook_create_folder(
        &self,
        person: Uuid,
        req: CreateUserNoteFolder,
    ) -> anyhow::Result<UserNoteFolder>;
    /// Rename and/or MOVE. `parent_id` is tri-state exactly like a note's
    /// `folder_id`, and the shared service path refuses a move that would put a
    /// folder inside its own subtree.
    async fn notebook_update_folder(
        &self,
        person: Uuid,
        id: UserNoteFolderId,
        req: UpdateUserNoteFolder,
    ) -> anyhow::Result<UserNoteFolder>;
    /// Delete a folder, REPARENTING its contents up one level. Never deletes
    /// notes.
    async fn notebook_delete_folder(
        &self,
        person: Uuid,
        id: UserNoteFolderId,
    ) -> anyhow::Result<()>;
}

/// The pick filter, mirroring `GET /api/v1/tasks`.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct TaskQuery {
    /// Board id or key. Omit to search every board.
    pub board: Option<String>,
    /// Labels that must ALL be present, e.g. ["agent-ready"].
    #[serde(default)]
    pub label: Vec<String>,
    /// Labels that must NOT be present, e.g. ["blocked"].
    #[serde(default)]
    pub not_label: Vec<String>,
    /// A user id, or "none" for unclaimed work.
    pub assignee: Option<String>,
    /// backlog | unstarted | started | completed | canceled
    pub column_type: Option<String>,
    /// 0 none, 1 urgent, 2 high, 3 medium, 4 low.
    pub priority: Option<i32>,
    /// false excludes anything with an unresolved blocker.
    pub is_blocked: Option<bool>,
    /// An epic's children (MAIN-81): a uuid or key. Returns the tickets filed
    /// under that epic, across every column.
    pub parent: Option<String>,
    /// Include tasks in a `backlog` column (default false — the backlog is a
    /// human refinement space the loop never draws from).
    pub backlog: Option<bool>,
    /// Include tasks in a `completed`/`canceled` column (default false —
    /// finished work is not work).
    pub done: Option<bool>,
    pub limit: Option<i64>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ListSessionsParams {
    /// Only sessions that are currently starting/running/detached.
    #[serde(default)]
    pub active_only: bool,
}

#[derive(Deserialize, JsonSchema)]
pub struct StartSessionParams {
    /// Workspace name or slug.
    pub workspace: String,
    /// Node name; defaults to any online node that has the workspace.
    pub node: Option<String>,
    /// Runtime executable: "claude", "hermes", "codex", "bash", ...
    pub runtime: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct SendToSessionParams {
    pub session_id: String,
    /// Text to type into the session. Include "\n" to submit it.
    pub text: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ReadSessionParams {
    pub session_id: String,
    /// History lines above the visible screen to include (default 100).
    pub history_lines: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
pub struct KillSessionParams {
    pub session_id: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct GetActivityParams {
    /// Workspace name or slug to filter by.
    pub workspace: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: i64,
}

fn default_limit() -> i64 {
    50
}

#[derive(Deserialize, JsonSchema)]
pub struct WorkspaceParams {
    /// Workspace name or slug.
    pub workspace: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct AppendNoteParams {
    /// Workspace name or slug.
    pub workspace: String,
    /// Markdown to append to the rolling note.
    pub content: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct CreateTaskParams {
    pub title: String,
    pub description: Option<String>,
    /// File under an epic (MAIN-81): a uuid or key of a `type='epic'` task on
    /// the same board. Omit for a top-level task.
    pub parent: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct CloneRepoParams {
    /// Git URL (GitHub/GitLab/Bitbucket/raw).
    pub url: String,
    /// Node name; auto-picked when omitted.
    pub node: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct CreateProjectParams {
    pub name: String,
    pub node: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct AddWorktreeParams {
    /// Workspace name or slug.
    pub workspace: String,
    pub branch: String,
    pub node: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct TaskIdParams {
    pub task_id: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct StartWorkParams {
    pub task_id: String,
    /// Runtime: claude/hermes/codex/bash/… (defaults to bash).
    pub runtime: Option<String>,
    pub node: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct MoveTaskParams {
    pub task_id: String,
    /// Column name: Triage / Todo / In Progress / Done.
    pub column: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct TaskRefParams {
    /// Human key (NOOK-42) or uuid.
    pub task: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct AttachmentRefParams {
    /// The attachment id, as `list_task_attachments` prints it.
    pub attachment: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct SetDescriptionParams {
    /// Human key (NOOK-42) or uuid.
    pub task: String,
    /// The new description (Markdown). Replaces the whole body; read it first
    /// with `get_task` and edit the text you want to change.
    pub description: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ClaimParams {
    /// Human key (NOOK-42) or uuid.
    pub task: String,
    /// Move it here at the same time, by type — usually "started".
    pub column_type: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct CommentParams {
    pub task: String,
    /// Markdown.
    pub body_md: String,
    /// Which tool is speaking, e.g. "loop-review on azul".
    pub author_name: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct LabelParams {
    pub task: String,
    /// Label name, e.g. "blocked".
    pub label: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct PriorityParams {
    pub task: String,
    /// 0 none, 1 urgent, 2 high, 3 medium, 4 low.
    pub priority: i32,
}

#[derive(Deserialize, JsonSchema)]
pub struct SetTaskParentParams {
    /// The task to (re)file — a uuid or key.
    pub task: String,
    /// The epic to file it under (uuid or key of a `type='epic'` task on the
    /// same board). Omit to DETACH the task from its epic.
    pub parent: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct LinkParams {
    /// The task doing the blocking/relating.
    pub from: String,
    pub to: String,
    /// blocks | relates | duplicates
    pub kind: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct SubmitPrParams {
    pub task_id: String,
    pub pr_url: Option<String>,
}

/// The build-run list filter (MAIN-525 AC-1).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct BuildRunQuery {
    /// Workspace name, slug or id — the repo whose runs to list.
    pub workspace: String,
    /// Only runs still in flight (queued/claimed/running/waiting_on_human).
    #[serde(default)]
    pub live_only: bool,
    /// "build" (the default), "review", "spec", "decompose", "epic-run", or
    /// "any" for every kind of run this repo has had.
    pub kind: Option<String>,
    /// Newest-first page size. Default 20, capped at 100.
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetBuildRunParams {
    /// A card key (`MAIN-42` — answers with that card's NEWEST run) or a run id.
    pub run: String,
    /// Transcript lines to return from the END of the run (default 100, max
    /// 2000), mirroring read_session's `history_lines`. The response says what
    /// it left out.
    pub tail_lines: Option<u32>,
}

/// Open a tunnel to a port on a session's node (MAIN-11 AC-1).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct OpenTunnelParams {
    /// The session whose machine the port is on. It also binds the tunnel's
    /// lifetime: the tunnel ends when the session does.
    pub session_id: String,
    /// The port the app is listening on, on that machine's loopback.
    pub port: u16,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StopTunnelParams {
    /// The tunnel's generated name — the `label` create and list return.
    pub label: String,
}

#[derive(Clone)]
pub struct NookMcp {
    backend: Arc<dyn NookBackend>,
}

fn to_result<T: serde::Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    let json = serde_json::to_string_pretty(value).map_err(|e| {
        // Serializing OUR OWN response failed — a server bug, and the caller
        // learns the same nothing they learn about any other one (AC-2). The
        // serde message is diagnostic and goes to the log.
        tracing::error!(error = %e, "cannot serialize an mcp tool result");
        McpError::internal_error(GENERIC, None)
    })?;
    Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
}

/// A backend failure as the MCP error a caller can act on (MAIN-275).
///
/// Every tool used to flatten to `internal_error(e.to_string())`, which got two
/// things wrong at once. An agent could not tell "you may not do that" from
/// "that does not exist" — both arrived as a generic internal error — and the
/// `to_string()` handed the caller whatever the failure happened to say, which
/// is the leak REST deliberately never allows.
///
/// The backend returns `anyhow::Result`, but the services under it return
/// `ApiResult`, so `?` leaves the `ApiError` inside the chain. Downcasting
/// recovers the semantics without changing a single tool signature (NG-1).
///
/// The rule is REST's, applied here: a 4xx tells the caller what they did, a
/// 5xx tells them nothing and the detail goes to the log.
fn backend_err(e: anyhow::Error) -> McpError {
    let Some(api) = e.downcast_ref::<ApiError>() else {
        // Not an ApiError at all — a plumbing failure with no HTTP meaning.
        // Generic to the caller, logged here, same as `ApiError::Internal`.
        tracing::error!(error = %e, "mcp tool failed");
        return McpError::internal_error(GENERIC, None);
    };
    match api {
        // What the caller may do. `invalid_request` rather than
        // `invalid_params`: the parameters were fine, the caller is not.
        ApiError::Unauthorized | ApiError::Forbidden => {
            McpError::invalid_request(api.to_string(), None)
        }
        ApiError::ForbiddenMsg(m) => McpError::invalid_request(m.clone(), None),
        // What the caller asked for. `resource_not_found` is the kind that
        // exists for exactly this, and is what lets an agent retry with a
        // different name instead of giving up.
        ApiError::NotFound => McpError::resource_not_found(api.to_string(), None),
        // What the caller sent. A conflict is the caller's to resolve too —
        // re-sending the same thing will fail the same way.
        ApiError::BadRequest(m)
        | ApiError::Conflict(m)
        | ApiError::PayloadTooLarge(m)
        | ApiError::Unprocessable(m) => McpError::invalid_params(m.clone(), None),
        ApiError::TooManyRequests(m) | ApiError::SetupRequired(m) => {
            McpError::invalid_request(m.clone(), None)
        }
        ApiError::ServiceUnavailable(m) => McpError::internal_error(m.clone(), None),
        // Ours, not theirs. The message is fixed and the cause is logged —
        // never `e.to_string()`, which is what leaked before.
        ApiError::Db(_) | ApiError::Internal(_) => {
            tracing::error!(error = %api, "mcp tool failed");
            McpError::internal_error(GENERIC, None)
        }
    }
}

/// The one thing an MCP caller is ever told about a server-side failure. Matches
/// REST's body exactly, so the two surfaces cannot drift into disagreeing about
/// how much a client learns.
const GENERIC: &str = "internal error";

/// `read_session`'s `history_lines` default, applied to a run's transcript so
/// the two observe tools answer at the same size (MAIN-525 AC-2).
pub const DEFAULT_TAIL_LINES: u32 = 100;

/// The notebook caller's person, or the explicit not-authenticated error every
/// notebook tool returns for the static `MCP_TOKEN` path (which carries no
/// person). The identity rides in the request's extensions (see [`McpCaller`]).
fn require_person(parts: &Parts) -> Result<Uuid, McpError> {
    require_caller(parts).map(|c| c.person_id)
}

/// The full authenticated MCP caller, or the same pointed refusal `require_person`
/// gives the static-`MCP_TOKEN` path. Kanban work tools (dispatch/start-work) need
/// the caller's `user_id`/`tenant_id`, not just the person, to authorize exactly
/// as the HTTP routes do (MAIN-223 AC-4).
fn require_caller(parts: &Parts) -> Result<McpCaller, McpError> {
    parts.extensions.get::<McpCaller>().cloned().ok_or_else(|| {
        McpError::invalid_request(
            "MCP is not authenticated as a user — connect with your own token".to_string(),
            None,
        )
    })
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NotebookCreateFolderParams {
    /// The folder's name.
    pub name: String,
    /// Put it inside this folder. Omit for a folder at the notebook root.
    #[serde(default)]
    pub parent_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NotebookRenameFolderParams {
    /// The folder to rename.
    pub id: String,
    /// Its new name.
    pub name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NotebookMoveFolderParams {
    /// The folder to move.
    pub id: String,
    /// Move it under this folder. Omit (or pass an empty string) to move it to
    /// the notebook root. A move that would put a folder inside its own subtree
    /// is refused.
    #[serde(default)]
    pub parent_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NotebookFolderIdParams {
    pub id: String,
}

/// The tri-state an UPDATE's move needs, from the flat string MCP clients can
/// express: absent = leave alone, empty = move to the root, an id = move under
/// it.
///
/// A tool schema cannot easily carry `Option<Option<T>>`, and "" is the one
/// value that is never a valid uuid — so it is unambiguous rather than a
/// sentinel that could collide with real input.
fn parse_move_target(s: Option<String>) -> Result<Option<Option<UserNoteFolderId>>, McpError> {
    match s {
        None => Ok(None),
        Some(v) if v.trim().is_empty() => Ok(Some(None)),
        Some(v) => Ok(Some(Some(parse_folder_id(v.trim())?))),
    }
}

/// A parent folder for a tool whose whole job is placement — create, and the
/// dedicated move. Here absent means the ROOT, not "leave alone": there is no
/// prior place to leave a new folder in, and a move that changes nothing is not
/// a move a client meant to ask for.
fn parse_parent(s: Option<String>) -> Result<Option<UserNoteFolderId>, McpError> {
    match s.as_deref().map(str::trim) {
        None | Some("") => Ok(None),
        Some(v) => Ok(Some(parse_folder_id(v)?)),
    }
}

fn parse_note_id(s: &str) -> Result<UserNoteId, McpError> {
    s.parse()
        .map_err(|_| McpError::invalid_params(format!("invalid note id {s:?}"), None))
}

fn parse_folder_id(s: &str) -> Result<UserNoteFolderId, McpError> {
    s.parse()
        .map_err(|_| McpError::invalid_params(format!("invalid folder id {s:?}"), None))
}

#[derive(Deserialize, JsonSchema)]
pub struct NotebookSearchParams {
    /// Case-insensitive substring matched against note title + folder path.
    pub q: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct NotebookNoteIdParams {
    /// The note's id.
    pub id: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct NotebookCreateParams {
    pub title: String,
    /// Markdown body. Defaults to empty.
    pub content_md: Option<String>,
    /// Optional folder id to file the note under.
    pub folder_id: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct NotebookUpdateParams {
    pub id: String,
    /// New title. Omit to leave unchanged.
    pub title: Option<String>,
    /// New markdown body. Omit to leave unchanged.
    pub content_md: Option<String>,
    /// Move the note into this folder id, at any depth. Omit to leave it where
    /// it is; pass an empty string to move it to the notebook root.
    pub folder_id: Option<String>,
}

#[tool_router]
impl NookMcp {
    pub fn new(backend: Arc<dyn NookBackend>) -> Self {
        Self { backend }
    }

    #[tool(
        description = "List workspaces with their locations (which nodes have them checked out)"
    )]
    async fn list_workspaces(&self) -> Result<CallToolResult, McpError> {
        to_result(&self.backend.list_workspaces().await.map_err(backend_err)?)
    }

    #[tool(description = "List nodes (machines) with status and capabilities")]
    async fn list_nodes(&self) -> Result<CallToolResult, McpError> {
        to_result(&self.backend.list_nodes().await.map_err(backend_err)?)
    }

    #[tool(description = "List terminal/AI sessions")]
    async fn list_sessions(
        &self,
        Parameters(p): Parameters<ListSessionsParams>,
    ) -> Result<CallToolResult, McpError> {
        to_result(
            &self
                .backend
                .list_sessions(p.active_only)
                .await
                .map_err(backend_err)?,
        )
    }

    #[tool(
        description = "Start a tmux-backed session (claude/hermes/codex/bash/...) in a workspace"
    )]
    async fn start_session(
        &self,
        Parameters(p): Parameters<StartSessionParams>,
    ) -> Result<CallToolResult, McpError> {
        to_result(
            &self
                .backend
                .start_session(p.workspace, p.node, p.runtime)
                .await
                .map_err(backend_err)?,
        )
    }

    #[tool(description = "Type text into a running session's terminal (task injection)")]
    async fn send_to_session(
        &self,
        Parameters(p): Parameters<SendToSessionParams>,
    ) -> Result<CallToolResult, McpError> {
        self.backend
            .send_to_session(p.session_id, p.text)
            .await
            .map_err(backend_err)?;
        Ok(CallToolResult::success(vec![ContentBlock::text("sent")]))
    }

    #[tool(
        description = "Read a session's terminal screen (plus recent history) as plain text — \
                       the observe half of send_to_session"
    )]
    async fn read_session(
        &self,
        Parameters(p): Parameters<ReadSessionParams>,
    ) -> Result<CallToolResult, McpError> {
        let text = self
            .backend
            .read_session(p.session_id, p.history_lines.unwrap_or(100))
            .await
            .map_err(backend_err)?;
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(description = "Kill a session (ends its tmux session on the node for real)")]
    async fn kill_session(
        &self,
        Parameters(p): Parameters<KillSessionParams>,
    ) -> Result<CallToolResult, McpError> {
        self.backend
            .kill_session(p.session_id)
            .await
            .map_err(backend_err)?;
        Ok(CallToolResult::success(vec![ContentBlock::text("killed")]))
    }

    #[tool(description = "Get the activity timeline (chronological events)")]
    async fn get_activity(
        &self,
        Parameters(p): Parameters<GetActivityParams>,
    ) -> Result<CallToolResult, McpError> {
        to_result(
            &self
                .backend
                .get_activity(p.workspace, p.limit)
                .await
                .map_err(backend_err)?,
        )
    }

    #[tool(description = "Read a workspace's rolling notes")]
    async fn get_notes(
        &self,
        Parameters(p): Parameters<WorkspaceParams>,
    ) -> Result<CallToolResult, McpError> {
        to_result(
            &self
                .backend
                .get_notes(p.workspace)
                .await
                .map_err(backend_err)?,
        )
    }

    #[tool(description = "Append markdown to a workspace's rolling note")]
    async fn append_note(
        &self,
        Parameters(p): Parameters<AppendNoteParams>,
    ) -> Result<CallToolResult, McpError> {
        to_result(
            &self
                .backend
                .append_note(p.workspace, p.content)
                .await
                .map_err(backend_err)?,
        )
    }

    #[tool(description = "Create a task on the local board")]
    async fn create_task(
        &self,
        Parameters(p): Parameters<CreateTaskParams>,
    ) -> Result<CallToolResult, McpError> {
        to_result(
            &self
                .backend
                .create_task(p.title, p.description, p.parent)
                .await
                .map_err(backend_err)?,
        )
    }

    #[tool(
        description = "Clone a git repository onto a node (auto-picks an online node if omitted)"
    )]
    async fn clone_repo(
        &self,
        Parameters(p): Parameters<CloneRepoParams>,
    ) -> Result<CallToolResult, McpError> {
        let msg = self
            .backend
            .clone_repo(p.url, p.node)
            .await
            .map_err(backend_err)?;
        Ok(CallToolResult::success(vec![ContentBlock::text(msg)]))
    }

    #[tool(description = "Create a new empty git project on a node")]
    async fn create_project(
        &self,
        Parameters(p): Parameters<CreateProjectParams>,
    ) -> Result<CallToolResult, McpError> {
        let msg = self
            .backend
            .create_project(p.name, p.node)
            .await
            .map_err(backend_err)?;
        Ok(CallToolResult::success(vec![ContentBlock::text(msg)]))
    }

    #[tool(description = "Add a git worktree (branch) of a workspace on a node")]
    async fn add_worktree(
        &self,
        Parameters(p): Parameters<AddWorktreeParams>,
    ) -> Result<CallToolResult, McpError> {
        let msg = self
            .backend
            .add_worktree(p.workspace, p.branch, p.node)
            .await
            .map_err(backend_err)?;
        Ok(CallToolResult::success(vec![ContentBlock::text(msg)]))
    }

    #[tool(
        description = "Triage-dispatch a task: the scheduler picks the best online node by resources"
    )]
    async fn dispatch_task(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<TaskIdParams>,
    ) -> Result<CallToolResult, McpError> {
        let caller = require_caller(&parts)?;
        to_result(
            &self
                .backend
                .dispatch_task(caller, p.task_id)
                .await
                .map_err(backend_err)?,
        )
    }

    #[tool(
        description = "Start work on a task: creates a worktree + session (runtime defaults to bash)"
    )]
    async fn start_work(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<StartWorkParams>,
    ) -> Result<CallToolResult, McpError> {
        let caller = require_caller(&parts)?;
        to_result(
            &self
                .backend
                .start_work(caller, p.task_id, p.runtime, p.node)
                .await
                .map_err(backend_err)?,
        )
    }

    #[tool(
        description = "List a repo's loop runs, newest first — builds by default. Reach for \
                       this to answer \"what is this workspace building right now\": set \
                       live_only to keep only runs still in flight, or kind=\"any\" to see \
                       review and spec runs too. Each row names its card, state, executor \
                       node and start. Use get_build_run for one run's transcript."
    )]
    async fn list_build_runs(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(q): Parameters<BuildRunQuery>,
    ) -> Result<CallToolResult, McpError> {
        let caller = require_caller(&parts)?;
        to_result(
            &self
                .backend
                .list_build_runs(caller, q)
                .await
                .map_err(backend_err)?,
        )
    }

    #[tool(
        description = "Status of ONE loop run: state, how long it has been going, its \
                       executor node, the PR if one exists, and a bounded tail of its \
                       transcript. Reach for this to answer \"how is MAIN-42's build \
                       going\" — address it by card key (its newest run) or by run id. A \
                       card nothing has built answers empty, not an error. Raise tail_lines \
                       for more transcript; the reply states what it truncated."
    )]
    async fn get_build_run(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<GetBuildRunParams>,
    ) -> Result<CallToolResult, McpError> {
        let caller = require_caller(&parts)?;
        to_result(
            &self
                .backend
                .get_build_run(caller, p.run, p.tail_lines.unwrap_or(DEFAULT_TAIL_LINES))
                .await
                .map_err(backend_err)?,
        )
    }

    // ── Tunnels (MAIN-11) ───────────────────────────────────────────────────

    #[tool(
        description = "Open a tunnel: publish a port a session is serving on at a URL on \
                       this deployment's tunnel domain, so a person can see what was just \
                       built without a shell on that machine. Name the session (an MCP \
                       caller is not inside one) \
                       and the port it listens on; the reply carries the URL, the \
                       generated name, the node and the port. The URL is NOT public — \
                       opening it needs a signed-in member of this tenant, so offer it as \
                       a link for the team, never as a share link. The tunnel ends with \
                       its session, when it goes idle, or on stop_tunnel."
    )]
    async fn open_tunnel(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<OpenTunnelParams>,
    ) -> Result<CallToolResult, McpError> {
        let caller = require_caller(&parts)?;
        to_result(
            &self
                .backend
                .open_tunnel(caller, p.session_id, p.port)
                .await
                .map_err(backend_err)?,
        )
    }

    #[tool(
        description = "List this tenant's live tunnels: each one's generated name, URL, \
                       node, port, the session it belongs to and when it was opened, plus \
                       how long it has been idle. Every URL here needs a signed-in member \
                       of this tenant to open."
    )]
    async fn list_tunnels(
        &self,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let caller = require_caller(&parts)?;
        to_result(
            &self
                .backend
                .list_tunnels(caller)
                .await
                .map_err(backend_err)?,
        )
    }

    #[tool(
        description = "Close a tunnel by its generated name, taking its URL down at once. \
                       A name with no tunnel behind it is an error, not a quiet success."
    )]
    async fn stop_tunnel(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<StopTunnelParams>,
    ) -> Result<CallToolResult, McpError> {
        let caller = require_caller(&parts)?;
        let label = p.label.trim().to_string();
        self.backend
            .stop_tunnel(caller, label.clone())
            .await
            .map_err(backend_err)?;
        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "closed {label}"
        ))]))
    }

    #[tool(description = "Move a task to a named column (Triage/Todo/In Progress/Done)")]
    async fn move_task(
        &self,
        Parameters(p): Parameters<MoveTaskParams>,
    ) -> Result<CallToolResult, McpError> {
        to_result(
            &self
                .backend
                .move_task(p.task_id, p.column)
                .await
                .map_err(backend_err)?,
        )
    }

    #[tool(
        description = "Safely replace a task's description (Markdown). Reads the current version and writes with an optimistic-concurrency guard, retrying on a concurrent edit, so it never clobbers someone else's change. Read the body first with get_task."
    )]
    async fn set_task_description(
        &self,
        Parameters(p): Parameters<SetDescriptionParams>,
    ) -> Result<CallToolResult, McpError> {
        to_result(
            &self
                .backend
                .set_task_description(p.task, p.description)
                .await
                .map_err(backend_err)?,
        )
    }

    #[tool(description = "Record a PR for a task and move it to Done")]
    async fn submit_pr(
        &self,
        Parameters(p): Parameters<SubmitPrParams>,
    ) -> Result<CallToolResult, McpError> {
        to_result(
            &self
                .backend
                .submit_pr(p.task_id, p.pr_url)
                .await
                .map_err(backend_err)?,
        )
    }

    // ── The agent loop ──────────────────────────────────────────────────────
    //
    // The descriptions carry the safety rule as well as the mechanics, so an
    // agent reading the tool list learns that `agent-ready` is a human's to
    // apply without having to be told separately — and there is deliberately
    // no tool that could apply it.

    #[tool(
        description = "Find work. One compound filter, one query. The loop's pick step is \
                       label=[\"agent-ready\"], not_label=[\"blocked\"], assignee=\"none\", \
                       is_blocked=false. Results are ordered the way work should be taken: \
                       urgent first, tasks with no priority last, then oldest first."
    )]
    async fn list_tasks(
        &self,
        Parameters(p): Parameters<TaskQuery>,
    ) -> Result<CallToolResult, McpError> {
        to_result(&self.backend.list_tasks(p).await.map_err(backend_err)?)
    }

    #[tool(
        description = "Read one whole issue by key (NOOK-42) or id: description, labels, \
                       comments, relations, and whether it is blocked. Read this before \
                       starting work — the acceptance criteria live in the description."
    )]
    async fn get_task(
        &self,
        Parameters(p): Parameters<TaskRefParams>,
    ) -> Result<CallToolResult, McpError> {
        to_result(&self.backend.get_task(p.task).await.map_err(backend_err)?)
    }

    #[tool(
        description = "List the files attached to a ticket and to its comments: filename, \
                       content type, size and the id to read or fetch each one. Metadata only \
                       — no bytes. Check this before starting work: a file named in the \
                       description is part of the contract."
    )]
    async fn list_task_attachments(
        &self,
        Parameters(p): Parameters<TaskRefParams>,
    ) -> Result<CallToolResult, McpError> {
        to_result(
            &self
                .backend
                .list_task_attachments(p.task)
                .await
                .map_err(backend_err)?,
        )
    }

    #[tool(
        description = "Read one attachment by id. A text file (markdown, JSON, CSV, source) \
                       comes back as content you can read. Anything else — an image, a PDF, \
                       an archive — comes back as a pointer telling you to fetch it with \
                       `nook attachments get <id>`; the bytes are never returned here."
    )]
    async fn read_task_attachment(
        &self,
        Parameters(p): Parameters<AttachmentRefParams>,
    ) -> Result<CallToolResult, McpError> {
        to_result(
            &self
                .backend
                .read_task_attachment(p.attachment)
                .await
                .map_err(backend_err)?,
        )
    }

    #[tool(
        description = "Claim a task, atomically, and optionally move it to a column type \
                       (usually \"started\"). Another agent may have taken it first: that \
                       returns a conflict and is NORMAL, not a failure — pick the next task \
                       and carry on."
    )]
    async fn claim_task(
        &self,
        Parameters(p): Parameters<ClaimParams>,
    ) -> Result<CallToolResult, McpError> {
        to_result(
            &self
                .backend
                .claim_task(p.task, p.column_type)
                .await
                .map_err(backend_err)?,
        )
    }

    #[tool(description = "Release a claimed task so somebody else can pick it up")]
    async fn release_task(
        &self,
        Parameters(p): Parameters<TaskRefParams>,
    ) -> Result<CallToolResult, McpError> {
        to_result(
            &self
                .backend
                .release_task(p.task)
                .await
                .map_err(backend_err)?,
        )
    }

    #[tool(
        description = "Comment on a task in markdown. This is where reasoning belongs: a \
                       blocking question, a review verdict, why an approach was abandoned. \
                       Set author_name to say which tool you are (e.g. \"loop-build on azul\")."
    )]
    async fn comment_task(
        &self,
        Parameters(p): Parameters<CommentParams>,
    ) -> Result<CallToolResult, McpError> {
        to_result(
            &self
                .backend
                .comment_task(p.task, p.body_md, p.author_name)
                .await
                .map_err(backend_err)?,
        )
    }

    #[tool(
        description = "Add a label to a task. NOTE: `agent-ready` is applied by HUMANS only \
                       — it is the approval gate that says an agent may take the work, and \
                       an agent applying it to its own task would be approving itself. Use \
                       this for labels like `blocked` or `needs-discussion`."
    )]
    async fn add_label(
        &self,
        Parameters(p): Parameters<LabelParams>,
    ) -> Result<CallToolResult, McpError> {
        // Enforced, not merely documented. A description is guidance; this is
        // the property the whole human-in-the-loop design rests on, and
        // guidance is not what you protect a safety gate with.
        if p.label.trim().eq_ignore_ascii_case("agent-ready") {
            return Err(McpError::invalid_params(
                "`agent-ready` is the human approval gate and cannot be applied by an agent. \
                 Ask a person to mark the task ready.",
                None,
            ));
        }
        to_result(
            &self
                .backend
                .add_label(p.task, p.label)
                .await
                .map_err(backend_err)?,
        )
    }

    #[tool(
        description = "Remove a label from a task. Removing `agent-ready` is allowed — \
                       handing work back is always permitted; taking it is what needs approval."
    )]
    async fn remove_label(
        &self,
        Parameters(p): Parameters<LabelParams>,
    ) -> Result<CallToolResult, McpError> {
        to_result(
            &self
                .backend
                .remove_label(p.task, p.label)
                .await
                .map_err(backend_err)?,
        )
    }

    #[tool(description = "Set a task's priority: 0 none, 1 urgent, 2 high, 3 medium, 4 low")]
    async fn set_priority(
        &self,
        Parameters(p): Parameters<PriorityParams>,
    ) -> Result<CallToolResult, McpError> {
        to_result(
            &self
                .backend
                .set_priority(p.task, p.priority)
                .await
                .map_err(backend_err)?,
        )
    }

    #[tool(
        description = "File a task under an epic, or detach it. `parent` is a uuid or key of a \
                       type=epic task on the same board; omit `parent` to detach (MAIN-81)."
    )]
    async fn set_task_parent(
        &self,
        Parameters(p): Parameters<SetTaskParentParams>,
    ) -> Result<CallToolResult, McpError> {
        to_result(
            &self
                .backend
                .set_task_parent(p.task, p.parent)
                .await
                .map_err(backend_err)?,
        )
    }

    #[tool(
        description = "Link two tasks: kind is `blocks`, `relates` or `duplicates`. A \
                       `blocks` link means `from` must reach a completed column before `to` \
                       can be picked up."
    )]
    async fn link_tasks(
        &self,
        Parameters(p): Parameters<LinkParams>,
    ) -> Result<CallToolResult, McpError> {
        to_result(
            &self
                .backend
                .link_tasks(p.from, p.to, p.kind)
                .await
                .map_err(backend_err)?,
        )
    }

    // ── Notebook tools (person-scoped; MAIN-102) ────────────────────────────
    // Each requires an OIDC-resolved person; the static MCP_TOKEN path gets the
    // clear not-a-user error from `require_person`. The person rides in the
    // request extensions, which rmcp exposes via `Extension<Parts>`.

    #[tool(
        description = "List every note in your personal notebook (title + folder path only; \
                       bodies are never returned). Use notebook_search to filter."
    )]
    async fn notebook_list_notes(
        &self,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let person = require_person(&parts)?;
        to_result(
            &self
                .backend
                .notebook_list_notes(person, None)
                .await
                .map_err(backend_err)?,
        )
    }

    #[tool(
        description = "Search your notebook by a case-insensitive substring of a note's title or \
                       its folder path. Returns title + folder path only — a note's body is never \
                       searched and never returned here, and a sealed note's body is not readable \
                       over MCP at all (the server cannot decrypt it). Read a hit with \
                       notebook_get_note."
    )]
    async fn notebook_search(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<NotebookSearchParams>,
    ) -> Result<CallToolResult, McpError> {
        let person = require_person(&parts)?;
        to_result(
            &self
                .backend
                .notebook_list_notes(person, Some(p.q))
                .await
                .map_err(backend_err)?,
        )
    }

    #[tool(
        description = "Read one of your notebook notes by id, decrypted. A sealed note returns \
                       no body (the server cannot decrypt it)."
    )]
    async fn notebook_get_note(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<NotebookNoteIdParams>,
    ) -> Result<CallToolResult, McpError> {
        let person = require_person(&parts)?;
        let id = parse_note_id(&p.id)?;
        to_result(
            &self
                .backend
                .notebook_get_note(person, id)
                .await
                .map_err(backend_err)?,
        )
    }

    #[tool(description = "Create a note in your personal notebook.")]
    async fn notebook_create_note(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<NotebookCreateParams>,
    ) -> Result<CallToolResult, McpError> {
        let person = require_person(&parts)?;
        let folder_id = match p.folder_id.as_deref() {
            Some(s) => Some(parse_folder_id(s)?),
            None => None,
        };
        let req = CreateUserNote {
            title: p.title,
            content_md: p.content_md.unwrap_or_default(),
            folder_id,
        };
        to_result(
            &self
                .backend
                .notebook_create_note(person, req)
                .await
                .map_err(backend_err)?,
        )
    }

    #[tool(
        description = "Update one of your notebook notes: change its title, body, or file it \
                       under a folder at any depth. Omit folder_id to leave the note where it \
                       is; pass an empty string to move it to the notebook root. A sealed note's \
                       body cannot be updated this way."
    )]
    async fn notebook_update_note(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<NotebookUpdateParams>,
    ) -> Result<CallToolResult, McpError> {
        let person = require_person(&parts)?;
        let id = parse_note_id(&p.id)?;
        let folder_id = parse_move_target(p.folder_id)?;
        let req = UpdateUserNote {
            title: p.title,
            content_md: p.content_md,
            folder_id,
        };
        to_result(
            &self
                .backend
                .notebook_update_note(person, id, req)
                .await
                .map_err(backend_err)?,
        )
    }

    #[tool(description = "Delete one of your notebook notes by id.")]
    async fn notebook_delete_note(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<NotebookNoteIdParams>,
    ) -> Result<CallToolResult, McpError> {
        let person = require_person(&parts)?;
        let id = parse_note_id(&p.id)?;
        self.backend
            .notebook_delete_note(person, id)
            .await
            .map_err(backend_err)?;
        Ok(CallToolResult::success(vec![ContentBlock::text("deleted")]))
    }

    #[tool(description = "List your notebook folders.")]
    async fn notebook_list_folders(
        &self,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let person = require_person(&parts)?;
        to_result(
            &self
                .backend
                .notebook_list_folders(person)
                .await
                .map_err(backend_err)?,
        )
    }

    #[tool(
        description = "Create a folder in your personal notebook. Give parent_id to nest it \
                       inside another folder, to any depth; omit it for a folder at the root."
    )]
    async fn notebook_create_folder(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<NotebookCreateFolderParams>,
    ) -> Result<CallToolResult, McpError> {
        let person = require_person(&parts)?;
        let parent_id = parse_parent(p.parent_id)?;
        to_result(
            &self
                .backend
                .notebook_create_folder(
                    person,
                    CreateUserNoteFolder {
                        name: p.name,
                        parent_id,
                    },
                )
                .await
                .map_err(backend_err)?,
        )
    }

    #[tool(
        description = "Rename a notebook folder. Its contents and its place in the tree are \
                       untouched — use notebook_move_folder to move it."
    )]
    async fn notebook_rename_folder(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<NotebookRenameFolderParams>,
    ) -> Result<CallToolResult, McpError> {
        let person = require_person(&parts)?;
        let id = parse_folder_id(p.id.trim())?;
        to_result(
            &self
                .backend
                .notebook_update_folder(
                    person,
                    id,
                    UpdateUserNoteFolder {
                        name: Some(p.name),
                        // `None` is "leave the parent alone", which is the whole
                        // difference between this tool and the next one.
                        parent_id: None,
                    },
                )
                .await
                .map_err(backend_err)?,
        )
    }

    #[tool(
        description = "Move a notebook folder under another folder, at any depth, taking its \
                       notes and sub-folders with it. Omit parent_id to move it to the notebook \
                       root. Moving a folder inside its own subtree is refused."
    )]
    async fn notebook_move_folder(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<NotebookMoveFolderParams>,
    ) -> Result<CallToolResult, McpError> {
        let person = require_person(&parts)?;
        let id = parse_folder_id(p.id.trim())?;
        let parent_id = parse_parent(p.parent_id)?;
        to_result(
            &self
                .backend
                .notebook_update_folder(
                    person,
                    id,
                    UpdateUserNoteFolder {
                        name: None,
                        // Always `Some`: a move tool always sets the parent, and
                        // `Some(None)` is the root.
                        parent_id: Some(parent_id),
                    },
                )
                .await
                .map_err(backend_err)?,
        )
    }

    #[tool(
        description = "Delete a notebook folder. Its notes and any folders inside it move up one level — nothing is deleted with it."
    )]
    async fn notebook_delete_folder(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<NotebookFolderIdParams>,
    ) -> Result<CallToolResult, McpError> {
        let person = require_person(&parts)?;
        let id = parse_folder_id(p.id.trim())?;
        self.backend
            .notebook_delete_folder(person, id)
            .await
            .map_err(backend_err)?;
        Ok(CallToolResult::success(vec![ContentBlock::text("deleted")]))
    }
}

#[tool_handler]
impl ServerHandler for NookMcp {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.instructions = Some(
            "NookOS: the workspace operating system. Observe nodes, workspaces, \
             sessions and activity; start sessions and inject tasks. NookOS \
             coordinates — humans approve."
                .into(),
        );
        info
    }
}

/// The `/mcp` streamable-HTTP service as an axum router.
/// The transport config with OUR host allowlist in place of rmcp's
/// loopback-only default (MAIN-190), served STATELESSLY (MAIN-524). Split out
/// so both are testable: everything else stays at the crate's defaults.
///
/// Stateless is deliberate. rmcp's default demands a session ticket: it issues
/// `Mcp-Session-Id` on `initialize` and, for any later POST arriving without
/// it, answers 422 instead of serving the request — which is every `tools/list`
/// ChatGPT's connector sent us. All our tools are request/response, so the one
/// thing statefulness buys (server→client push) is unused, while it costs
/// sessions held in one process's RAM: every deploy severs every client, and a
/// second replica would need sticky routing.
fn transport_config(
    allowed_hosts: Vec<String>,
) -> rmcp::transport::streamable_http_server::StreamableHttpServerConfig {
    rmcp::transport::streamable_http_server::StreamableHttpServerConfig::default()
        .with_allowed_hosts(allowed_hosts)
        .with_stateful_mode(false)
}

/// `allowed_hosts`: every Host header this service answers for — the caller
/// derives them from its own config (public hostname, loopbacks, in-cluster
/// service name). Port-less entries match any port. rmcp's DNS-rebinding
/// protection 403s everything else, so an empty-feeling list here is an outage
/// for every remote client, not a hardening win (MAIN-190).
pub fn router(backend: Arc<dyn NookBackend>, allowed_hosts: Vec<String>) -> axum::Router {
    let service = StreamableHttpService::new(
        move || Ok(NookMcp::new(backend.clone())),
        LocalSessionManager::default().into(),
        transport_config(allowed_hosts),
    );
    axum::Router::new().fallback_service(service)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// MAIN-190 AC-4: the given hosts land in the transport config verbatim —
    /// and non-empty, because an empty list means allow-all in rmcp and a
    /// defaulted one means loopback-only; both ends of that spectrum were the
    /// bug and the anti-fix respectively.
    #[test]
    fn transport_config_threads_the_allowed_hosts() {
        let hosts = vec!["nook.example.test".to_string(), "localhost".to_string()];
        let cfg = transport_config(hosts.clone());
        assert_eq!(cfg.allowed_hosts, hosts);
        assert!(
            !cfg.stateful_mode,
            "MAIN-524: stateless on purpose — a stateful transport 422s every \
             request that carries no Mcp-Session-Id, which is what stopped the \
             ChatGPT connector listing our tools. A refactor back to \
             ::default() re-breaks it silently."
        );
    }

    fn parts_with(caller: Option<McpCaller>) -> Parts {
        let mut parts = axum::http::Request::builder()
            .body(())
            .unwrap()
            .into_parts()
            .0;
        if let Some(c) = caller {
            parts.extensions.insert(c);
        }
        parts
    }

    #[test]
    fn require_person_yields_the_caller_when_resolved() {
        let person = Uuid::now_v7();
        let caller = McpCaller {
            person_id: person,
            user_id: UserId::new(),
            tenant_id: TenantId::new(),
        };
        assert_eq!(require_person(&parts_with(Some(caller))).unwrap(), person);
    }

    #[test]
    fn require_person_refuses_with_a_pointed_error_when_unauthenticated() {
        // No McpCaller in extensions — the MCP_TOKEN path. Every notebook tool
        // takes this branch and must refuse (AC-3).
        let err = require_person(&parts_with(None)).unwrap_err();
        assert!(
            err.message.contains("not authenticated as a user"),
            "pointed not-a-user error: {}",
            err.message
        );
    }

    /// MAIN-210 AC-1/AC-2: the two ways a flat string carries a folder target,
    /// and why they differ. An update leaves the parent alone when the field is
    /// absent, so "move to the root" needs the empty string; a create or a
    /// dedicated move has no "leave alone" case, so absent already means root.
    #[test]
    fn a_folder_target_says_leave_alone_root_or_here() {
        let id = UserNoteFolderId::new();
        assert!(matches!(parse_move_target(None), Ok(None)));
        assert!(matches!(
            parse_move_target(Some("  ".into())),
            Ok(Some(None))
        ));
        assert_eq!(
            parse_move_target(Some(format!(" {id} "))).unwrap(),
            Some(Some(id)),
            "an id is trimmed, not rejected"
        );
        assert!(parse_move_target(Some("not-a-uuid".into())).is_err());

        assert!(matches!(parse_parent(None), Ok(None)));
        assert!(matches!(parse_parent(Some("".into())), Ok(None)));
        assert_eq!(parse_parent(Some(id.to_string())).unwrap(), Some(id));
        assert!(parse_parent(Some("not-a-uuid".into())).is_err());
    }

    /// MAIN-210 AC-6: EVERY notebook tool is person-scoped, so every one of them
    /// must take the caller's identity and refuse a static-token call. Asserted
    /// against the source text rather than one tool at a time — the risk is a
    /// tool added later that forgets, and a per-tool test only covers the tools
    /// somebody remembered to write one for.
    #[test]
    fn every_notebook_tool_requires_a_person() {
        let src = include_str!("lib.rs");
        // The tool impls only: the trait declares methods of the same names
        // further up, and they take an already-resolved person.
        let section = src
            .split_once("#[tool_router]")
            .expect("the tool impl block")
            .1
            .split_once("#[tool_handler]")
            .expect("the end of the tool impl block")
            .0;
        let tools: Vec<&str> = section.split("async fn notebook_").skip(1).collect();
        assert_eq!(
            tools.len(),
            11,
            "the notebook tool surface changed — check the new tool too"
        );
        for body in tools {
            let name: String = body.chars().take_while(|c| *c != '(').collect();
            let head = &body[..body.find(".backend").unwrap_or(body.len())];
            assert!(
                head.contains("require_person(&parts)?"),
                "notebook_{name} does not resolve a person — a static-token \
                 caller would reach somebody's notebook"
            );
        }
    }

    #[test]
    fn require_caller_yields_the_whole_identity_or_refuses_the_static_token() {
        // MAIN-223 AC-4: dispatch/start-work need the full caller (user + tenant),
        // not just the person — so they authorize exactly as the HTTP routes do.
        let caller = McpCaller {
            person_id: Uuid::now_v7(),
            user_id: UserId::new(),
            tenant_id: TenantId::new(),
        };
        let got = require_caller(&parts_with(Some(caller.clone()))).unwrap();
        assert_eq!(got.user_id, caller.user_id);
        assert_eq!(got.tenant_id, caller.tenant_id);

        // The static MCP_TOKEN path carries no caller: the work tools refuse it
        // with the same pointed error, never a silent dead end.
        let err = require_caller(&parts_with(None)).unwrap_err();
        assert!(
            err.message.contains("not authenticated as a user"),
            "pointed not-a-user error: {}",
            err.message
        );
    }
}

/// The backend-error mapping (MAIN-275).
///
/// These are about what an MCP CALLER is told. The old behaviour flattened
/// everything to `internal_error(e.to_string())`, so an agent could not tell a
/// refusal from a missing row, and every internal failure handed over whatever
/// text it happened to carry.
#[cfg(test)]
mod backend_err_tests {
    use super::*;

    /// The backend returns `anyhow::Result`; services under it return
    /// `ApiResult`. This is that shape — an `ApiError` reached by `?`, so it
    /// sits inside the anyhow chain rather than being the top-level error.
    fn through_backend(e: ApiError) -> McpError {
        fn inner(e: ApiError) -> anyhow::Result<()> {
            Err(e)?;
            Ok(())
        }
        backend_err(inner(e).unwrap_err())
    }

    /// AC-1: a refusal is a refusal, not a generic internal error. This is the
    /// distinction an agent needs to stop retrying.
    #[test]
    fn a_refusal_is_an_invalid_request_not_an_internal_error() {
        for e in [
            ApiError::Unauthorized,
            ApiError::Forbidden,
            ApiError::ForbiddenMsg("a node token cannot do this".into()),
        ] {
            let m = through_backend(e);
            assert_eq!(m.code, McpError::invalid_request("", None).code, "{m:?}");
            assert_ne!(
                m.code,
                McpError::internal_error("", None).code,
                "a refusal must not arrive as an internal error"
            );
        }
    }

    /// AC-1: "that does not exist" has its own kind, so an agent can retry with
    /// a different name instead of giving up.
    #[test]
    fn a_missing_row_is_resource_not_found() {
        let m = through_backend(ApiError::NotFound);
        assert_eq!(m.code, McpError::resource_not_found("", None).code, "{m:?}");
    }

    /// AC-1: what the caller sent is the caller's to fix.
    #[test]
    fn bad_input_and_conflicts_are_invalid_params() {
        for e in [
            ApiError::BadRequest("a task needs a title".into()),
            ApiError::Conflict("that name is taken".into()),
        ] {
            let m = through_backend(e);
            assert_eq!(m.code, McpError::invalid_params("", None).code, "{m:?}");
        }
    }

    /// AC-1: a 4xx keeps its message — that is the whole point of not
    /// flattening. An agent acts on "a task needs a title".
    #[test]
    fn a_client_error_keeps_its_message() {
        let m = through_backend(ApiError::BadRequest("a task needs a title".into()));
        assert_eq!(m.message, "a task needs a title");
        let m = through_backend(ApiError::ForbiddenMsg("a node token cannot do this".into()));
        assert_eq!(m.message, "a node token cannot do this");
    }

    /// AC-2: the leak. `Internal` and `Db` must say nothing — asserted against
    /// the secret itself, so a future change that "helpfully" includes the
    /// cause fails here.
    #[test]
    fn an_internal_error_leaks_nothing_to_the_caller() {
        let secret = "connection refused to 10.0.0.7:5432 as user nook_admin";
        let m = through_backend(ApiError::Internal(anyhow::anyhow!("{secret}")));
        assert_eq!(m.code, McpError::internal_error("", None).code);
        assert_eq!(m.message, GENERIC);
        assert!(
            !m.message.contains("10.0.0.7"),
            "leaked a host: {}",
            m.message
        );
        assert!(
            !m.message.contains("nook_admin"),
            "leaked a user: {}",
            m.message
        );
        assert!(
            !m.message.contains("connection refused"),
            "leaked the cause: {}",
            m.message
        );
    }

    /// AC-2: and the same for a failure that is not an `ApiError` at all — a
    /// plumbing error with no HTTP meaning still must not hand its text over.
    #[test]
    fn a_non_api_failure_also_leaks_nothing() {
        let m = backend_err(anyhow::anyhow!("password=hunter2 in the connection string"));
        assert_eq!(m.code, McpError::internal_error("", None).code);
        assert_eq!(m.message, GENERIC);
        assert!(!m.message.contains("hunter2"), "leaked: {}", m.message);
    }

    /// The generic message is REST's, byte for byte. Two surfaces agreeing by
    /// accident is two surfaces that will disagree later.
    #[test]
    fn the_generic_message_matches_rest() {
        // What `ApiError::Internal` renders in the REST body.
        assert_eq!(GENERIC, "internal error");
    }
}
