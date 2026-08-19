//! The node ↔ control-plane WebSocket protocol.
//!
//! One persistent outbound connection per node (no inbound SSH, no public
//! ports). JSON text frames; terminal bytes ride base64-encoded inside
//! `SessionOutput`/`SessionInput` (simple and debuggable — binary framing is
//! a future optimization). All enums are adjacently tagged for clean
//! generated TypeScript.

use nook_types::{AuthProfile, Capabilities, NodeId, SessionId, TenantId, WorkspaceId};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// What a session's tmux name starts with: `nook_<session-uuid-simple>`.
///
/// Shared rather than owned by the node because BOTH ends now need it — the
/// node to build the name, the control plane to read a session id back out of
/// one a node reported. Two copies of a naming rule is exactly the drift that
/// makes a sweep quietly stop matching anything (MAIN-363).
pub const TMUX_SESSION_PREFIX: &str = "nook_";

/// The canonical NookOS hook set, shared by the node (which installs it) and the
/// control plane (which stores it as managed content). See [`hooks`].
pub mod hooks;

/// Which docker compose project a build worktree's stack runs under, shared by
/// the side that decides what may be reaped and the side that reaps it.
pub mod compose;

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
    /// The last segment of the scan root this checkout was found under —
    /// `engineering-team` for `~/.nook/workspace/engineering-team/acme/api`.
    ///
    /// Cross-tenant placement (MAIN-353) lets tenant B's reconciler clone onto a
    /// node homed in tenant A, and MAIN-363 put those files in B's own folder.
    /// But the report carried only the path, which is an opaque string, so the
    /// control plane attributed everything to A — the node's tenant — and minted
    /// a duplicate workspace there, stealing the real one's checkout. Getting the
    /// FOLDER right never helped, because nothing read the folder.
    ///
    /// The node has always known this: it is the root it was told to clone into.
    /// This is that knowledge said out loud, so attribution stops being a guess
    /// that races the checkout row's INSERT.
    ///
    /// `None` for a checkout under a root that is not tenant-scoped — the flat
    /// pre-MAIN-347 `~/.nook/workspace`, or a control-plane-slug root. Those keep
    /// exactly today's behaviour (attributed to the node's own tenant), which is
    /// why a legacy tree that cannot be moved does not have to be.
    #[serde(default)]
    pub root_segment: Option<String>,
}

