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
use nook_db::DbPool;
use nook_proto::{AttachServerMessage, ControlToNode, UiEvent};
use nook_types::{GitFileStatus, NodeId, SessionId, TenantId};
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

/// A live tunnel: what one `<label>.<TUNNEL_DOMAIN>` host resolves to.
///
/// The node's NAME is carried rather than looked up, because the pages that
/// need it are the failure ones — "nothing is listening on port 3000 on node
/// `beelink`" — and reaching the database to render a 502 is a query made at
/// the worst possible moment.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Tunnel {
    pub label: String,
    /// Whose tunnel it is. Membership of THIS tenant is the entire access
    /// policy (MAIN-9 NG-6), so it is the field the surface authorises against.
    pub tenant_id: TenantId,
    pub node_id: NodeId,
    pub node_name: String,
    /// The port on the node's own loopback.
    pub port: u16,
    /// The session the tunnel was opened from, when there is one. It is what
    /// ends the tunnel with the terminal that opened it (MAIN-404 AC-4).
    pub session_id: Option<SessionId>,
    /// When it was opened. Wall clock, unlike the idle timer beside it, because
    /// this one is shown to a person and travels to other replicas — neither of
    /// which an `Instant` can do.
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// A tunnel plus the two clocks only the local process can keep.
struct TunnelEntry {
    tunnel: Tunnel,
    /// When a request last came through here. An `Instant`, so it is this
    /// replica's own monotonic reading and no clock skew can make a tunnel look
    /// used in the future.
    last_used: Instant,
    /// When this replica last told its peers the tunnel is in use — `None`
    /// until it has. What keeps a busy tunnel to one NOTIFY per announce
    /// interval instead of one per request; see [`Registry::touch_tunnel`].
    last_announced: Option<Instant>,
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
    /// In-flight tunnel requests issued by THIS replica (MAIN-402 AC-3).
    ///
    /// A sender rather than a `oneshot`, because a tunnel answer is a head
    /// frame plus an unbounded run of chunks and the caller consumes them as
    /// they arrive — buffering the body here would be the memory bug AC-2 is
    /// avoiding on the node.
    ///
    /// In memory only, so a restart drops every tunnel in flight (MAIN-9 NG-8).
    /// That is the intended behaviour and not a gap: a half-streamed response
    /// cannot be resumed from a table, and pretending otherwise would leave a
    /// client waiting on a stream nobody is writing.
    pending_tunnels: DashMap<Uuid, mpsc::Sender<nook_proto::NodeToControl>>,
    /// Tunnel requests relayed here FROM another replica: which one to send the
    /// frames back to. Unlike its single-shot siblings this entry outlives many
    /// frames and is removed on the terminal one.
    remote_pending_tunnels: DashMap<Uuid, Uuid>,
    /// label → what that tunnel host resolves to (MAIN-403). Live state, held
    /// like the rest of it: a restart drops every tunnel (MAIN-9 NG-8).
    ///
    /// REPLICATED, not local (MAIN-404). A tunnel host is served by whichever
    /// replica the load balancer picked, so a route only one replica knows
    /// about is a URL that works about as often as you are lucky — and `list`
    /// would show a different answer per replica. Every put and take is
    /// broadcast; the copies are equal and none of them is an owner, so no
    /// tunnel is stranded by the replica that opened it going away.
    tunnel_routes: DashMap<String, TunnelEntry>,
    /// Serialises label allocation — see [`Registry::open_tunnel_route`]. Held
    /// only across a pure derivation and one insert, never across an await.
    tunnel_open: std::sync::Mutex<()>,
    /// Grant ids already exchanged for a tunnel cookie, so the second exchange
    /// of one fails. See [`Registry::spend_grant`].
    spent_grants: DashMap<Uuid, Instant>,
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
            pending_tunnels: DashMap::new(),
            remote_pending_tunnels: DashMap::new(),
            tunnel_routes: DashMap::new(),
            tunnel_open: std::sync::Mutex::new(()),
            spent_grants: DashMap::new(),
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
    pub fn start_bus(self: &Arc<Self>, pool: DbPool) {
        let (tx, rx) = mpsc::unbounded_channel();
        if self.bus_tx.set(tx).is_ok() {
            bus::start(self.clone(), pool, rx);
        }
    }

