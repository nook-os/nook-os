//! NookOS MCP server: control NookOS from any MCP client.
//!
//! Tools delegate to a [`NookBackend`] implemented by the control plane's
//! service layer, so REST and MCP can never drift apart. The dependency
//! direction is control-plane → this crate; the trait keeps it acyclic.

use std::sync::Arc;

use async_trait::async_trait;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo};
use rmcp::schemars::{self, JsonSchema};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::StreamableHttpService;
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler};
use serde::Deserialize;

use nook_types::{Event, Node, Note, Session, TaskItem, WorkspaceDetail};

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
    async fn create_task(
        &self,
        title: String,
        description: Option<String>,
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
    /// Triage-dispatch a task: the scheduler picks the best online node.
    async fn dispatch_task(&self, task_id: String) -> anyhow::Result<TaskItem>;
    /// Start work on a task: worktree + session. `runtime` defaults to bash.
    async fn start_work(
        &self,
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
    /// Find work: the compound pick filter, as one query.
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
    async fn link_tasks(
        &self,
        from: String,
        to: String,
        kind: String,
    ) -> anyhow::Result<serde_json::Value>;
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

#[derive(Clone)]
pub struct NookMcp {
    backend: Arc<dyn NookBackend>,
}

fn to_result<T: serde::Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    let json = serde_json::to_string_pretty(value)
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
    Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
}

fn backend_err(e: anyhow::Error) -> McpError {
    McpError::internal_error(e.to_string(), None)
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
                .create_task(p.title, p.description)
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
        Parameters(p): Parameters<TaskIdParams>,
    ) -> Result<CallToolResult, McpError> {
        to_result(
            &self
                .backend
                .dispatch_task(p.task_id)
                .await
                .map_err(backend_err)?,
        )
    }

    #[tool(
        description = "Start work on a task: creates a worktree + session (runtime defaults to bash)"
    )]
    async fn start_work(
        &self,
        Parameters(p): Parameters<StartWorkParams>,
    ) -> Result<CallToolResult, McpError> {
        to_result(
            &self
                .backend
                .start_work(p.task_id, p.runtime, p.node)
                .await
                .map_err(backend_err)?,
        )
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
pub fn router(backend: Arc<dyn NookBackend>) -> axum::Router {
    let service = StreamableHttpService::new(
        move || Ok(NookMcp::new(backend.clone())),
        LocalSessionManager::default().into(),
        Default::default(),
    );
    axum::Router::new().fallback_service(service)
}
