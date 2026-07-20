//! The node ↔ control-plane WebSocket protocol.
//!
//! One persistent outbound connection per node (no inbound SSH, no public
//! ports). JSON text frames; terminal bytes ride base64-encoded inside
//! `SessionOutput`/`SessionInput` (simple and debuggable — binary framing is
//! a future optimization). All enums are adjacently tagged for clean
//! generated TypeScript.

use nook_types::{Capabilities, NodeId, SessionId};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// A git repository found under a node's workspace roots. Repositories are
/// self-describing; the node reports, the control plane reconciles.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct DiscoveredWorkspace {
    pub path: String,
    pub name: String,
    pub git_remote_url: Option<String>,
    pub branch: Option<String>,
    pub dirty: bool,
    /// True when this checkout is a linked git worktree (its `.git` is a file
    /// pointing at the primary repo, not a directory).
    #[serde(default)]
    pub worktree: bool,
}

/// Messages the node sends to the control plane.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum NodeToControl {
    /// Idempotent full resync: sent on every (re)connect.
    Register {
        capabilities: Capabilities,
        /// tmux sessions (names) that are still alive on this node, so the
        /// control plane can reconcile session state after restarts.
        live_tmux_sessions: Vec<String>,
    },
    Heartbeat {
        load: serde_json::Value,
    },
    WorkspacesDiscovered {
        workspaces: Vec<DiscoveredWorkspace>,
    },
    SessionStarted {
        session_id: SessionId,
        tmux_session: String,
    },
    SessionOutput {
        session_id: SessionId,
        data_b64: String,
    },
    SessionExited {
        session_id: SessionId,
        exit_code: Option<i32>,
    },
    /// A session could not be started at all — the checkout is gone, the
    /// runtime isn't installed, tmux refused. Distinct from `Error` because it
    /// names the session, so the control plane can fail that row instead of
    /// leaving it "starting" forever with the reason buried in a log.
    SessionFailed {
        session_id: SessionId,
        message: String,
    },
    Error {
        context: String,
        message: String,
    },
    /// Response to `GetGitStatus` (request/response over the same socket).
    GitStatusResult {
        request_id: uuid::Uuid,
        branch: Option<String>,
        files: Vec<nook_types::GitFileStatus>,
        diff: String,
    },
    /// Generic completion for long-running git operations (clone, worktree).
    OpResult {
        request_id: uuid::Uuid,
        ok: bool,
        path: Option<String>,
        message: String,
    },
    Pong,
}

/// What to do with a session's terminals (tmux windows/panes).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum WindowAction {
    /// Just report the current terminals.
    List,
    /// Open another terminal in the session and focus it.
    New {
        cwd: Option<String>,
    },
    /// Split the visible terminal so two are on screen at once.
    Split {
        vertical: bool,
    },
    Select {
        index: u32,
    },
    Close {
        index: u32,
    },
    Rename {
        index: u32,
        name: String,
    },
}