/// Messages the node sends to the control plane.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum NodeToControl {
    /// Idempotent full resync: sent on every (re)connect.
    Register {
        /// Boxed, and it matters for the channel rather than for this message:
        /// `Capabilities` is by far the largest thing on this enum, so inline
        /// it set the size of EVERY variant — including `SessionOutput`, which
        /// is sent thousands of times a second while a terminal is busy. One
        /// allocation on the rarest message keeps the common ones small.
        /// Serialises identically to the unboxed field, so the wire is
        /// unchanged.
        capabilities: Box<Capabilities>,
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
    /// What happened when this node tried to merge the managed hook set into
    /// `settings.json`. Mirrors `SkillInstalled`'s contract: failure is
    /// reported, never fatal — a bad merge (unreadable/invalid file) is a
    /// recorded error and the node keeps running (MAIN-105).
    HooksInstalled {
        /// The settings file written (or that the merge targeted), for an
        /// operator who wants to go and look.
        path: String,
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
    /// A line of a CHAT session's conversation (MAIN-502), appended to the
    /// session's messages verbatim.
    ///
    /// `JobTranscript`'s counterpart for a session rather than a run, and the
    /// same contract: recorded, never interpreted. `role` is `agent` for what
    /// the runtime said and `system` for the node's own lifecycle notes — a
    /// human turn is recorded by the control plane when it accepts it, never
    /// on the echo, for the reason `drive_streaming` spells out.
    ChatMessage {
        session_id: SessionId,
        role: String,
        body: String,
    },
    /// A chat session's agent is blocked on a permission (MAIN-502).
    ///
    /// The agent stays blocked until a `ChatPermissionDecision` comes back —
    /// that is the runtime's own contract for this exchange, not a timeout we
    /// impose. A node that dies holding one leaves an answered-never request,
    /// which the session's next start supersedes.
    ChatPermission {
        session_id: SessionId,
        /// The runtime's id for the request, to address the answer to.
        request_id: String,
        /// The tool being asked about — `Bash`, `Write`, …
        tool_name: String,
        /// The runtime's own one-line summary of what it wants to do, when it
        /// offers one (a command line, a file path). Empty when it does not.
        #[serde(default)]
        description: String,
    },
    /// Freshly re-probed runtime authorization (MAIN-126). The node re-runs its
    /// probes when an authorize login flow ends and pushes the new set, so the
    /// Nodes UI flips a profile to `authorized` right away rather than waiting
    /// for the next reconnect's `Register`.
    RuntimeAuthStatus {
        profiles: Vec<AuthProfile>,
    },
    /// The outcome of an [`ControlToNode::InstallRuntimeCredential`] (MAIN-283).
    ///
    /// Reported whether it worked or not, and never fatal — a machine whose
    /// credential directory is unwritable should keep running sessions, and the
    /// operator needs to be told which runtime it was rather than have the node
    /// disappear. Mirrors `SkillInstalled`: `error` present means it failed.
    ///
    /// A successful write whose re-probe still says "not authorized" is a
    /// FAILURE here — the payload landed but the runtime did not accept it,
    /// which is exactly the case an operator must not be told is fine.
    RuntimeCredentialInstalled {
        runtime: String,
        /// Where it landed, for an operator who wants to go and look. Empty on
        /// failure.
        path: String,
        #[serde(default)]
        error: Option<String>,
    },
    /// A session could not be started at all — the checkout is gone, the
    /// runtime isn't installed, tmux refused. Distinct from `Error` because it
    /// names the session, so the control plane can fail that row instead of
    /// leaving it "starting" forever with the reason buried in a log.
    SessionFailed {
        session_id: SessionId,
        message: String,
    },
    /// The node could not take one or more of the ports it was leased, so it did
    /// NOT start the session (MAIN-301 follow-on).
    ///
    /// The authoritative check. A range promises nothing else is listening in
    /// it and an exclusion list records what an operator already knows, but
    /// only `bind()` can answer for certain, and only at the moment of use —
    /// so this is the one signal that cannot be stale.
    PortsUnavailable {
        session_id: SessionId,
        /// The ports that would not bind. Not "all its ports".
        ports: Vec<i32>,
        /// Echoed from the `StartSession` that failed, so the control plane can
        /// bound its retries without keeping per-session state.
        #[serde(default)]
        attempt: u8,
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
    /// A chunk of a running loop job's output, appended to its transcript
    /// (MAIN-161). `source` is `agent` for session output, `system` for the
    /// node's own lifecycle notes. Never interpreted — recorded verbatim (NG-2).
    JobTranscript {
        job_id: String,
        source: String,
        content: String,
    },
    /// A loop job's agent started or stopped working (MAIN-240). A REAL signal
    /// off the runtime's own turn boundaries, not inferred from output timing —
    /// which is what the "agent is working" indicator (MAIN-237) needs to stop
    /// guessing. Only the streaming adapter emits it; a tmux job simply never
    /// reports one, and the UI falls back to what it did before.
    JobTurn {
        job_id: String,
        active: bool,
    },
    /// A loop job's session ended (MAIN-161). `ok=false` means it failed —
    /// non-zero exit, a timeout, or a launch that never got going — and
    /// `message` carries the reason / transcript tail for crash honesty (AC-4).
    JobFinished {
        job_id: String,
        ok: bool,
        message: String,
    },
    /// The node REFUSED to launch a build run's agent (MAIN-482). Its working
    /// directory was not a worktree of the node's own loop clone cache, was a
    /// checkout the node reports to the control plane, or had HEAD attached to
    /// the repository's default branch.
    ///
    /// Not a `JobFinished { ok: false }`, because the board consequence
    /// differs: a refused run never reached its agent, so no outcome will ever
    /// be reported for it — and the outcome handler is the only thing that
    /// releases the loop's claim. The control plane answers this by giving the
    /// card back rather than leaving it claimed with nothing running (AC-6).
    JobRefused {
        job_id: String,
        reason: String,
    },
    /// Where a BUILD run's worktree is, reported once the node has created or
    /// adopted it (MAIN-480 AC-4). The tree outlives the run, so the control
    /// plane records it on the card: that record is what pins later passes to
    /// this node, what `prune-worktree` addresses, and what tells the node's
    /// `reconcile` the directory is still wanted.
    LoopWorktreeReady {
        job_id: String,
        path: String,
    },
    /// Every build worktree this node holds, reported on (re)connect (MAIN-480
    /// AC-1). The node can no longer decide alone which of these are orphans —
    /// a tree BETWEEN passes has no running job, which is exactly the state
    /// that used to be indistinguishable from a crash leftover — so it states
    /// what it has and the control plane orders removal of what it no longer
    /// records.
    LoopWorktreesHeld {
        paths: Vec<String>,
    },
    /// Every build worktree compose project this node currently holds, reported
    /// on (re)connect and periodically after (MAIN-507 AC-5). Same division of
    /// labour as [`NodeToControl::LoopWorktreesHeld`]: whether a stack is still
    /// wanted is a card fact the node cannot see, so it states what is running
    /// and the control plane answers with the ones to bring down.
    BuildStacksHeld {
        projects: Vec<String>,
    },
    /// Whether this node is taking new loop work, and why not (MAIN-505).
    ///
    /// Sent right after `Register` on EVERY connect — including the clear
    /// `None` — and again whenever it changes. Asserting it unconditionally is
    /// what stops a cordon outliving the process that raised it: a node that
    /// restarted into the new agent holds nothing and says so, where a node
    /// that only ever spoke up when cordoned would leave the last one standing.
    CordonChanged {
        cordon: Option<nook_types::NodeCordon>,
    },
    Pong,
    /// The head of a tunnel response: status and headers, before any body.
    ///
    /// Separate from the chunks so the control plane can start writing a real
    /// HTTP response the moment the node has one, rather than waiting for a
    /// body it is deliberately not buffering.
    TunnelResponse {
        request_id: uuid::Uuid,
        #[serde(default = "crate::tunnel_v1")]
        version: u16,
        status: u16,
        headers: Vec<crate::TunnelHeader>,
    },
    /// One streamed piece of a tunnel response body.
    ///
    /// `seq` exists because the relay in a multi-replica control plane is not a
    /// single socket end to end: frames cross the bus, and "they arrived in
    /// order" is an assumption rather than a guarantee. `last` marks the end
    /// explicitly — a stream that ends by the sender going quiet is
    /// indistinguishable from one that died.
    TunnelChunk {
        request_id: uuid::Uuid,
        seq: u64,
        data_b64: String,
        last: bool,
    },
    /// The node could not complete the exchange: nothing listening on the port,
    /// the upstream died mid-response, or a `version` it does not understand.
    ///
    /// A terminal frame, like `last`. Anything holding this request can drop it
    /// on receipt.
    TunnelFailed {
        request_id: uuid::Uuid,
        message: String,
    },
    /// The upstream accepted a [`crate::ControlToNode::TunnelUpgrade`]: the
    /// visitor may now be told `101` (MAIN-10 AC-1).
    ///
    /// It carries the upstream's own handshake headers because one of them
    /// decides whether the visitor's socket works at all — a browser that
    /// asked for a subprotocol and is answered without one fails the
    /// connection, and `vite-hmr` is exactly that case. The control plane
    /// cannot invent the answer: only the app behind the tunnel knows which
    /// subprotocol it speaks.
    TunnelUpgraded {
        request_id: uuid::Uuid,
        #[serde(default = "crate::tunnel_v1")]
        version: u16,
        headers: Vec<crate::TunnelHeader>,
    },
    /// One frame the upstream sent, on its way to the visitor.
    ///
    /// Base64 even for text, like `SessionOutput`: a WebSocket payload is
    /// bytes, and a binary frame that is not valid UTF-8 has nowhere to live
    /// in a JSON string. `binary` preserves the distinction that survives the
    /// encoding — an app that reads `Blob` from a text frame gets the wrong
    /// type, which is a corruption of a different kind.
    TunnelWsData {
        request_id: uuid::Uuid,
        data_b64: String,
        binary: bool,
    },
    /// The upstream socket closed. Terminal, like `last` on a chunk: whoever
    /// holds this request drops it on receipt.
    TunnelWsClose {
        request_id: uuid::Uuid,
        code: Option<u16>,
        reason: Option<String>,
    },
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
    /// Merge the managed hook set into Claude Code's `settings.json`.
    ///
    /// Beside `InstallSkill`: same push-the-content model (a node already has an
    /// authenticated channel; the fragment is a few hundred bytes), same
    /// skip-when-the-sha-matches convergence. `content` is the JSON settings
    /// fragment the control plane holds; `sha256` is of that content, so a node
    /// whose file already carries this exact managed set writes nothing on
    /// reconnect. User-owned hook entries are preserved on merge — only the
    /// nook-managed ones (each marked) are replaced (MAIN-105).
    InstallHooks {
        content: String,
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
        /// Which workspace this checkout is, so the session can export it and
        /// anything inside — the git-ssh shim in particular — can name the repo
        /// it is in without asking the control plane about the session first
        /// (MAIN-367). `None` for an ad-hoc terminal, which is in no workspace.
        #[serde(default)]
        workspace_id: Option<WorkspaceId>,
        /// Which TENANT this session belongs to, exported as `NOOK_TENANT_ID`
        /// so a `nook` command inside it is already scoped and an agent never
        /// has to guess which board or workspace it means.
        ///
        /// The WORKSPACE's tenant, never the node's. Cross-tenant placement
        /// (MAIN-353) runs one org's checkout on another org's machine, so the
        /// node's own `tenant_slug` is the wrong answer in exactly the case
        /// this exists for. `None` for an ad-hoc terminal, which is in no
        /// workspace and so in no tenant.
        #[serde(default)]
        tenant_id: Option<TenantId>,
        cols: u16,
        rows: u16,
        /// The ports leased to this session (MAIN-301), each carrying the env
        /// var its workspace declared for it — so an app in a worktree binds
        /// somewhere free instead of a hardcoded 3000, and an app with a web
        /// port AND an api port gets both. Empty when the node advertises no
        /// range or the workspace declares no listeners; the session still
        /// starts, it just has nothing to offer.
        ///
        /// The NAMES come from the workspace, never from this end — that is
        /// what keeps the node as ignorant of frameworks as the broker is.
        #[serde(default)]
        ports: Vec<nook_types::LeasedPort>,
        /// Which try this is. Rides in the message rather than in a table so a
        /// re-send after a port clash cannot loop forever, and so nothing has to
        /// be cleaned up when a session finally starts.
        #[serde(default)]
        attempt: u8,
        /// Declared listeners this session asked for and did NOT get (MAIN-377).
        ///
        /// Travels beside `ports` because it is the other half of the same
        /// answer. An absent env var has two opposite meanings — "no nook here,
        /// use your default" and "the node ran out, and your default is the
        /// literal every other session also falls back to" — and only this can
        /// tell the session which it is. Empty in the satisfied case, which is
        /// the overwhelmingly common one.
        #[serde(default)]
        unsatisfied: Vec<String>,
        /// What the control plane is keeping this session for (MAIN-326), when
        /// it is a managed one. `None` — an ad-hoc terminal or a hand-started
        /// session — is the overwhelmingly common case and means "just open the
        /// runtime", which is all this message ever did before.
        ///
        /// A PURPOSE, never a command. The node maps it to a fixed line it types
        /// once the runtime is up, for the same reason `StartAuthSession`
        /// carries a runtime rather than a login command: nothing on this wire
        /// should be able to choose what runs on a machine.
        #[serde(default)]
        managed_purpose: Option<nook_types::ManagedPurpose>,
        /// Which slice of the repo's work this session owns (MAIN-446), when it
        /// is one of several sharing a declaration. `None` — everything that is
        /// not a sharded review loop — means the whole of it.
        ///
        /// Travels as a PAIR for the reason the type gives: an index is only
        /// valid against the divisor it was computed with, and two independent
        /// fields on a wire can arrive disagreeing.
        #[serde(default)]
        shard: Option<nook_types::ShardAssignment>,
        /// Terminal or chat (MAIN-502). Absent — every node and every control
        /// plane that predates the field — is `terminal`, which is the tmux
        /// path this message has always meant.
        #[serde(default)]
        interface: nook_types::SessionInterface,
    },
    /// A human turn for a CHAT session's agent (MAIN-502).
    ///
    /// `SessionInput`'s counterpart, and deliberately not `SessionInput`
    /// itself: that carries keystrokes for a PTY, base64 because a terminal
    /// is a byte stream. This carries one whole message, because the streaming
    /// protocol's unit is a turn — which is also why a pasted multi-line brief
    /// survives here and never did through `send-keys`.
    ChatMessage {
        session_id: SessionId,
        text: String,
    },
    /// A human's answer to a chat session's outstanding permission request
    /// (MAIN-502). The agent is blocked on this and resumes the moment it
    /// arrives; a denial is reported to the agent as a refusal, not as a
    /// silent failure.
    ChatPermissionDecision {
        session_id: SessionId,
        /// The runtime's own id for the request, echoed back untouched. The
        /// node matches it against what it is holding; a stale id — already
        /// answered, or from a previous process — is dropped, which is what
        /// makes answering from two devices harmless.
        request_id: String,
        allow: bool,
        /// "Allow always (this tool, this session)" (MAIN-620 AC-3). The node
        /// answers that tool itself from here on and stops announcing it.
        ///
        /// `#[serde(default)]` so a control plane that has this and a node that
        /// does not still speak: the older node ignores the field and behaves
        /// exactly as it did, which is one more prompt rather than a broken
        /// session.
        #[serde(default)]
        remember: bool,
    },
    /// Install a runtime credential this node did not obtain (MAIN-283).
    ///
    /// `payload` is **opaque**: the control plane got it from the runtime's own
    /// provider and this end neither parses nor validates it. What the node
    /// decides is only *where* it goes — a fixed per-runtime rule keyed by
    /// `runtime`, never a path from the wire, for the same reason
    /// `StartAuthSession` carries a runtime rather than a command.
    ///
    /// This replaces driving `claude auth login` in a live session: a fleet can
    /// be authorized once and delivered to, instead of one terminal at a time.
    InstallRuntimeCredential {
        runtime: String,
        /// The credential bytes, base64 so the frame stays JSON-safe whatever
        /// the runtime's file format is.
        payload_b64: String,
    },
    /// Start a runtime's LOGIN flow in a session, so a headless node can be
    /// authorized from the UI (MAIN-126). The node — never the caller — chooses
    /// the allowlisted login command for `runtime` (e.g. `claude auth login`);
    /// `runtime` is only the key into that fixed table. The session then streams
    /// and takes input exactly like any other, so the device code/URL is
    /// readable and any pasted-back code reaches the CLI.
    StartAuthSession {
        session_id: SessionId,
        runtime: String,
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
        /// The slug of the tenant this clone is FOR, so the checkout lands in
        /// that tenant's tree (MAIN-363).
        ///
        /// The node cannot work this out: it knows only its own home tenant,
        /// and cross-tenant placement (MAIN-353) means the tenant that asked is
        /// routinely not that one. Absent — an older control plane, or a node
        /// that predates this — the node falls back to its own configured slug,
        /// which is the pre-existing behaviour rather than a regression.
        #[serde(default)]
        tenant_slug: Option<String>,
    },
    /// Add a git worktree next to an existing checkout: the same workspace
    /// gains another location (branch) on this node.
    AddWorktree {
        request_id: uuid::Uuid,
        repo_path: String,
        branch: String,
    },
    /// Remove a git worktree checkout (the "done → prune" step).
    ///
    /// The node brings the worktree's compose stack down FIRST (MAIN-507 AC-3):
    /// the project name is derived from this directory, so after git takes the
    /// tree away nothing can name the containers it left running.
    ///
    /// `delete_branch` also frees the branch the tree held, with git's own
    /// `branch -d` (MAIN-537 AC-5) — asked for only where the tree is a build's
    /// and the card is over, never on a human's own checkout. `#[serde(default)]`
    /// so a node running an older build parses the message and does what it did
    /// before: remove the directory and leave the branch.
    RemoveWorktree {
        request_id: uuid::Uuid,
        worktree_path: String,
        #[serde(default)]
        delete_branch: bool,
    },
    /// Bring build worktree compose projects down, volumes and all (MAIN-507).
    ///
    /// The control plane names them because it is the only side that knows
    /// which cards are finished; the node reaps only names
    /// [`crate::compose::is_build_stack_project`] accepts, so a bug on either
    /// side still cannot reach a human's own stack (NG-3).
    ///
    /// Answered with [`NodeToControl::OpResult`], whose `path` carries the
    /// projects that actually came down (`None` when nothing was running) —
    /// that is what the card's report is written from (AC-7).
    ReapBuildStacks {
        request_id: uuid::Uuid,
        projects: Vec<String>,
    },
    /// Stage a checkout and commit it. `paths` names what to stage; `None`
    /// stages everything (MAIN-325).
    ///
    /// `#[serde(default)]` so a node running an older build still parses the
    /// message — it ignores the field and stages everything, which is exactly
    /// what it did before. Wrong for a partial commit, but a stale node is a
    /// deploy problem, not a crash.
    GitCommit {
        request_id: uuid::Uuid,
        checkout_path: String,
        message: String,
        #[serde(default)]
        paths: Option<Vec<String>>,
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
    /// Create a brand-new git project under the node's workspace root, with
    /// the scaffold `gitops::init_project` writes.
    InitProject {
        request_id: uuid::Uuid,
        name: String,
        /// What the project is, in the creator's words. Rides in optionally
        /// (MAIN-619 AC-8) so a control plane that does not send it — or a
        /// caller that had nothing to say — still gets everything else.
        #[serde(default)]
        description: Option<String>,
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
    /// A human answered a durable interaction this node requested (MAIN-159).
    /// Pushed to the executor so a waiting run is unblocked without polling;
    /// `request_id` is the interaction id the `nook interactions ask` raised.
    InteractionAnswer {
        request_id: String,
        answer: String,
    },
    /// Run a loop job on this node (MAIN-161): materialize the workspace from the
    /// clone cache, make a per-job worktree, spawn the matching skill session
    /// (`nook-spec`/`nook-epic`) pointed at `target_task_key` with `NOOK_JOB_ID`
    /// set, stream output back as `JobTranscript`, and report `JobFinished`.
    /// Everything the node needs is on the message — no DB round-trip.
    RunLoopJob {
        job_id: String,
        /// `spec` | `decompose` | `review` — selects the skill.
        kind: String,
        /// The pull request a `review` run owns (MAIN-455). `None` for every
        /// other kind.
        ///
        /// Two jobs at once: it tells the agent WHICH PR it is reviewing, so it
        /// never has to search a list for its share of the work, and it names
        /// the agent session to resume (`--from-pr`) so a second review of the
        /// same PR keeps the tree and the earlier reasoning instead of
        /// rebuilding both.
        #[serde(default)]
        review_pr_number: Option<u64>,
        /// A human forced this review at an already-verdicted head
        /// (MAIN-473): exported to the run as `NOOK_REVIEW_FORCED`, so the
        /// reviewer's already-reviewed skip-check stands aside for exactly
        /// this run. `#[serde(default)]` keeps older peers readable.
        #[serde(default)]
        review_forced: bool,
        /// The workspace's own forge token (MAIN-456), exported to the run as
        /// `GH_TOKEN`. `None` means the node's fleet env applies — the
        /// single-tenant fallback.
        #[serde(default)]
        gh_token: Option<String>,
        /// The control plane's advertised HTTP API base URL (MAIN-465,
        /// `NOOK_PUBLIC_API_URL`): what the
        /// run's `nook` CLI should dial and report. The job's token is minted
        /// by the control plane that raised the run, so it travels with its
        /// ISSUER'S canonical address — not with whatever address this node
        /// happened to dial (inside compose, an internal service name nobody
        /// outside the network namespace can identify). `None` means the
        /// deployment advertises nothing and the node's own configured server
        /// address applies, exactly as before.
        #[serde(default)]
        server_url: Option<String>,
        /// The board key of the ticket the skill points at (e.g. `MAIN-42`).
        target_task_key: String,
        /// The clonable git remote, resolved by the control plane from the
        /// executor's `node_workspaces` row.
        repo_url: String,
        /// Which workspace that row is for (MAIN-367), so the job's session can
        /// export `NOOK_WORKSPACE_ID` and the git-ssh shim can name the repo it
        /// is working in. Resolved from the same row as `repo_url`.
        #[serde(default)]
        workspace_id: Option<WorkspaceId>,
        /// The workspace's pinned ssh key, for the CLONE CACHE.
        ///
        /// `workspace_id` above lets the job's SESSION authenticate, via the
        /// git-ssh shim. That is not enough: the job first prepares a bare
        /// mirror in `~/.nook/clone-cache`, and that runs in the node process
        /// before any session exists — with no `GIT_SSH_COMMAND`, no
        /// `NOOK_WORKSPACE_ID`, and so the node's own generated key, which no
        /// private repo authorizes. A loop job on a private repo therefore died
        /// at "preparing workspace" with `Permission denied (publickey)` however
        /// correctly the credential was pinned.
        ///
        /// Delivered the same way `CloneRepo` has always delivered one: over
        /// this socket, written to a transient 0600 file for the git command and
        /// removed straight after. `None` means the workspace pins nothing, and
        /// the node's own reach applies — a public repo or a local path.
        #[serde(default)]
        ssh_key: Option<String>,
        /// The credential the AGENT acts with.
        ///
        /// Without these the agent shells out to `nook`, which reads a FILE —
        /// whatever `nook login` last wrote on the executor. On a shared
        /// operator node that is one human's token for one tenant, so a job for
        /// another tenant's workspace listed that human's boards and drafted
        /// against the wrong one. The job never picked a board; it had no
        /// identity to pick with.
        ///
        /// Scoped to the job's tenant, issued as its initiator, revoked when the
        /// job ends. `None` means minting failed — the agent falls back to the
        /// node's login exactly as before, which is logged loudly.
        ///
        /// Only the token travels: the node already knows which control plane it
        /// belongs to, so sending a server URL would be a second source of truth
        /// for something it can never disagree with itself about.
        #[serde(default)]
        nook_token: Option<String>,
        /// The branch the per-job worktree is based on.
        branch: String,
        /// The human's opening brief for this run (MAIN-231), if one was given
        /// at create time. Delivered into the session's environment as
        /// `NOOK_JOB_SEED` and typed alongside the skill command.
        #[serde(default)]
        seed: Option<String>,
        /// The ports this run's workspace declared, leased from the node's
        /// range (MAIN-552), each exported into the run's environment as the
        /// variable that workspace named.
        ///
        /// Without these a build's `docker compose up` took
        /// `docker-compose.yml`'s `${VAR:-default}` fallbacks and collided with
        /// every other stack on the machine — including the human sessions that
        /// have leased since MAIN-301. Empty when the node advertises no range
        /// or the workspace declares nothing for this runtime, which is a
        /// working run without ports rather than a failure.
        #[serde(default)]
        ports: Vec<nook_types::LeasedPort>,
        /// Declared listeners that went unleased, in declaration order — only
        /// ever OPTIONAL ones, since a required one keeps the job queued
        /// instead. Exported as `NOOK_PORTS_UNSATISFIED`, the same name and the
        /// same reason as a session's (MAIN-377): an absent variable otherwise
        /// reads as "cloned outside nook, use your default", which is the
        /// shared literal everything else also falls back to.
        #[serde(default)]
        unsatisfied_ports: Vec<String>,
    },
    /// An unsolicited steering message from a human to a running loop job
    /// (MAIN-231). Pushed to the executor, which types it into the job's live
    /// session. Unlike `InteractionAnswer` this answers no ask — it is the
    /// human volunteering direction mid-run.
    JobMessage {
        job_id: String,
        body: String,
    },
    Ping,
    /// Proxy ONE HTTP request to a port on this node (MAIN-402 AC-2).
    ///
    /// The request is buffered — it has already been read whole by the time the
    /// control plane sends it — but the RESPONSE is streamed back as
    /// [`NodeToControl::TunnelChunk`]s. A tunnel that buffers the response is a
    /// memory bug waiting for somebody to curl a large file through it.
    TunnelRequest {
        /// See [`TUNNEL_PROTOCOL_VERSION`]. Absent means generation 1.
        #[serde(default = "tunnel_v1")]
        version: u16,
        /// Correlates every frame of this exchange, in both directions and
        /// across control-plane replicas.
        request_id: uuid::Uuid,
        /// The port on the node's own loopback to connect to.
        port: u16,
        method: String,
        /// Path AND query, exactly as received — splitting them here would mean
        /// re-encoding, and re-encoding a query string is how a signature check
        /// on the other side starts failing.
        path: String,
        headers: Vec<TunnelHeader>,
        /// Base64, because this frame is JSON and a body is not text.
        body_b64: String,
    },
    /// Open a WebSocket to a port on this node (MAIN-10 AC-1).
    ///
    /// The upgrade half of [`ControlToNode::TunnelRequest`] — same
    /// `request_id` convention, same loopback-only rule, same `version` — and
    /// a variant of its own rather than a flag on it, because what follows is
    /// different in kind: a run of frames in BOTH directions with no head to
    /// wait for and no `last` to end it.
    ///
    /// `headers` has already had NookOS's credentials taken out of it, and the
    /// node drops the handshake headers its own client owns; what is left is
    /// what the app is entitled to see, `sec-websocket-protocol` included.
    TunnelUpgrade {
        #[serde(default = "tunnel_v1")]
        version: u16,
        request_id: uuid::Uuid,
        port: u16,
        path: String,
        headers: Vec<TunnelHeader>,
    },
    /// One frame from the visitor, on its way to the upstream socket. See
    /// [`NodeToControl::TunnelWsData`] for why it is base64 in both directions.
    TunnelWsData {
        request_id: uuid::Uuid,
        data_b64: String,
        binary: bool,
    },
    /// The visitor's end is gone — they closed it, or the tunnel it rode on
    /// was stopped, expired or lost its session (MAIN-10 AC-4, AC-5). Either
    /// way the node drops the upstream connection.
    TunnelWsClose {
        request_id: uuid::Uuid,
        code: Option<u16>,
        reason: Option<String>,
    },
}

pub mod tunnel;

/// The generation of the tunnel frames below (MAIN-402 AC-1).
///
/// Bumped when a frame's MEANING changes, not when a field is added — an added
/// field carries `#[serde(default)]` and needs no bump. A node compares this to
/// its own and refuses a request it cannot honour, with
/// [`NodeToControl::TunnelFailed`] naming the version it understands. That is
/// the difference between degrading and misparsing: an unknown VARIANT is
/// already skipped by the node's read loop, but a known variant with a meaning
/// it does not share would otherwise be obeyed wrongly and silently.
///
/// MAIN-10's upgrade frames did NOT bump it, and that is the rule working
/// rather than an oversight: they are new variants, so an older node skips
/// `TunnelUpgrade` and the control plane gives up on the handshake, while its
/// plain HTTP tunnels keep working. Bumping would have made every existing
/// node refuse those too, to fix nothing.
pub const TUNNEL_PROTOCOL_VERSION: u16 = 1;

/// The default for a peer that predates the field — the first generation.
fn tunnel_v1() -> u16 {
    1
}

/// One header, as it crosses the wire. A `Vec` of these rather than a map,
/// because HTTP allows repeats (`set-cookie`) and a map would silently keep one.
pub type TunnelHeader = (String, String);

/// Live events pushed to browsers over `/api/v1/ws/ui`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum UiEvent {
    /// A device flow has started and is waiting for a human (MAIN-290).
    ///
    /// Carries the only two things a person needs — the code to type and where
    /// to type it — so the UI can put them on screen while the control plane is
    /// still polling the provider. The device code itself is NOT here: it is
    /// the secret half of the exchange and never leaves the control plane.
    RuntimeAuthPrompt {
        flow_id: uuid::Uuid,
        runtime: String,
        user_code: String,
        /// The pre-filled link when the provider offers one, else the plain
        /// verification URI.
        verification_uri: String,
        /// Seconds until the code stops working, so the UI can count down
        /// rather than leave a dead code on screen.
        expires_in_secs: u64,
    },
    /// One node's outcome for a delivery (MAIN-290).
    ///
    /// Emitted per node, not once per flow: authorize-once/deliver-to-N means
    /// some machines can succeed while others fail, and a single aggregate
    /// result would hide that. `error` present means this node did not get it.
    RuntimeAuthDelivered {
        flow_id: uuid::Uuid,
        node_id: NodeId,
        runtime: String,
        #[serde(default)]
        error: Option<String>,
    },
    /// The flow ended without a credential (MAIN-290).
    ///
    /// `kind` is the machine-readable class — `expired`, `denied`, `provider`,
    /// `transport` — so the UI can offer "start again" for an expired code and
    /// something different for a refusal, rather than one dead end for all of
    /// them.
    RuntimeAuthFailed {
        flow_id: uuid::Uuid,
        runtime: String,
        kind: String,
        message: String,
    },
    NodeStatus {
        node_id: NodeId,
        name: String,
        status: String,
    },
    SessionStatus {
        session_id: SessionId,
        status: String,
    },
    /// What the agent in a session is doing right now: `running`, `waiting`
    /// (blocked on a human), or `idle`. Driven by Claude Code hooks reporting
    /// through `nook agent-state`, so the terminal tabs can show a spinner vs a
    /// "needs you" mark without anyone watching the output. `window` is the
    /// tmux window index the agent runs in, so the right in-session terminal
    /// chip lights up and the shells beside it do not.
    SessionAgentState {
        session_id: SessionId,
        #[serde(default)]
        window: Option<u32>,
        state: String,
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
    /// A durable interaction was raised, answered, or canceled (MAIN-159).
    /// Carries only the subject ticket id (when any), the same "what you have is
    /// stale" contract as `TaskChanged`: the client refetches the pending list
    /// and, if a ticket is named, that ticket's interactions.
    InteractionChanged {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<nook_types::TaskId>,
    },
    /// A loop job's transcript grew or its state changed (MAIN-128) — the nudge
    /// that drives every live job surface. Carries the TARGET TICKET id (not
    /// the job id) when the job has one, the same "what you have is stale"
    /// contract as `TaskChanged`. `None` for a REVIEW run (MAIN-455), which has
    /// no ticket — its surface is the workspace's Reviews panel, and skipping
    /// the nudge for ticketless jobs is exactly what left that panel static
    /// while a spec's streamed. Visibility is enforced on the refetch, so the
    /// nudge itself leaks nothing.
    JobChanged {
        #[serde(default)]
        task_id: Option<nook_types::TaskId>,
    },
    /// A loop job's agent started or stopped a turn (MAIN-240). Distinct from
    /// `JobChanged`, whose contract is "what you have is stale, refetch": this
    /// carries the fact itself, because a turn is live state with no row to go
    /// and read. Only the streaming adapter produces it.
    JobTurn {
        task_id: nook_types::TaskId,
        job_id: nook_types::JobId,
        active: bool,
    },
    /// A chat session's conversation grew (MAIN-502) — a message arrived, or a
    /// permission request was answered.
    ///
    /// Carries only the session id, the same "what you have is stale, refetch"
    /// contract as `TaskChanged`. Visibility is enforced on the refetch, so
    /// the nudge itself leaks nothing; and because it is a nudge rather than
    /// the message, a second device that was offline for a while converges on
    /// the same conversation instead of replaying a stream it half-missed.
    SessionMessage {
        session_id: SessionId,
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

    /// Every skill shipped in the repo must be teachable by the same path
    /// `nook teach` uses: a parseable frontmatter `name:` that is a valid skill
    /// name AND equals the directory it lives in, so teaching `skills/<dir>` is
    /// never a surprise. This guards the nook-spec/build/review packaging
    /// (MAIN-31) and the pre-existing nookos skill together.
    #[test]
    fn every_shipped_skill_is_teachable_and_named_after_its_directory() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../skills");
        let mut checked = 0;
        for entry in std::fs::read_dir(&root).expect("skills/ must exist") {
            let dir = entry.unwrap().path();
            if !dir.is_dir() {
                continue;
            }
            let file = dir.join("SKILL.md");
            let content = std::fs::read_to_string(&file)
                .unwrap_or_else(|_| panic!("{} must have a SKILL.md", dir.display()));
            let declared = skill_name_from_frontmatter(&content)
                .unwrap_or_else(|| panic!("{} has no frontmatter name:", file.display()));
            valid_skill_name(&declared).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
            let dir_name = dir.file_name().unwrap().to_string_lossy();
            assert_eq!(
                declared,
                dir_name,
                "{}: frontmatter name must match its directory",
                file.display()
            );
            checked += 1;
        }
        assert!(
            checked >= 4,
            "expected nookos + nook-spec/build/review, saw {checked}"
        );
    }

    /// The MAIN-31 rename must be complete: none of the three loop-derived
    /// skills may still name the old `loop-*` skills. (The GitHub *labels*
    /// `loop-changes-requested` / `loop-approved` are a different string and are
    /// deliberately preserved, so this checks only the skill-name tokens.)
    #[test]
    fn the_nook_skills_carry_no_loop_skill_names() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../skills");
        for name in ["nook-spec", "nook-build", "nook-review"] {
            let content = std::fs::read_to_string(root.join(name).join("SKILL.md"))
                .unwrap_or_else(|_| panic!("{name}/SKILL.md must exist"));
            for stale in ["loop-spec", "loop-build", "loop-review"] {
                assert!(!content.contains(stale), "{name} still references {stale}");
            }
        }
    }
}

#[cfg(test)]
mod wire_tests {
    use super::*;

    /// The MAIN-105 push variant round-trips: adjacently tagged, sha travels
    /// with the content, and nothing is lost across serialize→deserialize.
    #[test]
    fn install_hooks_round_trips() {
        let msg = ControlToNode::InstallHooks {
            content: r#"{"Stop":[]}"#.into(),
            sha256: "abc123".into(),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "install_hooks");
        assert_eq!(json["data"]["sha256"], "abc123");
        let back: ControlToNode = serde_json::from_value(json).unwrap();
        assert!(
            matches!(back, ControlToNode::InstallHooks { content, sha256 }
                if content == r#"{"Stop":[]}"# && sha256 == "abc123")
        );
    }

    /// Credential delivery round-trips, and the payload survives base64
    /// intact — including bytes that are not valid UTF-8, because the payload
    /// is opaque and a runtime's credential file need not be text (MAIN-283).
    #[test]
    fn install_runtime_credential_round_trips_with_an_opaque_payload() {
        use base64::Engine as _;
        let raw: &[u8] = &[0x00, 0xff, 0xfe, b'{', b'}'];
        let msg = ControlToNode::InstallRuntimeCredential {
            runtime: "claude".into(),
            payload_b64: base64::engine::general_purpose::STANDARD.encode(raw),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "install_runtime_credential");
        assert_eq!(json["data"]["runtime"], "claude");
        // The credential must not be readable in the frame as plain text.
        assert_ne!(json["data"]["payload_b64"], "\u{0}\u{ff}");

        let back: ControlToNode = serde_json::from_value(json).unwrap();
        let ControlToNode::InstallRuntimeCredential {
            runtime,
            payload_b64,
        } = back
        else {
            panic!("wrong variant");
        };
        assert_eq!(runtime, "claude");
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(payload_b64)
                .unwrap(),
            raw,
            "the payload survives the wire byte for byte"
        );
    }

    /// The delivery report round-trips, and `error` is what distinguishes a
    /// failure from a success (MAIN-283 AC-5).
    #[test]
    fn runtime_credential_installed_round_trips_both_outcomes() {
        for error in [None, Some("cannot create /nope".to_string())] {
            let msg = NodeToControl::RuntimeCredentialInstalled {
                runtime: "claude".into(),
                path: "/nook-claude/.credentials.json".into(),
                error: error.clone(),
            };
            let json = serde_json::to_value(&msg).unwrap();
            assert_eq!(json["type"], "runtime_credential_installed");
            let back: NodeToControl = serde_json::from_value(json).unwrap();
            assert!(matches!(
                back,
                NodeToControl::RuntimeCredentialInstalled { error: e, .. } if e == error
            ));
        }
    }

    /// The re-probe push round-trips (MAIN-126 AC-4).
    #[test]
    fn runtime_auth_status_round_trips() {
        let msg = NodeToControl::RuntimeAuthStatus {
            profiles: vec![AuthProfile {
                id: "claude".into(),
                label: "Claude Code".into(),
                runtime: "claude".into(),
                state: nook_types::AuthState::Authorized,
                identity: Some("pm@example.com".into()),
            }],
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "runtime_auth_status");
        let back: NodeToControl = serde_json::from_value(json).unwrap();
        assert!(
            matches!(back, NodeToControl::RuntimeAuthStatus { profiles } if profiles.len() == 1 && profiles[0].state == nook_types::AuthState::Authorized)
        );
    }

    /// The authorize-launch variant round-trips as an adjacently-tagged message.
    #[test]
    fn start_auth_session_round_trips() {
        let msg = ControlToNode::StartAuthSession {
            session_id: SessionId::new(),
            runtime: "claude".into(),
            cols: 120,
            rows: 32,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "start_auth_session");
        assert_eq!(json["data"]["runtime"], "claude");
        let back: ControlToNode = serde_json::from_value(json).unwrap();
        assert!(
            matches!(back, ControlToNode::StartAuthSession { runtime, .. } if runtime == "claude")
        );
    }

    /// The report variant round-trips, and `error` defaults to absent (a
    /// success) exactly as `SkillInstalled`'s does.
    #[test]
    fn hooks_installed_round_trips_and_error_defaults() {
        let ok = NodeToControl::HooksInstalled {
            path: "/home/u/.claude/settings.json".into(),
            error: None,
        };
        let json = serde_json::to_value(&ok).unwrap();
        assert_eq!(json["type"], "hooks_installed");
        let back: NodeToControl = serde_json::from_value(json).unwrap();
        assert!(matches!(
            back,
            NodeToControl::HooksInstalled { error: None, .. }
        ));

        // An older/absent `error` field deserializes as a success, not a parse
        // error — the failure-is-optional contract.
        let minimal = serde_json::json!({
            "type": "hooks_installed",
            "data": { "path": "/x" }
        });
        let back: NodeToControl = serde_json::from_value(minimal).unwrap();
        assert!(matches!(
            back,
            NodeToControl::HooksInstalled { error: None, .. }
        ));

        let failed = NodeToControl::HooksInstalled {
            path: String::new(),
            error: Some("settings.json is not valid JSON".into()),
        };
        let json = serde_json::to_value(&failed).unwrap();
        let back: NodeToControl = serde_json::from_value(json).unwrap();
        assert!(
            matches!(back, NodeToControl::HooksInstalled { error: Some(e), .. } if e.contains("valid JSON"))
        );
    }
}
