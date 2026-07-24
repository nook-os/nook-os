//! In-memory connection registry: which nodes are connected (and how to reach
//! them), per-tenant UI broadcast channels, and per-session terminal fan-out.
//!
//! Multi-instance: each control-plane process has an `instance_id`; node
//! ownership is leased in Postgres and everything that must cross instances
//! (node commands, terminal frames, UI events, op replies, viewer/driver
//! sizing) rides the LISTEN/NOTIFY bus in `bus.rs`. Callers never see any of
//! this — every method keeps its single-instance signature and the local path
//! stays the fast path. Without `start_bus` (tests, single instance) behavior
//! is identical to the original in-memory registry.

use std::collections::HashSet;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use nook_proto::{AttachServerMessage, ControlToNode, UiEvent};
use nook_types::{GitFileStatus, NodeId, SessionId, TenantId};
use sqlx::PgPool;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use uuid::Uuid;

use super::bus::{self, BusMessage, Outbound, ViewerEvent};

pub struct NodeHandle {
    pub tenant_id: TenantId,
    pub tx: mpsc::Sender<ControlToNode>,
}

/// Payload completing a `GetGitStatus` request.
pub struct GitStatusPayload {
    pub is_repo: bool,
    pub branch: Option<String>,
    pub files: Vec<GitFileStatus>,
    pub diff: String,
}

/// Completion of a long-running node operation.
pub struct OpPayload {
    pub ok: bool,
    pub path: Option<String>,
    pub message: String,
}

struct LocalNode {
    handle: NodeHandle,
    /// Guards against a stale socket's cleanup removing a fresh registration
    /// (node reconnected before the old connection finished dying).
    epoch: u64,
}

pub struct Registry {
    instance_id: Uuid,
    nodes: DashMap<NodeId, LocalNode>,
    next_epoch: std::sync::atomic::AtomicU64,
    ui: DashMap<TenantId, broadcast::Sender<UiEvent>>,
    /// Terminal output fan-out: every browser attached to a session
    /// subscribes here; the node's output frames are broadcast to all.
    attachments: DashMap<SessionId, broadcast::Sender<AttachServerMessage>>,
    /// Per-session viewer bookkeeping: the PTY follows the "driver" — the
    /// viewer who most recently TYPED. Lives on the session's OWNING instance
    /// only; other instances route viewer events here over the bus.
    viewers: DashMap<SessionId, SessionViewers>,
    next_viewer: std::sync::atomic::AtomicU64,
    /// In-flight git status requests awaiting a node's response.
    pending_git: DashMap<Uuid, oneshot::Sender<GitStatusPayload>>,
    /// In-flight long-running git operations (clone, worktree).
    pending_ops: DashMap<Uuid, oneshot::Sender<OpPayload>>,

    // ── Cross-instance state (inert until `start_bus`) ─────────────────────
    /// Which node a session lives on — sniffed from outgoing messages so
    /// viewer events can be routed by session id alone.
    session_nodes: DashMap<SessionId, NodeId>,
    /// node → (owning instance, local expiry) mirror of the Postgres leases.
    lease_cache: DashMap<NodeId, (Uuid, Instant)>,
    /// Requests we forwarded to our local node on behalf of another instance:
    /// request id → the instance the answer must go back to.
    remote_pending_ops: DashMap<Uuid, Uuid>,
    remote_pending_git: DashMap<Uuid, Uuid>,
    /// Other instances with live viewers for sessions our nodes own.
    remote_viewers: DashMap<SessionId, HashSet<Uuid>>,
    bus_tx: OnceLock<mpsc::UnboundedSender<Outbound>>,
    /// Flips to `true` once the Postgres LISTEN is actually established. Until
    /// then a NOTIFY this instance sends can be dropped — Postgres only
    /// delivers to sessions already listening — so anything that publishes
    /// immediately after `start_bus` must `await bus_ready()` first. In
    /// production the gap is invisible (instances start the bus long before
    /// serving traffic); in a test that publishes within milliseconds it is
    /// the whole ballgame.
    bus_ready: watch::Sender<bool>,