    /// The listener calls this once its Postgres `LISTEN` is live.
    ///
    /// `send_replace`, not `send`: the readiness watch is held with no permanent
    /// receiver (`watch::channel(false).0`), so `send` would be a no-op whenever
    /// no caller happens to be awaiting `bus_ready()` at that instant, losing the
    /// signal. `send_replace` updates the stored value unconditionally, so a
    /// later `bus_ready()` reads the truth (MAIN-93 AC-2).
    pub(crate) fn mark_bus_ready(&self) {
        self.bus_ready.send_replace(true);
    }

    /// The listener calls this the moment its connection drops, BEFORE it loops
    /// to reconnect. Without it, `bus_ready()` keeps reporting `true` through the
    /// reconnect window even though no `LISTEN` is live, so a caller proceeds and
    /// its NOTIFY is lost (MAIN-93 AC-2). Readiness is re-signalled only once the
    /// new `LISTEN` completes. `send_replace` for the same reason as above — the
    /// clear must land even when nobody is currently awaiting readiness.
    pub(crate) fn mark_bus_unready(&self) {
        self.bus_ready.send_replace(false);
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

    /// Drop this node's connection — but only if `epoch` is still the live one.
    ///
    /// Returns whether it actually removed anything, i.e. whether the caller's
    /// connection was the current one. `false` means the node already
    /// reconnected and a NEWER connection owns it, so the caller must not go on
    /// to record consequences of "the node went away" (MAIN-363).
    #[must_use]
    pub fn unregister_node(&self, id: NodeId, epoch: u64) -> bool {
        self.nodes.remove_if(&id, |_, n| n.epoch == epoch).is_some()
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

    // ── Tunnels (MAIN-402) ─────────────────────────────────────────────────

    /// Register interest in a tunnel request's frames and hand back the stream.
    ///
    /// Called by whoever is about to send the `TunnelRequest`, BEFORE sending
    /// it — the node can answer faster than the caller can register, and a
    /// frame arriving with no entry is dropped.
    pub fn open_tunnel(&self, request_id: Uuid) -> mpsc::Receiver<nook_proto::NodeToControl> {
        // Bounded: a client that stops reading must slow the stream down rather
        // than let the node fill this replica's memory with chunks nobody
        // wants. 64 is a body's worth of chunks in flight, not a body.
        let (tx, rx) = mpsc::channel(64);
        self.pending_tunnels.insert(request_id, tx);
        rx
    }

    /// Stop tracking a tunnel — the caller gave up, or the exchange finished.
    /// Idempotent, so both the terminal frame and a dropped client can call it.
    pub fn close_tunnel(&self, request_id: Uuid) {
        self.pending_tunnels.remove(&request_id);
        self.remote_pending_tunnels.remove(&request_id);
    }

    /// A frame arrived from a node: hand it to the local waiter, or relay it to
    /// the replica that issued the request (AC-4).
    ///
    /// The terminal frame — a chunk marked `last`, or `TunnelFailed` — also
    /// closes the entry, which is the whole difference from `complete_op`:
    /// there, arriving IS finishing.
    pub fn tunnel_frame(&self, request_id: Uuid, frame: nook_proto::NodeToControl) {
        let terminal = matches!(
            &frame,
            nook_proto::NodeToControl::TunnelFailed { .. }
                | nook_proto::NodeToControl::TunnelChunk { last: true, .. }
        );

        if let Some(entry) = self.pending_tunnels.get(&request_id) {
            // `try_send` rather than `send`: this runs on the node's read loop,
            // and awaiting here would stall every other message from that
            // machine — the failure MAIN-362 spent a card removing.
            let _ = entry.try_send(frame);
        } else if let Some(requester) = self.remote_pending_tunnels.get(&request_id).map(|r| *r) {
            self.queue(Outbound::Direct {
                to: requester,
                msg: BusMessage::TunnelFrame { request_id, frame },
            });
        }

        if terminal {
            self.close_tunnel(request_id);
        }
    }

    // ── Tunnel routes (MAIN-403) ───────────────────────────────────────────

    /// What `<label>.<TUNNEL_DOMAIN>` currently points at, or `None` — which the
    /// surface answers with its 404 page rather than the SPA.
    pub fn tunnel_route(&self, label: &str) -> Option<Tunnel> {
        self.tunnel_routes.get(label).map(|e| e.tunnel.clone())
    }

    /// Publish a tunnel at its label, replacing any tunnel already there, and
    /// tell every other replica.
    pub fn put_tunnel_route(&self, tunnel: Tunnel) {
        self.announce_route(tunnel.label.clone(), Some(Box::new(tunnel.clone())));
        self.put_tunnel_local(tunnel);
    }

    /// Forget a tunnel, here and everywhere. Returns what was there, so a caller
    /// can report what it closed.
    pub fn take_tunnel_route(&self, label: &str) -> Option<Tunnel> {
        let gone = self.take_tunnel_local(label);
        if gone.is_some() {
            self.announce_route(label.to_string(), None);
        }
        gone
    }

    /// Every tunnel a tenant currently has, with how long each has been idle.
    ///
    /// The idle reading is THIS replica's, which is the honest one to report: it
    /// is what this replica's sweep would act on, and peers relay their own use
    /// within an announce interval of serving it.
    pub fn tunnels_for_tenant(&self, tenant: TenantId) -> Vec<(Tunnel, Duration)> {
        let mut out: Vec<(Tunnel, Duration)> = self
            .tunnel_routes
            .iter()
            .filter(|e| e.tunnel.tenant_id == tenant)
            .map(|e| (e.tunnel.clone(), e.last_used.elapsed()))
            .collect();
        // Newest first: the tunnel somebody just opened is the one they are
        // looking for.
        out.sort_by_key(|(t, _)| std::cmp::Reverse(t.created_at));
        out
    }

    /// Claim the first free label from `stem` and publish `build(label)` there,
    /// as one step.
    ///
    /// Not `tunnel_label_taken` then `put_tunnel_route`, because those are two
    /// decisions and a tunnel's whole identity is the label: two callers opening
    /// one at the same moment would both find `api` free and the second would
    /// silently replace the first's route with its own.
    ///
    /// The lock closes that window WITHIN a replica, which is where it is wide
    /// (a person and their agent both running `nook tunnels 3000`). Across
    /// replicas it stays open for as long as the announcement takes, because
    /// the table is broadcast state and not a consensus — the same trade the
    /// rest of this in-memory design makes.
    pub fn open_tunnel_route(&self, stem: &str, build: impl FnOnce(String) -> Tunnel) -> Tunnel {
        let _held = self.tunnel_open.lock().unwrap_or_else(|e| e.into_inner());
        let label = nook_proto::tunnel::unique_subdomain(stem, &|l| {
            // Across ALL tenants: a label is a host in one shared zone, so two
            // tenants cannot both have `api`.
            self.tunnel_routes.contains_key(l)
        });
        let tunnel = build(label);
        self.put_tunnel_route(tunnel.clone());
        tunnel
    }

    /// Record that a request came through, and — no more often than
    /// `announce_after` — tell the other replicas so their idle clocks agree.
    ///
    /// Throttled rather than per-request because the alternative is a NOTIFY
    /// for every HTTP request through every tunnel, which is a traffic
    /// amplifier and not a bookkeeping scheme. The cost is that the window is
    /// only honoured to within `announce_after` across replicas: a tunnel used
    /// on one replica can be swept by another that last heard about it an
    /// announce interval ago. A quarter-window is the caller's choice and keeps
    /// that error well inside the window's own precision.
    pub fn touch_tunnel(&self, label: &str, announce_after: Duration) {
        let mut announce = false;
        if let Some(mut e) = self.tunnel_routes.get_mut(label) {
            let now = Instant::now();
            e.last_used = now;
            // Never announced (the tunnel's first request) always announces:
            // opening one and using it immediately is the common case, and
            // starting the throttle already spent would swallow exactly that.
            if e.last_announced
                .is_none_or(|at| now.duration_since(at) >= announce_after)
            {
                e.last_announced = Some(now);
                announce = true;
            }
        }
        if announce {
            self.queue(Outbound::Broadcast(BusMessage::TunnelUsed {
                origin: self.instance_id,
                label: label.to_string(),
            }));
        }
    }

    /// Every tunnel that has gone unused for `idle`, removed and announced.
    ///
    /// Run on every replica, over its own copies. That is safe precisely
    /// because the removal is broadcast: two replicas deciding at once produce
    /// the same tunnel closed once, not two closures of different things.
    pub fn sweep_idle_tunnels(&self, idle: Duration) -> Vec<Tunnel> {
        let stale: Vec<String> = self
            .tunnel_routes
            .iter()
            .filter(|e| e.last_used.elapsed() >= idle)
            .map(|e| e.key().clone())
            .collect();
        stale
            .into_iter()
            .filter_map(|label| self.take_tunnel_route(&label))
            .collect()
    }

    /// Close every tunnel pointing at a node — it disconnected, so all of them
    /// are 502 factories now (MAIN-404 AC-4).
    pub fn take_tunnels_for_node(&self, node: NodeId) -> Vec<Tunnel> {
        self.take_tunnels_where(|t| t.node_id == node)
    }

    /// Close every tunnel a session opened: the terminal is gone, and so is
    /// whatever was listening on the port it exposed (MAIN-404 AC-4).
    pub fn take_tunnels_for_session(&self, session: SessionId) -> Vec<Tunnel> {
        self.take_tunnels_where(|t| t.session_id == Some(session))
    }

    fn take_tunnels_where(&self, pred: impl Fn(&Tunnel) -> bool) -> Vec<Tunnel> {
        // Collect the labels first: removing while iterating a DashMap is how
        // you deadlock a shard against itself.
        let hits: Vec<String> = self
            .tunnel_routes
            .iter()
            .filter(|e| pred(&e.tunnel))
            .map(|e| e.key().clone())
            .collect();
        hits.into_iter()
            .filter_map(|label| self.take_tunnel_route(&label))
            .collect()
    }

    fn put_tunnel_local(&self, tunnel: Tunnel) {
        self.tunnel_routes.insert(
            tunnel.label.clone(),
            TunnelEntry {
                tunnel,
                last_used: Instant::now(),
                last_announced: None,
            },
        );
    }

    fn take_tunnel_local(&self, label: &str) -> Option<Tunnel> {
        self.tunnel_routes.remove(label).map(|(_, e)| e.tunnel)
    }

    fn announce_route(&self, label: String, tunnel: Option<Box<Tunnel>>) {
        self.queue(Outbound::Broadcast(BusMessage::TunnelRoute {
            origin: self.instance_id,
            label,
            tunnel,
        }));
    }

    /// Redeem a grant id, returning `false` if it has already been used.
    ///
    /// This is what makes the cross-subdomain grant SINGLE-use (MAIN-403 AC-2):
    /// the token itself is signed and short-lived, and this ledger is what stops
    /// a second exchange of the one that just landed in a browser's history, a
    /// referrer header, or a proxy log.
    ///
    /// In memory, so it is per-replica: a grant is minted and redeemed within
    /// seconds by one browser, and the token's own expiry bounds anything the
    /// ledger cannot see. Entries are pruned when the map grows rather than on a
    /// timer — nothing here is worth a background task.
    pub fn spend_grant(&self, jti: Uuid, ttl: Duration) -> bool {
        if self.spent_grants.len() > 4096 {
            self.spent_grants.retain(|_, at| at.elapsed() < ttl);
        }
        self.spent_grants.insert(jti, Instant::now()).is_none()
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

    /// Refresh the lease mirror from the node repository (MAIN-305).
    pub async fn refresh_lease_cache(&self, nodes: &dyn crate::repo::nodes::NodeRepository) {
        let rows = nodes.live_leases().await.unwrap_or_default();
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
                        Some((rid, RequestKind::Tunnel)) => {
                            self.remote_pending_tunnels.insert(rid, requester);
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
            BusMessage::TunnelFrame { request_id, frame } => {
                // Only the three tunnel variants are legal here; anything else
                // is a peer sending something this replica should not act on.
                if matches!(
                    &frame,
                    nook_proto::NodeToControl::TunnelResponse { .. }
                        | nook_proto::NodeToControl::TunnelChunk { .. }
                        | nook_proto::NodeToControl::TunnelFailed { .. }
                ) {
                    self.tunnel_frame(request_id, frame);
                } else {
                    tracing::warn!(%request_id, "bus TunnelFrame carrying a non-tunnel frame");
                }
            }
            BusMessage::TunnelRoute {
                origin,
                label,
                tunnel,
            } => {
                // A broadcast comes back to its sender too. Applying our own
                // would be harmless for a take and wrong for a put: it would
                // reset the idle clock of a tunnel we already hold.
                if origin == self.instance_id {
                    return;
                }
                match tunnel {
                    Some(t) if t.label == label => self.put_tunnel_local(*t),
                    // A route whose payload names a different label than the key
                    // is a peer disagreeing with itself; take the label out
                    // rather than publish something at a name it did not claim.
                    Some(_) => {
                        tracing::warn!(%label, "bus TunnelRoute payload names another label");
                        self.take_tunnel_local(&label);
                    }
                    None => {
                        self.take_tunnel_local(&label);
                    }
                }
            }
            BusMessage::TunnelUsed { origin, label } => {
                if origin == self.instance_id {
                    return;
                }
                if let Some(mut e) = self.tunnel_routes.get_mut(&label) {
                    e.last_used = Instant::now();
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
    /// A stream, not a oneshot: the entry it creates has to survive every chunk
    /// and is removed on the terminal frame instead of on the first reply.
    Tunnel,
}

/// Request id (and reply family) carried by a control message, if any.
fn request_kind(msg: &ControlToNode) -> Option<(Uuid, RequestKind)> {
    match msg {
        ControlToNode::CloneRepo { request_id, .. }
        | ControlToNode::AddWorktree { request_id, .. }
        | ControlToNode::RemoveWorktree { request_id, .. }
        | ControlToNode::ReapBuildStacks { request_id, .. }
        | ControlToNode::InitProject { request_id, .. }
        | ControlToNode::CaptureSession { request_id, .. } => Some((*request_id, RequestKind::Op)),
        ControlToNode::GetGitStatus { request_id, .. } => Some((*request_id, RequestKind::Git)),
        ControlToNode::TunnelRequest { request_id, .. } => Some((*request_id, RequestKind::Tunnel)),
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
mod tunnel_route_tests {
    use super::*;
    use nook_types::{NodeId, SessionId, TenantId};

    fn tunnel(label: &str, tenant: TenantId, node: NodeId, session: Option<SessionId>) -> Tunnel {
        Tunnel {
            label: label.into(),
            tenant_id: tenant,
            node_id: node,
            node_name: "beelink".into(),
            port: 3000,
            session_id: session,
            created_at: chrono::Utc::now(),
        }
    }

    /// The label allocator is the tunnel's identity: two callers opening one at
    /// the same instant must not both get `api`.
    #[test]
    fn a_second_tunnel_at_one_stem_gets_the_next_label() {
        let r = Registry::new();
        let (t, n) = (TenantId::new(), NodeId(Uuid::now_v7()));
        let first = r.open_tunnel_route("api", |label| tunnel(&label, t, n, None));
        let second = r.open_tunnel_route("api", |label| tunnel(&label, t, n, None));
        assert_eq!(first.label, "api");
        assert_eq!(second.label, "api-2", "the first was not replaced");
        assert!(r.tunnel_route("api").is_some());
    }

    /// A label is a host in ONE shared zone, so uniqueness cannot be per-tenant.
    #[test]
    fn another_tenant_cannot_take_a_label_already_in_use() {
        let r = Registry::new();
        let n = NodeId(Uuid::now_v7());
        r.open_tunnel_route("api", |l| tunnel(&l, TenantId::new(), n, None));
        let theirs = r.open_tunnel_route("api", |l| tunnel(&l, TenantId::new(), n, None));
        assert_eq!(theirs.label, "api-2");
    }

    /// AC-4, both halves: a tunnel dies with the session it was opened from and
    /// with the machine it points at — and neither takes anything else with it.
    #[test]
    fn teardown_takes_a_sessions_tunnels_and_a_nodes_tunnels_and_no_others() {
        let r = Registry::new();
        let t = TenantId::new();
        let (node_a, node_b) = (NodeId(Uuid::now_v7()), NodeId(Uuid::now_v7()));
        let session = SessionId::new();
        r.open_tunnel_route("bound", |l| tunnel(&l, t, node_a, Some(session)));
        r.open_tunnel_route("loose", |l| tunnel(&l, t, node_a, None));
        r.open_tunnel_route("elsewhere", |l| tunnel(&l, t, node_b, None));

        let gone = r.take_tunnels_for_session(session);
        assert_eq!(gone.len(), 1);
        assert_eq!(gone[0].label, "bound");
        assert!(
            r.tunnel_route("loose").is_some(),
            "a tunnel with no session outlives one exiting"
        );

        let gone = r.take_tunnels_for_node(node_a);
        assert_eq!(gone.len(), 1, "only the node's own: {gone:?}");
        assert_eq!(gone[0].label, "loose");
        assert!(
            r.tunnel_route("elsewhere").is_some(),
            "another machine's tunnel is untouched"
        );
    }

    /// AC-3: the sweep is measured from the last request through the tunnel,
    /// not from when it was opened.
    #[test]
    fn the_idle_sweep_takes_the_unused_and_spares_the_touched() {
        let r = Registry::new();
        let (t, n) = (TenantId::new(), NodeId(Uuid::now_v7()));
        r.open_tunnel_route("busy", |l| tunnel(&l, t, n, None));
        r.open_tunnel_route("quiet", |l| tunnel(&l, t, n, None));
        // Age both past the window, then use one — which is the whole point:
        // an old tunnel somebody is still using is not an idle one.
        let window = Duration::from_secs(600);
        for label in ["busy", "quiet"] {
            let mut e = r.tunnel_routes.get_mut(label).expect("just inserted");
            e.last_used = Instant::now() - window - Duration::from_secs(1);
        }
        r.touch_tunnel("busy", Duration::from_secs(150));

        let swept = r.sweep_idle_tunnels(window);
        assert_eq!(swept.len(), 1, "{swept:?}");
        assert_eq!(swept[0].label, "quiet");
        assert!(r.tunnel_route("busy").is_some());
        assert!(
            r.sweep_idle_tunnels(window).is_empty(),
            "a second pass finds nothing left to take"
        );
    }

    /// Listing is per tenant and newest-first, because the tunnel somebody just
    /// opened is the one they are looking for.
    #[test]
    fn a_tenant_lists_its_own_tunnels_newest_first() {
        let r = Registry::new();
        let (mine, theirs) = (TenantId::new(), TenantId::new());
        let n = NodeId(Uuid::now_v7());
        let mut older = tunnel("older", mine, n, None);
        older.created_at -= chrono::Duration::minutes(5);
        r.put_tunnel_route(older);
        r.open_tunnel_route("newer", |l| tunnel(&l, mine, n, None));
        r.open_tunnel_route("not-mine", |l| tunnel(&l, theirs, n, None));

        let listed = r.tunnels_for_tenant(mine);
        let labels: Vec<&str> = listed.iter().map(|(t, _)| t.label.as_str()).collect();
        assert_eq!(labels, ["newer", "older"]);
    }
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

    /// The bus-readiness re-arm (MAIN-93 AC-2): readiness is cleared when the
    /// listener drops and only re-signalled once it re-LISTENs, so `bus_ready()`
    /// never reports `true` during a window with no active listener. Exercised
    /// directly over the watch, without a real Postgres LISTEN.
    #[tokio::test]
    async fn bus_readiness_re_arms_across_a_listener_drop() {
        let r = Registry::new();
        assert!(
            !r.bus_ready().await,
            "starts unready before the first LISTEN"
        );
        r.mark_bus_ready();
        assert!(r.bus_ready().await, "ready once LISTEN is live");
        // The listener connection drops → readiness must clear immediately.
        r.mark_bus_unready();
        assert!(
            !r.bus_ready().await,
            "unready during the reconnect window — the hole is closed"
        );
        // The reconnect completes its new LISTEN → ready again.
        r.mark_bus_ready();
        assert!(r.bus_ready().await, "ready again after re-LISTEN");
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