/// Messages the control plane sends to the node.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ControlToNode {
    RegisterAck {
        node_id: NodeId,
        node_name: String,
    },
    StartSession {
        session_id: SessionId,
        runtime: String,
        workspace_path: String,
        cols: u16,
        rows: u16,
    },
    AttachSession {
        session_id: SessionId,
        /// The tmux session name (from the control plane's records) so a
        /// restarted node can re-establish its PTY before replaying.
        tmux_session: Option<String>,
    },
    SessionInput {
        session_id: SessionId,
        data_b64: String,
    },
    ResizeSession {
        session_id: SessionId,
        cols: u16,
        rows: u16,
    },
    KillSession {
        session_id: SessionId,
    },
    /// Last viewer left: stop forwarding this session's output frames (the
    /// node keeps reading the PTY so exit detection stays live). AttachSession
    /// resumes the stream.
    DetachSession {
        session_id: SessionId,
    },
    RescanWorkspaces,
    /// Ask for branch + porcelain status + working-tree diff of a checkout.
    GetGitStatus {
        request_id: uuid::Uuid,
        workspace_path: String,
    },
    /// Clone a repository into the node's first workspace root. If `ssh_key`
    /// is set (a tenant credential decrypted by the control plane), the node
    /// uses it via a 0600 temp file and deletes it afterwards — never stored.
    CloneRepo {
        request_id: uuid::Uuid,
        url: String,
        dest_name: Option<String>,
        ssh_key: Option<String>,
    },
    /// Add a git worktree next to an existing checkout: the same workspace
    /// gains another location (branch) on this node.
    AddWorktree {
        request_id: uuid::Uuid,
        repo_path: String,
        branch: String,
    },
    /// Remove a git worktree checkout (the "done → prune" step).
    RemoveWorktree {
        request_id: uuid::Uuid,
        worktree_path: String,
    },
    /// Stage everything in a checkout and commit it.
    GitCommit {
        request_id: uuid::Uuid,
        checkout_path: String,
        message: String,
    },
    /// Push the checkout's current branch, setting upstream on first push.
    /// Carries the tenant credential (when there is one) for the same reason
    /// clone does: the key never lives on the node's disk permanently.
    GitPush {
        request_id: uuid::Uuid,
        checkout_path: String,
        ssh_key_material: Option<String>,
    },
    /// Delete a checkout directory outright — primary clone or worktree —
    /// when a workspace is deleted with "also remove the files".
    RemoveCheckout {
        request_id: uuid::Uuid,
        path: String,
    },
    /// Manage the terminals *inside* a session. One tmux session holds many
    /// windows (and each window many panes), so this is how a session gets
    /// more than one terminal. Replies via `OpResult` with the window list as
    /// JSON in `message`.
    SessionWindows {
        request_id: uuid::Uuid,
        tmux_session: String,
        action: WindowAction,
    },
    /// Create a brand-new empty git project under the node's workspace root.
    InitProject {
        request_id: uuid::Uuid,
        name: String,
    },
    /// Read a session's terminal screen (plus history tail) as plain text —
    /// the observe half of programmatic session control. Replied via
    /// `OpResult` with the captured text in `message`.
    CaptureSession {
        request_id: uuid::Uuid,
        tmux_session: String,
        /// How many history lines above the visible screen to include.
        history_lines: u32,
    },
    /// Write a file (e.g. a synced .env) into a checkout, mode 0600.
    WriteWorkspaceFile {
        checkout_path: String,
        name: String,
        content_b64: String,
    },
    /// Read a file back out of a checkout — how an imported repo's existing
    /// `.env` gets adopted into the vault. Replies via `OpResult` with the
    /// content base64-encoded in `message`; `ok: false` when there's no such
    /// file, which is the common and uninteresting case.
    ReadWorkspaceFile {
        request_id: uuid::Uuid,
        checkout_path: String,
        name: String,
    },
    Ping,
}

/// Live events pushed to browsers over `/api/v1/ws/ui`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum UiEvent {
    NodeStatus {
        node_id: NodeId,
        name: String,
        status: String,
    },
    SessionStatus {
        session_id: SessionId,
        status: String,
    },
    NodeResources {
        node_id: NodeId,
        resources: serde_json::Value,
    },
    Activity {
        event: nook_types::Event,
    },
}

/// Terminal attach socket messages (browser → control plane).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum AttachClientMessage {
    Input { data_b64: String },
    Resize { cols: u16, rows: u16 },
}

/// Terminal attach socket messages (control plane → browser).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum AttachServerMessage {
    Output {
        data_b64: String,
    },
    Status {
        status: String,
    },
    /// The agreed terminal grid: the PTY is sized to the LARGEST current
    /// viewer; every viewer renders this grid (scaling its font down if its
    /// panel is smaller), so a small window never shrinks the session for
    /// everyone else.
    Size {
        cols: u16,
        rows: u16,
    },
}