    /// The current agent state per session — `running` / `waiting` / `idle`,
    /// the tmux window it is in, and when it was last reported. Held in memory,
    /// not the database: it is ephemeral by nature (a spinner, not a record),
    /// and a browser that connects late reads it from here rather than waiting
    /// for the next transition. Keyed by session; the tenant is stored so the
    /// reload snapshot can be scoped to the caller.
    agent_state: DashMap<SessionId, AgentStateEntry>,
}

/// One session's live agent state. `at` gates staleness: a `running` state that
/// nothing has refreshed in `AGENT_STATE_TTL` is treated as gone, so a crashed
/// agent cannot leave a tab spinning forever.
#[derive(Clone)]
pub struct AgentStateEntry {
    pub tenant: TenantId,
    pub window: Option<u32>,
    pub state: String,
    pub at: Instant,
}

/// A `running`/`waiting` state older than this, with no refresh, is stale —
/// the hooks report on every transition, and a healthy agent transitions far
/// more often than this, so silence this long means the process is gone.
pub const AGENT_STATE_TTL: Duration = Duration::from_secs(15 * 60);

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    pub fn new() -> Self {
        Self {
            instance_id: Uuid::now_v7(),
            nodes: DashMap::new(),
            next_epoch: std::sync::atomic::AtomicU64::new(1),
            ui: DashMap::new(),
            attachments: DashMap::new(),
            viewers: DashMap::new(),
            next_viewer: std::sync::atomic::AtomicU64::new(0),
            pending_git: DashMap::new(),
            pending_ops: DashMap::new(),
            session_nodes: DashMap::new(),
            lease_cache: DashMap::new(),
            remote_pending_ops: DashMap::new(),
            remote_pending_git: DashMap::new(),
            remote_viewers: DashMap::new(),
            bus_tx: OnceLock::new(),
            bus_ready: watch::channel(false).0,
            agent_state: DashMap::new(),
        }
    }

    /// Record an agent's state and return `true` if it changed (a repeat of the
    /// same state just refreshes the timestamp, so a poll cannot spam the UI).
    /// `idle` clears the entry — idle is the absence of a spinner, and keeping a
    /// row for it would only be a thing to expire later.
    pub fn set_agent_state(
        &self,
        tenant: TenantId,
        session: SessionId,
        window: Option<u32>,
        state: &str,
    ) -> bool {
        if state == "idle" {
            return self.agent_state.remove(&session).is_some();
        }
        let changed = self
            .agent_state
            .get(&session)
            .is_none_or(|e| e.state != state || e.window != window);
        self.agent_state.insert(
            session,
            AgentStateEntry {
                tenant,
                window,
                state: state.to_string(),
                at: Instant::now(),
            },
        );
        changed
    }

    /// Forget a session's agent state — on death, or when it goes stale.
    pub fn clear_agent_state(&self, session: SessionId) -> bool {
        self.agent_state.remove(&session).is_some()
    }

    /// Every live (non-stale) agent state for a tenant, for seeding a browser on
    /// load. Sweeps stale entries as it goes, so a crashed agent's spinner does
    /// not survive a refresh.
    pub fn agent_states_for(&self, tenant: TenantId) -> Vec<(SessionId, Option<u32>, String)> {
        let now = Instant::now();
        let stale: Vec<SessionId> = self
            .agent_state
            .iter()
            .filter(|e| now.duration_since(e.at) > AGENT_STATE_TTL)
            .map(|e| *e.key())
            .collect();
        for s in stale {
            self.agent_state.remove(&s);
        }
        self.agent_state
            .iter()
            .filter(|e| e.tenant == tenant)
            .map(|e| (*e.key(), e.window, e.state.clone()))
            .collect()
    }

    pub fn instance_id(&self) -> Uuid {
        self.instance_id
    }

    /// Join the cross-instance bus. Idempotent; without it the registry is a
    /// plain single-instance in-memory registry.
    pub fn start_bus(self: &Arc<Self>, pool: PgPool) {
        let (tx, rx) = mpsc::unbounded_channel();
        if self.bus_tx.set(tx).is_ok() {
            bus::start(self.clone(), pool, rx);
        }
    }

    /// The listener calls this once its Postgres `LISTEN` is live.
    pub(crate) fn mark_bus_ready(&self) {
        let _ = self.bus_ready.send(true);
    }

    /// Resolve once this instance's bus listener is actually listening, so a
    /// message published straight after `start_bus` isn't dropped into the void.
    /// Returns immediately if the bus was never started (single-instance mode)
    /// or is already ready.
    /// Wait until the bus is listening, or give up.
    ///
    /// Bounded deliberately. The listener signals readiness from a spawned
    /// task, and a task that *dies* closes the channel and wakes us — but one
    /// that merely stalls, on a connection Postgres never completes, leaves
    /// this waiting forever. That is not hypothetical: it burned two and a
    /// half hours of CI on a single test, which is worse than failing, because
    /// a failure at least says something.
    ///
    /// Returns whether the bus actually became ready, so a caller can decide.
    /// Ten seconds is far longer than a local LISTEN takes and far shorter than
    /// anyone's patience.
    pub async fn bus_ready(&self) -> bool {
        const LIMIT: std::time::Duration = std::time::Duration::from_secs(10);

        if self.bus_tx.get().is_none() || *self.bus_ready.borrow() {
            return *self.bus_ready.borrow();
        }
        let mut rx = self.bus_ready.subscribe();
        tokio::time::timeout(LIMIT, async move {
            while rx.changed().await.is_ok() {
                if *rx.borrow() {
                    return true;
                }
            }
            // The sender dropped: the listener task is gone and readiness will
            // never arrive.
            false
        })
        .await
        .unwrap_or(false)
    }

    fn queue(&self, out: Outbound) {
        if let Some(tx) = self.bus_tx.get() {
            let _ = tx.send(out);
        }
    }

    // ── Nodes ──────────────────────────────────────────────────────────────

    /// Returns a registration epoch; pass it back to `unregister_node` so a
    /// stale connection's cleanup can't remove a fresh registration.
    pub fn register_node(&self, id: NodeId, handle: NodeHandle) -> u64 {
        let epoch = self
            .next_epoch
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.nodes.insert(id, LocalNode { handle, epoch });
        epoch
    }

    pub fn unregister_node(&self, id: NodeId, epoch: u64) {
        self.nodes.remove_if(&id, |_, n| n.epoch == epoch);
    }

    pub fn node_tx(&self, id: NodeId) -> Option<mpsc::Sender<ControlToNode>> {
        self.nodes.get(&id).map(|n| n.handle.tx.clone())
    }

    /// Online anywhere: locally connected, or a fresh ownership lease exists
    /// (whoever holds it).
    pub fn node_online(&self, id: NodeId) -> bool {
        self.nodes.contains_key(&id)
            || self
                .lease_cache
                .get(&id)
                .is_some_and(|e| e.1 > Instant::now())
    }

    /// The instance holding a fresh lease for `id`, when it isn't us.
    fn lease_owner(&self, id: NodeId) -> Option<Uuid> {
        let entry = self.lease_cache.get(&id)?;
        let (owner, expires) = *entry;
        (owner != self.instance_id && expires > Instant::now()).then_some(owner)
    }

    /// Best-effort send to a node; false if the node is offline everywhere or
    /// a channel is full (slow consumer ⇒ drop, never block the plane —
    /// `try_send` locally, unbounded-queue-then-NOTIFY across the bus).
    pub fn send_to_node(&self, id: NodeId, msg: ControlToNode) -> bool {
        if let Some(session) = session_of(&msg) {
            self.session_nodes.insert(session, id);
        }

        // Local fast path — we own the node's socket.
        if let Some(node) = self.nodes.get(&id) {
            if let ControlToNode::DetachSession { session_id } = &msg {
                // Pause the node's output stream only when NO instance has
                // viewers left (the caller only knows about its own).
                if self
                    .remote_viewers
                    .get(session_id)
                    .is_some_and(|s| !s.is_empty())
                {
                    return true;
                }
            }
            return node.handle.tx.try_send(msg).is_ok();
        }

        // Remote path — route to the owning instance over the bus.
        let Some(owner) = self.lease_owner(id) else {
            return false;
        };
        match &msg {
            ControlToNode::AttachSession { session_id, .. } => {
                self.queue(Outbound::Direct {
                    to: owner,
                    msg: BusMessage::Subscribe {
                        session_id: *session_id,
                        instance: self.instance_id,
                    },
                });
            }
            ControlToNode::DetachSession { session_id } => {
                // The owner decides whether the node stream actually pauses.
                self.queue(Outbound::Direct {
                    to: owner,
                    msg: BusMessage::Unsubscribe {
                        session_id: *session_id,
                        instance: self.instance_id,
                    },
                });
                return true;
            }
            _ => {}
        }
        let reply_to = request_kind(&msg).map(|_| self.instance_id);
        self.queue(Outbound::Direct {
            to: owner,
            msg: BusMessage::ToNode {
                node_id: id,
                reply_to,
                msg,
            },
        });
        true
    }

    // ── UI broadcast ───────────────────────────────────────────────────────

    pub fn ui_sender(&self, tenant: TenantId) -> broadcast::Sender<UiEvent> {
        self.ui
            .entry(tenant)
            .or_insert_with(|| broadcast::channel(256).0)
            .clone()
    }

    pub fn publish(&self, tenant: TenantId, event: UiEvent) {
        self.publish_local(tenant, event.clone());
        self.queue(Outbound::Broadcast(BusMessage::UiEvt {
            origin: self.instance_id,
            tenant,
            event,
        }));
    }

    fn publish_local(&self, tenant: TenantId, event: UiEvent) {
        if let Some(tx) = self.ui.get(&tenant) {
            let _ = tx.send(event); // no subscribers is fine
        }
    }

    // ── Git status request/response ────────────────────────────────────────

    /// Ask a node for git status/diff of a checkout. Returns a receiver that
    /// resolves when the node answers (caller applies its own timeout).
    pub fn request_git_status(
        &self,
        node_id: NodeId,
        workspace_path: String,
    ) -> Option<oneshot::Receiver<GitStatusPayload>> {
        let request_id = Uuid::now_v7();
        let (tx, rx) = oneshot::channel();
        self.pending_git.insert(request_id, tx);
        let sent = self.send_to_node(
            node_id,
            ControlToNode::GetGitStatus {
                request_id,
                workspace_path,
            },
        );
        if !sent {
            self.pending_git.remove(&request_id);
            return None;
        }
        Some(rx)
    }

    /// Complete a git status request: resolves locally, or routes the answer
    /// back to the instance that asked.
    pub fn complete_git_status(&self, request_id: Uuid, payload: GitStatusPayload) {
        if let Some((_, tx)) = self.pending_git.remove(&request_id) {
            let _ = tx.send(payload);
            return;
        }
        if let Some((_, requester)) = self.remote_pending_git.remove(&request_id) {
            self.queue(Outbound::Direct {
                to: requester,
                msg: BusMessage::GitReply {
                    request_id,
                    is_repo: payload.is_repo,
                    branch: payload.branch,
                    files: payload.files,
                    diff: payload.diff,
                },
            });
        }
    }

    /// Start a long-running op on a node (clone, worktree). The closure gets
    /// the allocated request id and builds the message to send.
    pub fn request_op(
        &self,
        node_id: NodeId,
        build: impl FnOnce(Uuid) -> ControlToNode,
    ) -> Option<oneshot::Receiver<OpPayload>> {
        let request_id = Uuid::now_v7();
        let (tx, rx) = oneshot::channel();
        self.pending_ops.insert(request_id, tx);
        if !self.send_to_node(node_id, build(request_id)) {
            self.pending_ops.remove(&request_id);
            return None;
        }
        Some(rx)
    }

    /// Complete an op: resolves locally, or routes the answer back to the
    /// instance that asked.
    pub fn complete_op(&self, request_id: Uuid, payload: OpPayload) {
        if let Some((_, tx)) = self.pending_ops.remove(&request_id) {
            let _ = tx.send(payload);
            return;
        }
        if let Some((_, requester)) = self.remote_pending_ops.remove(&request_id) {
            self.queue(Outbound::Direct {
                to: requester,
                msg: BusMessage::OpReply {
                    request_id,
                    ok: payload.ok,
                    path: payload.path,
                    message: payload.message,
                },
            });
        }
    }

    // ── Terminal attachments ───────────────────────────────────────────────

    pub fn attachment_sender(&self, session: SessionId) -> broadcast::Sender<AttachServerMessage> {
        self.attachments
            .entry(session)
            .or_insert_with(|| broadcast::channel(1024).0)
            .clone()
    }

    pub fn publish_session(&self, session: SessionId, msg: AttachServerMessage) {
        if let Some(tx) = self.attachments.get(&session) {
            let _ = tx.send(msg.clone());
        }
        if let Some(instances) = self.remote_viewers.get(&session) {
            for inst in instances.iter() {
                self.queue(Outbound::Direct {
                    to: *inst,
                    msg: BusMessage::SessionFrame {
                        session_id: session,
                        frame: msg.clone(),
                    },
                });
            }
        }
    }

    pub fn drop_attachment(&self, session: SessionId) {
        self.attachments.remove(&session);
    }

    // ── Viewer sizing (the typing viewer — the "driver" — owns the PTY) ────

    pub fn new_viewer_id(&self) -> u64 {
        self.next_viewer
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// Where a session's driver state lives. Unknown routes fall back to
    /// local, which is exactly the single-instance behavior.
    fn viewer_route(&self, session: SessionId) -> Option<Uuid> {
        let node = *self.session_nodes.get(&session)?;
        if self.nodes.contains_key(&node) {
            return None; // ours
        }
        self.lease_owner(node)
    }

    fn key(&self, viewer: u64) -> ViewerKey {
        ViewerKey {
            instance: self.instance_id,
            id: viewer,
        }
    }

    /// A viewer connected. The first viewer becomes the driver by default.
    pub fn viewer_attached(&self, session: SessionId, viewer: u64) {
        if let Some(owner) = self.viewer_route(session) {
            self.queue(Outbound::Direct {
                to: owner,
                msg: BusMessage::Viewer {
                    session_id: session,
                    instance: self.instance_id,
                    viewer,
                    event: ViewerEvent::Attached,
                },
            });
            return;
        }
        self.viewer_attached_key(session, self.key(viewer));
    }

    /// Record a viewer's size. Applied (returned) only when the viewer is the
    /// driver; spectators' sizes are stored for if they later take over. For
    /// remotely-owned sessions the vote is routed to the owner, which applies
    /// it and broadcasts the resulting `Size` — so this returns None.
    pub fn viewer_resize(
        &self,
        session: SessionId,
        viewer: u64,
        cols: u16,
        rows: u16,
    ) -> Option<(u16, u16)> {
        if let Some(owner) = self.viewer_route(session) {
            self.queue(Outbound::Direct {
                to: owner,
                msg: BusMessage::Viewer {
                    session_id: session,
                    instance: self.instance_id,
                    viewer,
                    event: ViewerEvent::Resize { cols, rows },
                },
            });
            return None;
        }
        self.viewer_resize_key(session, self.key(viewer), cols, rows)
    }

    /// A viewer typed: they become the driver. Returns their size when this is
    /// a takeover (so the PTY can adopt it). Remote: routed, owner applies.
    pub fn viewer_input(&self, session: SessionId, viewer: u64) -> Option<(u16, u16)> {
        if let Some(owner) = self.viewer_route(session) {
            self.queue(Outbound::Direct {
                to: owner,
                msg: BusMessage::Viewer {
                    session_id: session,
                    instance: self.instance_id,
                    viewer,
                    event: ViewerEvent::Input,
                },
            });
            return None;
        }
        self.viewer_input_key(session, self.key(viewer))
    }

    /// A viewer left. If the driver left, the most recently active remaining
    /// viewer takes over; returns its size so the PTY can adopt it.
    pub fn viewer_detached(&self, session: SessionId, viewer: u64) -> Option<(u16, u16)> {
        if let Some(owner) = self.viewer_route(session) {
            self.queue(Outbound::Direct {
                to: owner,
                msg: BusMessage::Viewer {
                    session_id: session,
                    instance: self.instance_id,
                    viewer,
                    event: ViewerEvent::Detached,
                },
            });
            return None;
        }
        self.viewer_detached_key(session, self.key(viewer))
    }

    /// The current agreed grid (the driver's last size), for late joiners.
    /// Remote sessions answer None; the owner pushes a `Size` frame on
    /// subscribe instead.
    pub fn current_size(&self, session: SessionId) -> Option<(u16, u16)> {
        let entry = self.viewers.get(&session)?;
        let s = entry.value();
        s.driver
            .and_then(|d| s.viewers.get(&d))
            .and_then(|v| v.size)
    }

    // ── Keyed (owner-side) viewer logic ────────────────────────────────────

    fn viewer_attached_key(&self, session: SessionId, key: ViewerKey) {
        let mut entry = self.viewers.entry(session).or_default();
        let s = entry.value_mut();
        s.viewers.insert(
            key,
            ViewerInfo {
                size: None,
                last_active: Instant::now(),
            },
        );
        if s.driver.is_none() {
            s.driver = Some(key);
        }
    }

    fn viewer_resize_key(
        &self,
        session: SessionId,
        key: ViewerKey,
        cols: u16,
        rows: u16,
    ) -> Option<(u16, u16)> {
        let mut entry = self.viewers.entry(session).or_default();
        let s = entry.value_mut();
        // Upsert: a remote viewer's Attached may not have arrived first.
        s.viewers
            .entry(key)
            .or_insert_with(|| ViewerInfo {
                size: None,
                last_active: Instant::now(),
            })
            .size = Some((cols, rows));
        if s.driver.is_none() {
            s.driver = Some(key);
        }
        (s.driver == Some(key)).then_some((cols, rows))
    }

    fn viewer_input_key(&self, session: SessionId, key: ViewerKey) -> Option<(u16, u16)> {
        let mut entry = self.viewers.get_mut(&session)?;
        let s = entry.value_mut();
        let takeover = if s.driver == Some(key) {
            None
        } else {
            s.driver = Some(key);
            s.viewers.get(&key).and_then(|v| v.size)
        };
        if let Some(v) = s.viewers.get_mut(&key) {
            v.last_active = Instant::now();
        }
        takeover
    }

    fn viewer_detached_key(&self, session: SessionId, key: ViewerKey) -> Option<(u16, u16)> {
        let mut promoted = None;
        let mut empty = false;
        if let Some(mut entry) = self.viewers.get_mut(&session) {
            let s = entry.value_mut();
            s.viewers.remove(&key);
            empty = s.viewers.is_empty();
            if !empty && s.driver == Some(key) {
                let next = s
                    .viewers
                    .iter()
                    .max_by_key(|(_, v)| v.last_active)
                    .map(|(id, _)| *id);
                s.driver = next;
                promoted = next.and_then(|id| s.viewers.get(&id)).and_then(|v| v.size);
            }
        }
        if empty {
            self.viewers.remove(&session);
        }
        promoted
    }

    /// Apply an owner-side driver decision: resize the PTY and tell viewers.
    fn apply_size(&self, session: SessionId, cols: u16, rows: u16) {
        if let Some(node) = self.session_nodes.get(&session).map(|n| *n) {
            self.send_to_node(
                node,
                ControlToNode::ResizeSession {
                    session_id: session,
                    cols,
                    rows,
                },
            );
        }
        self.publish_session(session, AttachServerMessage::Size { cols, rows });
    }

    // ── Bus plumbing (called from bus.rs) ──────────────────────────────────

    /// Refresh the lease mirror from Postgres.
    pub async fn refresh_lease_cache(&self, pool: &PgPool) {
        let rows: Vec<(Uuid, Uuid, f64)> = sqlx::query_as(
            "SELECT id, owning_instance_id,
                    EXTRACT(EPOCH FROM lease_expires_at - now())::float8
             FROM nodes
             WHERE owning_instance_id IS NOT NULL AND lease_expires_at > now()",
        )
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        self.lease_cache.clear();
        let now = Instant::now();
        for (node, owner, ttl) in rows {
            self.lease_cache.insert(
                NodeId(node),
                (
                    owner,
                    now + std::time::Duration::from_secs_f64(ttl.max(0.0)),
                ),
            );
        }
    }

    /// Handle a message delivered by the bus listener.
    pub(crate) fn handle_bus(&self, msg: BusMessage) {
        match msg {
            BusMessage::ToNode {
                node_id,
                reply_to,
                msg,
            } => {
                if let Some(requester) = reply_to.filter(|r| *r != self.instance_id) {
                    match request_kind(&msg) {
                        Some((rid, RequestKind::Op)) => {
                            self.remote_pending_ops.insert(rid, requester);
                        }
                        Some((rid, RequestKind::Git)) => {
                            self.remote_pending_git.insert(rid, requester);
                        }
                        None => {}
                    }
                }
                let delivered = self
                    .node_tx(node_id)
                    .is_some_and(|tx| tx.try_send(msg).is_ok());
                if !delivered {
                    tracing::debug!(%node_id, "bus ToNode for a node we don't hold");
                }
            }
            BusMessage::OpReply {
                request_id,
                ok,
                path,
                message,
            } => self.complete_op(request_id, OpPayload { ok, path, message }),
            BusMessage::GitReply {
                request_id,
                is_repo,
                branch,
                files,
                diff,
            } => self.complete_git_status(
                request_id,
                GitStatusPayload {
                    is_repo,
                    branch,
                    files,
                    diff,
                },
            ),
            BusMessage::SessionFrame { session_id, frame } => {
                if let Some(tx) = self.attachments.get(&session_id) {
                    let _ = tx.send(frame);
                }
            }
            BusMessage::UiEvt {
                origin,
                tenant,
                event,
            } => {
                if origin != self.instance_id {
                    self.publish_local(tenant, event);
                }
            }
            BusMessage::Subscribe {
                session_id,
                instance,
            } => {
                self.remote_viewers
                    .entry(session_id)
                    .or_default()
                    .insert(instance);
                // Late joiner on another instance: hand it the current grid.
                if let Some((cols, rows)) = self.current_size(session_id) {
                    self.queue(Outbound::Direct {
                        to: instance,
                        msg: BusMessage::SessionFrame {
                            session_id,
                            frame: AttachServerMessage::Size { cols, rows },
                        },
                    });
                }
            }
            BusMessage::Unsubscribe {
                session_id,
                instance,
            } => {
                let now_empty = {
                    let Some(mut set) = self.remote_viewers.get_mut(&session_id) else {
                        return;
                    };
                    set.remove(&instance);
                    set.is_empty()
                };
                if now_empty {
                    self.remote_viewers.remove(&session_id);
                    // No remote viewers left; if we also have no local ones,
                    // let the node pause its output stream.
                    let local = self
                        .attachments
                        .get(&session_id)
                        .map(|tx| tx.receiver_count())
                        .unwrap_or(0);
                    if local == 0 {
                        if let Some(node) = self.session_nodes.get(&session_id).map(|n| *n) {
                            self.send_to_node(node, ControlToNode::DetachSession { session_id });
                        }
                    }
                }
            }
            BusMessage::Viewer {
                session_id,
                instance,
                viewer,
                event,
            } => {
                let key = ViewerKey {
                    instance,
                    id: viewer,
                };
                match event {
                    ViewerEvent::Attached => self.viewer_attached_key(session_id, key),
                    ViewerEvent::Resize { cols, rows } => {
                        if let Some((c, r)) = self.viewer_resize_key(session_id, key, cols, rows) {
                            self.apply_size(session_id, c, r);
                        }
                    }
                    ViewerEvent::Input => {
                        if let Some((c, r)) = self.viewer_input_key(session_id, key) {
                            self.apply_size(session_id, c, r);
                        }
                    }
                    ViewerEvent::Detached => {
                        if let Some((c, r)) = self.viewer_detached_key(session_id, key) {
                            self.apply_size(session_id, c, r);
                        }
                    }
                }
            }
        }
    }
}

