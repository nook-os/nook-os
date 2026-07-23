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
    /// What happened when this node tried to write a taught skill.
    ///
    /// Reported rather than assumed: "the control plane sent it" and "every
    /// agent on that machine can read it" are different claims, and only the
    /// node can make the second one. A machine with no agents installed is a
    /// success with an empty `agents` list, not a failure — but an operator
    /// should be able to see the difference.
    SkillInstalled {
        name: String,
        /// Agent names written to, e.g. `["Hermes", "Claude Code"]`.
        agents: Vec<String>,
        /// Absolute paths written, for an operator who wants to go and look.
        paths: Vec<String>,
        /// Present only on failure; the node keeps running either way.
        #[serde(default)]
        error: Option<String>,
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
        /// Whether the checkout is a git repository at all. Defaults to `true`
        /// so a node built before this field keeps its old behaviour — an
        /// absent answer means "unknown", and hiding the git panel on a real
        /// repository is a worse failure than showing an empty one.
        #[serde(default = "crate::yes")]
        is_repo: bool,
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
        /// The agent version this control plane expects. A node that differs
        /// knows it is behind without having to ask anything else.
        #[serde(default)]
        expected_agent_version: Option<String>,
        /// Every certificate authority this tenant currently trusts.
        ///
        /// The node compares these against the bundle it holds. Anything here
        /// that it does not have means a rotation is being staged, and it
        /// renews immediately rather than waiting up to thirty days for its
        /// certificate to expire — which is what lets an operator promote the
        /// new CA soon after staging instead of hoping the fleet caught up.
        #[serde(default)]
        ca_fingerprints: Vec<String>,
    },
    /// Replace your binary and restart.
    ///
    /// Only ever obeyed by a node whose process will be restarted for it —
    /// under a service manager. Told to update, a node run by hand would
    /// replace its binary and exit, which is a fleet that goes dark on the
    /// operator who least expected it.
    UpdateAgent,
    /// Write this skill into every agent installed on the machine.
    ///
    /// Content travels with the instruction rather than a URL to fetch: a node
    /// already has an authenticated channel to the control plane, and skills
    /// are documents measured in kilobytes. Making the node go and get it
    /// would add a second thing that can fail, on a different port, needing
    /// its own credential.
    InstallSkill {
        name: String,
        content: String,
        /// Of the content. The node skips the write when what is on disk
        /// already matches, so reconnect-driven convergence is free rather
        /// than rewriting every skill on every machine on every reconnect.
        sha256: String,
    },
    /// The tenant's trust bundle changed — usually a CA was staged.
    ///
    /// Pushed rather than polled, so a node that has been connected for a week
    /// reacts in seconds. `RegisterAck` carries the same list for the connect
    /// case; this is the same news arriving mid-connection.
    TrustChanged {
        ca_fingerprints: Vec<String>,
    },
    /// Remove a skill that was taught. Only ever removes `<skills>/<name>/`
    /// directories this fleet wrote; a hand-installed skill of the same name
    /// is somebody's own work and is left alone.
    ForgetSkill {
        name: String,
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
    /// Something a person should see, right now.
    ///
    /// Carries the whole notification rather than an id, unlike `TaskChanged`.
    /// The distinction is what the client does with it: a task id says "refetch
    /// that", and the client already knows how. A notification has no canonical
    /// place to be refetched from — it IS the message — and a toast that had to
    /// round-trip before it could be shown would arrive after the moment it was
    /// about.
    Notification {
        notification: serde_json::Value,
    },
    /// A task changed — moved, relabelled, commented on, claimed.
    ///
    /// Carries only the id, not the task. Agents and other browsers change
    /// tasks constantly, and a payload would be a second copy of state that
    /// arrives out of order with the fetch the viewer is already doing. The id
    /// says "what you have for this one is stale"; the client refetches what it
    /// actually needs, which for a board is one card and for an open detail
    /// panel is the whole issue.
    TaskChanged {
        task_id: nook_types::TaskId,
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

/// Is this a name a skill may be taught under?
///
/// It lives here, in the crate that defines the message carrying it, because
/// both ends have to enforce it and two implementations that "must agree" is a
/// bug with a delay on it: a name the control plane accepts and the node
/// refuses is a skill that reports as taught and exists on no machine.
///
/// An allow-list, not a search for bad characters. The name becomes a path
/// component (`<skills>/<name>/SKILL.md`) on every machine in the fleet, so the
/// question is "what may it be", not "what must it not be" — `..` and `/` are
/// the ones that matter, and an allow-list rules out the ones nobody thought of.
pub fn valid_skill_name(name: &str) -> Result<&str, String> {
    let n = name.trim();
    if n.is_empty() {
        return Err("a skill needs a name".into());
    }
    if n.len() > 64 {
        return Err(format!("a skill name may be at most 64 characters: {n:?}"));
    }
    if !n
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "skill names may only contain letters, digits, '-' and '_' — got {n:?}"
        ));
    }
    Ok(n)
}

/// A skill's name as the document itself declares it.
///
/// Skills carry YAML frontmatter with a `name:`, and that is the name the agent
/// knows it by — so `nook teach ./SKILL.md` teaches the skill the file says it
/// is, rather than a fleet-wide skill called "skill".
pub fn skill_name_from_frontmatter(content: &str) -> Option<String> {
    let rest = content.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    rest[..end].lines().find_map(|line| {
        let v = line.strip_prefix("name:")?.trim().trim_matches(['"', '\'']);
        (!v.trim().is_empty()).then(|| v.trim().to_string())
    })
}

/// serde default for booleans that mean "assume yes when an older peer omits
/// the field".
pub(crate) fn yes() -> bool {
    true
}

#[cfg(test)]
mod skill_name_tests {
    use super::*;

    /// Each of these, accepted, writes a directory somewhere nobody asked for
    /// — on every machine in the fleet at once.
    #[test]
    fn a_name_that_would_escape_the_skills_directory_is_refused() {
        for bad in [
            "..",
            ".",
            "../../etc",
            "a/b",
            "a\\b",
            "/etc/passwd",
            "",
            "   ",
            "has space",
            "semi;colon",
            "dot.dot",
            "tilde~",
        ] {
            assert!(valid_skill_name(bad).is_err(), "must refuse {bad:?}");
        }
        for ok in ["nookos", "code-review", "my_skill_2", "A1"] {
            assert_eq!(valid_skill_name(ok).unwrap(), ok, "must accept {ok:?}");
        }
        // Trimmed rather than refused: a trailing newline off a shell pipeline
        // is a typo, and rejecting it teaches nobody anything.
        assert_eq!(valid_skill_name("  tidy\n").unwrap(), "tidy");
        assert!(valid_skill_name(&"x".repeat(65)).is_err());
    }

    #[test]
    fn the_name_is_read_from_frontmatter_only() {
        let doc = "---\nname: code-review\ndescription: x\n---\n\n# Body\n";
        assert_eq!(
            skill_name_from_frontmatter(doc).as_deref(),
            Some("code-review")
        );
        assert_eq!(
            skill_name_from_frontmatter("---\nname: \"quoted\"\n---\n").as_deref(),
            Some("quoted")
        );
        assert_eq!(skill_name_from_frontmatter("# no frontmatter\n"), None);
        assert_eq!(skill_name_from_frontmatter("---\nname:\n---\n"), None);
        // A `name:` in the body is prose, not a declaration.
        assert_eq!(skill_name_from_frontmatter("# t\n\nname: nope\n"), None);
    }
}