/// The session a control message concerns, for session→node routing.
fn session_of(msg: &ControlToNode) -> Option<SessionId> {
    match msg {
        ControlToNode::StartSession { session_id, .. }
        | ControlToNode::AttachSession { session_id, .. }
        | ControlToNode::SessionInput { session_id, .. }
        | ControlToNode::ResizeSession { session_id, .. }
        | ControlToNode::KillSession { session_id }
        | ControlToNode::DetachSession { session_id } => Some(*session_id),
        _ => None,
    }
}

enum RequestKind {
    Op,
    Git,
}

/// Request id (and reply family) carried by a control message, if any.
fn request_kind(msg: &ControlToNode) -> Option<(Uuid, RequestKind)> {
    match msg {
        ControlToNode::CloneRepo { request_id, .. }
        | ControlToNode::AddWorktree { request_id, .. }
        | ControlToNode::RemoveWorktree { request_id, .. }
        | ControlToNode::InitProject { request_id, .. }
        | ControlToNode::CaptureSession { request_id, .. } => Some((*request_id, RequestKind::Op)),
        ControlToNode::GetGitStatus { request_id, .. } => Some((*request_id, RequestKind::Git)),
        _ => None,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct ViewerKey {
    instance: Uuid,
    id: u64,
}

#[derive(Default)]
struct SessionViewers {
    viewers: std::collections::HashMap<ViewerKey, ViewerInfo>,
    driver: Option<ViewerKey>,
}

struct ViewerInfo {
    size: Option<(u16, u16)>,
    last_active: Instant,
}

#[cfg(test)]
mod agent_state_tests {
    use super::*;
    use nook_types::{SessionId, TenantId};

    #[test]
    fn set_returns_true_only_on_change() {
        let r = Registry::new();
        let (t, s) = (TenantId::new(), SessionId::new());
        assert!(
            r.set_agent_state(t, s, Some(0), "running"),
            "first set is a change"
        );
        assert!(
            !r.set_agent_state(t, s, Some(0), "running"),
            "a repeat only refreshes the timestamp — no UI churn"
        );
        assert!(
            r.set_agent_state(t, s, Some(0), "waiting"),
            "new state is a change"
        );
        assert!(
            r.set_agent_state(t, s, Some(1), "waiting"),
            "new window is a change"
        );
    }

    #[test]
    fn idle_clears_the_entry() {
        let r = Registry::new();
        let (t, s) = (TenantId::new(), SessionId::new());
        r.set_agent_state(t, s, None, "running");
        assert!(
            r.set_agent_state(t, s, None, "idle"),
            "idle over a live entry is a change"
        );
        assert!(r.agent_states_for(t).is_empty(), "idle leaves no row");
        assert!(
            !r.set_agent_state(t, s, None, "idle"),
            "idle over nothing is not a change"
        );
    }

    #[test]
    fn states_are_scoped_to_their_tenant() {
        let r = Registry::new();
        let (t1, t2) = (TenantId::new(), TenantId::new());
        let (a, b) = (SessionId::new(), SessionId::new());
        r.set_agent_state(t1, a, None, "running");
        r.set_agent_state(t2, b, None, "waiting");
        let one = r.agent_states_for(t1);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].0, a);
        assert_eq!(
            r.agent_states_for(t2).len(),
            1,
            "a tenant never sees another's agents"
        );
    }

    #[test]
    fn a_stale_entry_is_swept_on_read() {
        let r = Registry::new();
        let (t, s) = (TenantId::new(), SessionId::new());
        r.set_agent_state(t, s, None, "running");
        // Backdate past the TTL to simulate an agent that crashed without ever
        // reporting idle.
        if let Some(mut e) = r.agent_state.get_mut(&s) {
            e.at = Instant::now() - AGENT_STATE_TTL - Duration::from_secs(1);
        }
        assert!(
            r.agent_states_for(t).is_empty(),
            "a stale spinner does not survive a read"
        );
    }

    #[test]
    fn clear_forgets_on_death() {
        let r = Registry::new();
        let (t, s) = (TenantId::new(), SessionId::new());
        r.set_agent_state(t, s, None, "waiting");
        assert!(
            r.clear_agent_state(s),
            "clearing a live entry reports it removed"
        );
        assert!(r.agent_states_for(t).is_empty());
        assert!(!r.clear_agent_state(s), "clearing nothing is a no-op");
    }
}
